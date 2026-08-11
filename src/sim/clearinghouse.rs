//! Clearinghouse — the whole forward-hop / adjudication / return-hop pipeline
//! as a request/response call (DESIGN.md 'Virtual time'). No actor task, no spawns, no
//! sleeps: durations are computed onto arrival timestamps, and `transact`
//! executes on the *calling* claim task's thread, so adjudication runs in
//! parallel across worker threads by construction. Transport faults are drawn
//! from transport-family RNG streams (keyed by attempt: retries get fresh
//! fates) and recorded to sim-truth; with a default `FaultProfile` the
//! clearinghouse is honest.

use std::collections::HashMap;
use std::time::Duration;

use rand::Rng;
use tokio::sync::mpsc;

use crate::domain::{Arrival, ClaimId, Delivery, PayerId, Submission, Transaction, Transactor};
use crate::sim::faults::FaultProfile;
use crate::sim::payer::{self, PayerConfig};
use crate::sim::rng::RngFactory;
use crate::sim::sim_truth::{FaultKind, InjectedFault};

pub struct Clearinghouse {
    pub payers: HashMap<PayerId, PayerConfig>,
    pub rng: RngFactory,
    /// Resolved per payer: the payer's override when configured, otherwise
    /// the global profile. Different routes really do fail differently.
    pub faults: HashMap<PayerId, FaultProfile>,
    pub sim_truth: mpsc::UnboundedSender<InjectedFault>,
}

impl Transactor for Clearinghouse {
    /// The sim's complete account of one submission — every delivery it will
    /// ever produce, with computed virtual arrival times. Duplicate delivery
    /// (fault 2.4) shares the attempt number, so the stateless payer
    /// adjudicates both copies into the identical remittance and both copies
    /// share one transport fate — exactly as the spawned-task implementation
    /// behaved, since fates are keyed by (claim_id, attempt, point), never by
    /// copy.
    fn transact(&self, submission: &Submission) -> Transaction {
        let claim = &submission.claim;
        let claim_id = claim.claim_id.clone();
        let attempt = submission.attempt;
        let faults = self.faults[&claim.payer_id];

        // Forward hop: biller → payer. A drop here means the payer never saw
        // the claim (fault 2.1) — pure silence, indistinguishable to the
        // biller from every other kind of loss.
        if chance(
            &self.rng,
            &claim_id,
            attempt,
            "forward/drop",
            faults.forward_drop_rate,
        ) {
            self.record(&claim_id, attempt, FaultKind::ForwardDrop);
            return Transaction {
                arrivals: Vec::new(),
            };
        }

        // Fault 2.4: duplicate delivery.
        let copies = if chance(
            &self.rng,
            &claim_id,
            attempt,
            "forward/dup",
            faults.duplicate_rate,
        ) {
            self.record(&claim_id, attempt, FaultKind::Duplicate);
            2
        } else {
            1
        };

        let cfg = self.payers[&claim.payer_id];
        tracing::debug!(%claim_id, payer_id = %claim.payer_id, attempt, copies, "clearinghouse delivering claim");

        // The return leg is computed once — both duplicate copies draw the
        // same RNG streams, so they share latency, adjudication, and fate.
        // Faults are recorded per copy, matching the per-task recording of
        // the executed implementation.
        let leg = self.return_leg(submission, cfg, faults);
        let mut arrivals = Vec::new();
        for _ in 0..copies {
            for kind in &leg.injected {
                self.record(&claim_id, attempt, *kind);
            }
            if let Some(arrival) = &leg.arrival {
                arrivals.push(arrival.clone());
            }
        }
        Transaction { arrivals }
    }
}

impl Clearinghouse {
    /// Payer adjudication plus the return hop, computed: what (if anything)
    /// comes back from one delivered copy, when, and which faults were
    /// injected along the way.
    fn return_leg(
        &self,
        submission: &Submission,
        cfg: PayerConfig,
        faults: FaultProfile,
    ) -> ReturnLeg {
        let claim = &submission.claim;
        let claim_id = &claim.claim_id;
        let attempt = submission.attempt;
        let rng = &self.rng;
        let mut leg = ReturnLeg::default();

        let latency = payer::latency(&cfg, rng, claim_id);
        let mut remit = payer::adjudicate(claim, &cfg, rng);

        // Fault 3.1: dishonest adjudication. Adjudication-family key — the
        // lie is reproduced identically on re-delivery, like any answer.
        if faults.dishonest_adjudication_rate > 0.0
            && rng
                .adjudication(claim_id, "dishonest")
                .random_bool(faults.dishonest_adjudication_rate)
        {
            payer::apply_dishonesty(&mut remit, rng);
            leg.injected.push(FaultKind::Dishonest);
        }

        // Return hop: payer → biller. The payer DID adjudicate; a drop here
        // (fault 2.2) is indistinguishable from 2.1 to the biller — and the
        // retry it provokes is answered by deterministic re-adjudication.
        if chance(
            rng,
            claim_id,
            attempt,
            "return/drop",
            faults.return_drop_rate,
        ) {
            leg.injected.push(FaultKind::ReturnDrop);
            return leg;
        }

        // Fault 2.3: extra transport delay. Whether this becomes a
        // "timeout" is the biller's policy's business, not ours.
        let mut delay = Duration::ZERO;
        if chance(
            rng,
            claim_id,
            attempt,
            "return/delay",
            faults.extra_delay_rate,
        ) {
            let secs = rng
                .transport(claim_id, attempt, "return/delay_magnitude")
                .random_range(0.0..faults.max_extra_delay_secs.max(f64::MIN_POSITIVE));
            leg.injected.push(FaultKind::ExtraDelay);
            delay = Duration::from_secs_f64(secs);
        }
        let arrive_at = submission.sent_at + latency + delay;

        // Fault 3.3: individual service lines vanish in transit. The
        // remittance still arrives, balanced as far as it goes — the
        // claim stays partially adjudicated and keeps aging.
        if faults.line_drop_rate > 0.0 {
            remit.lines.retain(|line| {
                let point = format!("return/line_drop/{}", line.service_line_id);
                let keep = !chance(rng, claim_id, attempt, &point, faults.line_drop_rate);
                if !keep {
                    leg.injected.push(FaultKind::LineDrop);
                }
                keep
            });
        }

        // Fault 3.5: the whole remittance turns to garbage in transit.
        // Nothing financial survives; sometimes a claim_id fragment does.
        if chance(
            rng,
            claim_id,
            attempt,
            "return/corrupt",
            faults.corrupt_remittance_rate,
        ) {
            leg.injected.push(FaultKind::CorruptRemittance);
            let correlatable = chance(rng, claim_id, attempt, "return/corrupt_readable", 0.5)
                .then(|| claim_id.clone());
            leg.arrival = Some(Arrival {
                at: arrive_at,
                delivery: Delivery::Garbage { correlatable },
            });
            return leg;
        }

        // Fault 3.2: claim_id mangled in transit — the remittance will
        // reach the biller but can never correlate; the real claim hears
        // only silence.
        if chance(
            rng,
            claim_id,
            attempt,
            "return/corrupt_id",
            faults.corrupt_claim_id_rate,
        ) {
            leg.injected.push(FaultKind::CorruptClaimId);
            remit.claim_id = ClaimId(format!("corrupt/{attempt}/{claim_id}"));
        }

        leg.arrival = Some(Arrival {
            at: arrive_at,
            delivery: Delivery::Intact(remit),
        });
        leg
    }

    fn record(&self, claim_id: &ClaimId, attempt: u32, kind: FaultKind) {
        let fault = InjectedFault {
            claim_id: claim_id.clone(),
            attempt,
            kind,
        };
        // Recorder outlives all injectors by construction; unbounded send
        // never blocks, so `transact` stays a plain sync call.
        let _ = self.sim_truth.send(fault);
    }
}

/// What one delivered copy produces: at most one arrival, plus the faults
/// injected while producing it.
#[derive(Default)]
struct ReturnLeg {
    arrival: Option<Arrival>,
    injected: Vec<FaultKind>,
}

/// One transport-fate draw: keyed by (seed, claim_id, attempt, point), so the
/// same run always injects the same faults and retries reroll.
fn chance(rng: &RngFactory, claim_id: &ClaimId, attempt: u32, point: &str, rate: f64) -> bool {
    rate > 0.0 && rng.transport(claim_id, attempt, point).random_bool(rate)
}

//! Clearinghouse actor — the only long-lived simulation component. Routes
//! claims by payer_id, spawns a stateless payer adjudication task per
//! delivery, and forwards remittances back to the biller. Transport faults are
//! injected here on both hops, drawn from transport-family RNG streams (keyed
//! by attempt: retries get fresh fates) and recorded to sim-truth; with a
//! default `FaultProfile` the clearinghouse is honest.

use std::collections::HashMap;

use rand::Rng;
use tokio::sync::mpsc;

use crate::domain::{ClaimId, Delivery, PayerId, Submission, SubmittedClaim};
use crate::sim::faults::FaultProfile;
use crate::sim::payer::{self, PayerConfig};
use crate::sim::rng::RngFactory;
use crate::sim::sim_truth::{FaultKind, InjectedFault};

pub struct Clearinghouse {
    pub payers: HashMap<PayerId, PayerConfig>,
    pub rng: RngFactory,
    pub faults: FaultProfile,
    pub sim_truth: mpsc::Sender<InjectedFault>,
}

impl Clearinghouse {
    pub async fn run(
        self,
        mut submissions: mpsc::Receiver<Submission>,
        remits_out: mpsc::Sender<Delivery>,
    ) {
        let (return_tx, mut return_rx) = mpsc::channel::<Delivery>(64);

        loop {
            tokio::select! {
                submission = submissions.recv() => match submission {
                    Some(submission) => self.deliver(submission, return_tx.clone()).await,
                    None => break, // biller side done submitting
                },
                Some(delivery) = return_rx.recv() => forward(&remits_out, delivery).await,
            }
        }

        // Drain in-flight payer tasks; the channel closes once the last payer
        // task drops its return sender.
        drop(return_tx);
        while let Some(delivery) = return_rx.recv().await {
            forward(&remits_out, delivery).await;
        }
    }

    /// Forward hop: biller → payer. A drop here means the payer never saw the
    /// claim (fault 2.1) — pure silence, indistinguishable to the biller from
    /// every other kind of loss.
    async fn deliver(&self, submission: Submission, return_tx: mpsc::Sender<Delivery>) {
        let claim_id = submission.claim.claim_id.clone();
        let attempt = submission.attempt;

        if chance(
            &self.rng,
            &claim_id,
            attempt,
            "forward/drop",
            self.faults.forward_drop_rate,
        ) {
            self.record(claim_id, attempt, FaultKind::ForwardDrop).await;
            return;
        }

        // Fault 2.4: duplicate delivery. Both copies share the attempt number,
        // so the stateless payer adjudicates both into the identical
        // remittance — the biller must apply at most one.
        let copies = if chance(
            &self.rng,
            &claim_id,
            attempt,
            "forward/dup",
            self.faults.duplicate_rate,
        ) {
            self.record(claim_id.clone(), attempt, FaultKind::Duplicate)
                .await;
            2
        } else {
            1
        };

        let payer_id = submission.claim.payer_id;
        let cfg = self.payers[&payer_id];
        tracing::debug!(%claim_id, %payer_id, attempt, copies, "clearinghouse delivering claim");
        for _ in 0..copies {
            self.spawn_payer(submission.claim.clone(), attempt, cfg, return_tx.clone());
        }
    }

    /// One stateless payer adjudication task per delivery.
    fn spawn_payer(
        &self,
        claim: SubmittedClaim,
        attempt: u32,
        cfg: PayerConfig,
        return_tx: mpsc::Sender<Delivery>,
    ) {
        let rng = self.rng;
        let faults = self.faults;
        let sim_truth = self.sim_truth.clone();
        tokio::spawn(async move {
            tokio::time::sleep(payer::latency(&cfg, &rng, &claim.claim_id)).await;
            let mut remit = payer::adjudicate(&claim, &cfg, &rng);

            // Fault 3.1: dishonest adjudication. Adjudication-family key — the
            // lie is reproduced identically on re-delivery, like any answer.
            if faults.dishonest_adjudication_rate > 0.0
                && rng
                    .adjudication(&claim.claim_id, "dishonest")
                    .random_bool(faults.dishonest_adjudication_rate)
            {
                payer::apply_dishonesty(&mut remit, &rng);
                record_fault(&sim_truth, &claim.claim_id, attempt, FaultKind::Dishonest).await;
            }

            // Return hop: payer → biller. The payer DID adjudicate; a drop here
            // (fault 2.2) is indistinguishable from 2.1 to the biller — and the
            // retry it provokes is answered by deterministic re-adjudication.
            if chance(
                &rng,
                &claim.claim_id,
                attempt,
                "return/drop",
                faults.return_drop_rate,
            ) {
                record_fault(&sim_truth, &claim.claim_id, attempt, FaultKind::ReturnDrop).await;
                return;
            }

            // Fault 2.3: extra transport delay. Whether this becomes a
            // "timeout" is the biller's policy's business, not ours.
            if chance(
                &rng,
                &claim.claim_id,
                attempt,
                "return/delay",
                faults.extra_delay_rate,
            ) {
                let secs = rng
                    .transport(&claim.claim_id, attempt, "return/delay_magnitude")
                    .random_range(0.0..faults.max_extra_delay_secs.max(f64::MIN_POSITIVE));
                record_fault(&sim_truth, &claim.claim_id, attempt, FaultKind::ExtraDelay).await;
                tokio::time::sleep(std::time::Duration::from_secs_f64(secs)).await;
            }

            // Fault 3.3: individual service lines vanish in transit. The
            // remittance still arrives, balanced as far as it goes — the
            // claim stays partially adjudicated and keeps aging.
            if faults.line_drop_rate > 0.0 {
                let mut dropped = 0;
                let claim_id = &claim.claim_id;
                remit.lines.retain(|line| {
                    let point = format!("return/line_drop/{}", line.service_line_id);
                    let keep = !chance(&rng, claim_id, attempt, &point, faults.line_drop_rate);
                    dropped += usize::from(!keep);
                    keep
                });
                for _ in 0..dropped {
                    record_fault(&sim_truth, claim_id, attempt, FaultKind::LineDrop).await;
                }
            }

            // Fault 3.5: the whole remittance turns to garbage in transit.
            // Nothing financial survives; sometimes a claim_id fragment does.
            if chance(
                &rng,
                &claim.claim_id,
                attempt,
                "return/corrupt",
                faults.corrupt_remittance_rate,
            ) {
                record_fault(
                    &sim_truth,
                    &claim.claim_id,
                    attempt,
                    FaultKind::CorruptRemittance,
                )
                .await;
                let correlatable = chance(
                    &rng,
                    &claim.claim_id,
                    attempt,
                    "return/corrupt_readable",
                    0.5,
                )
                .then(|| claim.claim_id.clone());
                let _ = return_tx.send(Delivery::Garbage { correlatable }).await;
                return;
            }

            // Fault 3.2: claim_id mangled in transit — the remittance will
            // reach the biller but can never correlate; the real claim hears
            // only silence.
            if chance(
                &rng,
                &claim.claim_id,
                attempt,
                "return/corrupt_id",
                faults.corrupt_claim_id_rate,
            ) {
                record_fault(
                    &sim_truth,
                    &claim.claim_id,
                    attempt,
                    FaultKind::CorruptClaimId,
                )
                .await;
                remit.claim_id = ClaimId(format!("corrupt/{attempt}/{}", claim.claim_id));
            }

            // Send failure means the clearinghouse itself is gone — shutdown.
            let _ = return_tx.send(Delivery::Intact(remit)).await;
        });
    }

    async fn record(&self, claim_id: ClaimId, attempt: u32, kind: FaultKind) {
        record_fault(&self.sim_truth, &claim_id, attempt, kind).await;
    }
}

async fn record_fault(
    sim_truth: &mpsc::Sender<InjectedFault>,
    claim_id: &ClaimId,
    attempt: u32,
    kind: FaultKind,
) {
    let fault = InjectedFault {
        claim_id: claim_id.clone(),
        attempt,
        kind,
    };
    // Recorder outlives all injectors by construction.
    let _ = sim_truth.send(fault).await;
}

async fn forward(remits_out: &mpsc::Sender<Delivery>, delivery: Delivery) {
    let _ = remits_out.send(delivery).await;
}

/// One transport-fate draw: keyed by (seed, claim_id, attempt, point), so the
/// same run always injects the same faults and retries reroll.
fn chance(rng: &RngFactory, claim_id: &ClaimId, attempt: u32, point: &str, rate: f64) -> bool {
    rate > 0.0 && rng.transport(claim_id, attempt, point).random_bool(rate)
}

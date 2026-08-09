//! Clearinghouse actor — the only long-lived simulation component. Routes
//! claims by payer_id, spawns a stateless payer adjudication task per
//! delivery, and forwards remittances back to the biller. Transport faults are
//! injected here on both hops, drawn from transport-family RNG streams (keyed
//! by attempt: retries get fresh fates) and recorded to sim-truth; with a
//! default `FaultProfile` the clearinghouse is honest.

use std::collections::HashMap;

use rand::Rng;
use tokio::sync::mpsc;

use crate::domain::{ClaimId, PayerId, RemittanceAdvice, Submission};
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
        remits_out: mpsc::Sender<RemittanceAdvice>,
    ) {
        let (return_tx, mut return_rx) = mpsc::channel::<RemittanceAdvice>(64);

        loop {
            tokio::select! {
                submission = submissions.recv() => match submission {
                    Some(submission) => self.deliver(submission, return_tx.clone()).await,
                    None => break, // biller side done submitting
                },
                Some(remit) = return_rx.recv() => forward(&remits_out, remit).await,
            }
        }

        // Drain in-flight payer tasks; the channel closes once the last payer
        // task drops its return sender.
        drop(return_tx);
        while let Some(remit) = return_rx.recv().await {
            forward(&remits_out, remit).await;
        }
    }

    /// Forward hop: biller → payer. A drop here means the payer never saw the
    /// claim (fault 2.1) — pure silence, indistinguishable to the biller from
    /// every other kind of loss.
    async fn deliver(&self, submission: Submission, return_tx: mpsc::Sender<RemittanceAdvice>) {
        let claim_id = submission.claim.claim_id.clone();
        let attempt = submission.attempt;

        if self.chance(
            &claim_id,
            attempt,
            "forward/drop",
            self.faults.forward_drop_rate,
        ) {
            self.record(claim_id, attempt, FaultKind::ForwardDrop).await;
            return;
        }

        let payer_id = submission.claim.payer_id;
        let cfg = self.payers[&payer_id];
        let rng = self.rng;
        tracing::debug!(%claim_id, %payer_id, attempt, "clearinghouse delivering claim");
        tokio::spawn(async move {
            let claim = submission.claim;
            tokio::time::sleep(payer::latency(&cfg, &rng, &claim.claim_id)).await;
            let remit = payer::adjudicate(&claim, &cfg, &rng);
            // Send failure means the clearinghouse itself is gone — shutdown.
            let _ = return_tx.send(remit).await;
        });
    }

    /// One transport-fate draw: keyed by (seed, claim_id, attempt, point), so
    /// the same run always injects the same faults and retries reroll.
    fn chance(&self, claim_id: &ClaimId, attempt: u32, point: &str, rate: f64) -> bool {
        rate > 0.0
            && self
                .rng
                .transport(claim_id, attempt, point)
                .random_bool(rate)
    }

    async fn record(&self, claim_id: ClaimId, attempt: u32, kind: FaultKind) {
        let fault = InjectedFault {
            claim_id,
            attempt,
            kind,
        };
        // Recorder outlives all injectors by construction.
        let _ = self.sim_truth.send(fault).await;
    }
}

async fn forward(remits_out: &mpsc::Sender<RemittanceAdvice>, remit: RemittanceAdvice) {
    let _ = remits_out.send(remit).await;
}

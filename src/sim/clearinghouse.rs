//! Clearinghouse actor — the only long-lived simulation component. Routes
//! claims by payer_id, spawns a stateless payer adjudication task per
//! delivery, and forwards remittances back to the biller. Transport-fault
//! injection (drop/duplicate/delay/corrupt, both directions) hooks in here as
//! the fault-table commits land; with a default `FaultProfile` it is honest.

use std::collections::HashMap;

use tokio::sync::mpsc;

use crate::domain::{PayerId, RemittanceAdvice, Submission};
use crate::sim::faults::FaultProfile;
use crate::sim::payer::{self, PayerConfig};
use crate::sim::rng::RngFactory;

pub struct Clearinghouse {
    pub payers: HashMap<PayerId, PayerConfig>,
    pub rng: RngFactory,
    #[allow(dead_code)] // read starting with the class-2 fault commits
    pub faults: FaultProfile,
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
                    Some(submission) => self.deliver(submission, return_tx.clone()),
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

    fn deliver(&self, submission: Submission, return_tx: mpsc::Sender<RemittanceAdvice>) {
        let payer_id = submission.claim.payer_id;
        let cfg = self.payers[&payer_id];
        let rng = self.rng;
        tracing::debug!(claim_id = %submission.claim.claim_id, %payer_id,
            attempt = submission.attempt, "clearinghouse delivering claim");
        tokio::spawn(async move {
            let claim = submission.claim;
            tokio::time::sleep(payer::latency(&cfg, &rng, &claim.claim_id)).await;
            let remit = payer::adjudicate(&claim, &cfg, &rng);
            // Send failure means the clearinghouse itself is gone — shutdown.
            let _ = return_tx.send(remit).await;
        });
    }
}

async fn forward(remits_out: &mpsc::Sender<RemittanceAdvice>, remit: RemittanceAdvice) {
    let _ = remits_out.send(remit).await;
}

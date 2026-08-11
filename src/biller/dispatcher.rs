//! Quarantine clerk — what remains of the remittance dispatcher under
//! computed time (DESIGN.md 'Virtual time'). Correlation-by-ID moved into the claim
//! task (each task matches payloads against its own claim_id), so the
//! routing map, registration protocol, and retirement set are gone. What
//! reaches the clerk is exactly what could never correlate: remittances
//! addressed to claim_ids nobody owns (fault 3.2) and garbage without a
//! readable fragment (fault 3.5). Its job is to make sure even the wreckage
//! lands on the books.

use tokio::sync::mpsc;

use crate::domain::{Arrival, ClaimId, Delivery};
use crate::ledger::events::{ClaimEvent, LedgerTx};

/// Single consumer of the stray-delivery channel; returns when every claim
/// task (and the ingest loop) has dropped its sender.
pub async fn run_quarantine(mut strays: mpsc::Receiver<Arrival>, ledger: LedgerTx) {
    while let Some(Arrival { at, delivery }) = strays.recv().await {
        let claim_id = match delivery {
            Delivery::Intact(remit) => {
                tracing::debug!(claim_id = %remit.claim_id, "remittance for unknown claim quarantined");
                remit.claim_id
            }
            Delivery::Garbage {
                correlatable: Some(claim_id),
            } => claim_id,
            Delivery::Garbage { correlatable: None } => ClaimId("<uncorrelatable-garbage>".into()),
        };
        ledger
            .emit(at, claim_id, ClaimEvent::RemittanceQuarantined)
            .await;
    }
}

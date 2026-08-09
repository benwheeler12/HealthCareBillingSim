//! Remittance dispatcher: correlation-by-ID as a real, testable mechanism.
//! Holds the routing map `claim_id → sender`; unknown claim_ids are
//! quarantined (fault 3.2), remittances for deregistered claims become
//! late-remittance ledger events.

use std::collections::{HashMap, HashSet};

use tokio::sync::{mpsc, oneshot};

use crate::domain::{ClaimId, Delivery, RemittanceAdvice};
use crate::ledger::events::{ClaimEvent, LedgerTx};

pub enum DispatcherControl {
    Register {
        claim_id: ClaimId,
        remit_tx: mpsc::Sender<RemittanceAdvice>,
        /// Claim tasks wait for this ack before submitting, so a remittance can
        /// never race its own registration across the two dispatcher channels.
        ack: oneshot::Sender<()>,
    },
    Deregister {
        claim_id: ClaimId,
    },
}

pub async fn run_dispatcher(
    mut control: mpsc::Receiver<DispatcherControl>,
    mut remits: mpsc::Receiver<Delivery>,
    ledger: LedgerTx,
) {
    let mut routes: HashMap<ClaimId, mpsc::Sender<RemittanceAdvice>> = HashMap::new();
    let mut retired: HashSet<ClaimId> = HashSet::new();
    let mut control_open = true;
    let mut remits_open = true;

    while control_open || remits_open {
        tokio::select! {
            msg = control.recv(), if control_open => match msg {
                Some(DispatcherControl::Register { claim_id, remit_tx, ack }) => {
                    routes.insert(claim_id, remit_tx);
                    let _ = ack.send(());
                }
                Some(DispatcherControl::Deregister { claim_id }) => {
                    routes.remove(&claim_id);
                    retired.insert(claim_id);
                }
                None => control_open = false,
            },
            msg = remits.recv(), if remits_open => match msg {
                Some(delivery) => match delivery {
                    Delivery::Intact(remit) => {
                        route(&mut routes, &mut retired, &ledger, remit).await
                    }
                    Delivery::Garbage { correlatable } => {
                        garbage(&routes, &retired, &ledger, correlatable).await
                    }
                },
                None => remits_open = false,
            },
        }
    }
}

async fn route(
    routes: &mut HashMap<ClaimId, mpsc::Sender<RemittanceAdvice>>,
    retired: &mut HashSet<ClaimId>,
    ledger: &LedgerTx,
    remit: RemittanceAdvice,
) {
    let claim_id = remit.claim_id.clone();
    if let Some(tx) = routes.get(&claim_id) {
        // Receiver gone: the claim task went terminal between our lookup and
        // its Deregister being processed. Same meaning as a late remittance.
        let mpsc::error::SendError(remit) = match tx.send(remit).await {
            Ok(()) => return,
            Err(err) => err,
        };
        routes.remove(&claim_id);
        retired.insert(claim_id.clone());
        ledger
            .emit(claim_id, ClaimEvent::LateRemittance { remit })
            .await;
        return;
    }
    if retired.contains(&claim_id) {
        ledger
            .emit(claim_id, ClaimEvent::LateRemittance { remit })
            .await;
        return;
    }
    tracing::warn!(%claim_id, "remittance for unknown claim quarantined");
    ledger
        .emit(claim_id, ClaimEvent::RemittanceQuarantined)
        .await;
}

/// Fault 3.5: unparseable garbage. Epistemically silence — no financial
/// content, so the timeout machinery owns recovery. If a claim_id fragment
/// survived and we know the claim, leave a "garbage received" note in its
/// history; otherwise quarantine the wreckage.
async fn garbage(
    routes: &HashMap<ClaimId, mpsc::Sender<RemittanceAdvice>>,
    retired: &HashSet<ClaimId>,
    ledger: &LedgerTx,
    correlatable: Option<ClaimId>,
) {
    match correlatable {
        Some(claim_id) if routes.contains_key(&claim_id) || retired.contains(&claim_id) => {
            tracing::warn!(%claim_id, "garbage remittance received; treating as silence");
            ledger.emit(claim_id, ClaimEvent::GarbageRemittance).await;
        }
        Some(claim_id) => {
            ledger
                .emit(claim_id, ClaimEvent::RemittanceQuarantined)
                .await;
        }
        None => {
            let claim_id = ClaimId("<uncorrelatable-garbage>".into());
            ledger
                .emit(claim_id, ClaimEvent::RemittanceQuarantined)
                .await;
        }
    }
}

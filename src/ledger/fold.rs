//! Fold-as-you-go with retained event log (Decisions #10): live state for
//! runtime monitoring, raw log for replay and audit. `apply` is pure state
//! transition — the async fn is only the channel shell.

use tokio::sync::mpsc;

use crate::ledger::events::{ClaimEvent, StampedEvent};
use crate::ledger::records::{ClaimRecord, ClaimState, Ledger};

/// Single consumer of the ledger channel. Returns the final ledger when every
/// sender has been dropped — which, by shutdown design, means every claim has
/// reached a terminal state.
pub async fn run_fold(mut rx: mpsc::Receiver<StampedEvent>) -> Ledger {
    let mut ledger = Ledger::default();
    while let Some(event) = rx.recv().await {
        ledger.apply(event);
    }
    ledger
}

impl Ledger {
    pub fn apply(&mut self, stamped: StampedEvent) {
        self.event_log.push(stamped.clone());
        match &stamped.event {
            ClaimEvent::Ingested { identity, lines } => {
                let record = ClaimRecord {
                    claim_id: stamped.claim_id.clone(),
                    identity: Some(identity.clone()),
                    state: ClaimState::Pending,
                    attempts: 0,
                    ingested_at: stamped.at,
                    first_submitted_at: None,
                    resolved_at: None,
                    lines: lines.clone(),
                    history: vec![stamped.clone()],
                };
                self.claims.insert(stamped.claim_id.clone(), record);
            }
            ClaimEvent::Rejected { reason } => {
                let record = ClaimRecord {
                    claim_id: stamped.claim_id.clone(),
                    identity: None,
                    state: ClaimState::Rejected { reason: reason.clone() },
                    attempts: 0,
                    ingested_at: stamped.at,
                    first_submitted_at: None,
                    resolved_at: Some(stamped.at),
                    lines: Vec::new(),
                    history: vec![stamped.clone()],
                };
                self.claims.insert(stamped.claim_id.clone(), record);
            }
            ClaimEvent::RemittanceQuarantined => self.quarantine.push(stamped.clone()),
            event => self.apply_to_existing(&stamped, event),
        }
    }

    fn apply_to_existing(&mut self, stamped: &StampedEvent, event: &ClaimEvent) {
        let Some(record) = self.claims.get_mut(&stamped.claim_id) else {
            // Events for unknown claims can't happen in correct wiring; keep the
            // fold total rather than panicking the system of record.
            debug_assert!(false, "event for unknown claim {}", stamped.claim_id);
            return;
        };
        record.history.push(stamped.clone());
        match event {
            ClaimEvent::Submitted { attempt, timeout_at } => {
                record.attempts = *attempt;
                record.first_submitted_at.get_or_insert(stamped.at);
                record.state = ClaimState::AwaitingResponse { timeout_at: *timeout_at };
            }
            ClaimEvent::RemittanceApplied { lines } => {
                for (service_line_id, adjudication) in lines {
                    let line = record
                        .lines
                        .iter_mut()
                        .find(|l| &l.service_line_id == service_line_id);
                    if let Some(line) = line {
                        line.adjudication = Some(adjudication.clone());
                    }
                }
            }
            ClaimEvent::Resolved => {
                record.state = ClaimState::Resolved;
                record.resolved_at = Some(stamped.at);
            }
            ClaimEvent::Flagged { reason } => {
                record.state = ClaimState::Flagged { reason: reason.clone() };
                record.resolved_at = Some(stamped.at);
            }
            ClaimEvent::LateRemittance => {} // history entry only
            ClaimEvent::Ingested { .. }
            | ClaimEvent::Rejected { .. }
            | ClaimEvent::RemittanceQuarantined => unreachable!("handled in apply"),
        }
    }
}

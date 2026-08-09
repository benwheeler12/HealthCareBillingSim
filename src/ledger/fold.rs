//! Fold-as-you-go with retained event log (Decisions #10): live state for
//! runtime monitoring, raw log for replay and audit. `apply` is pure state
//! transition — the async fn is only the channel shell.

use tokio::sync::{mpsc, watch};

use crate::domain::{RemittanceAdvice, VirtualTime};
use crate::ledger::events::{ClaimEvent, StampedEvent};
use crate::ledger::records::{
    Adjudication, ClaimRecord, ClaimState, FlagReason, Ledger, LineRecord,
};

/// Live counters published by the fold for best-effort observers (the CLI
/// progress line). This is the "broadcast tap" from the design: the
/// authoritative path stays the lossless mpsc; watchers may lag or miss
/// intermediate values and nothing cares.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    pub claims: usize,
    pub resolved: usize,
    pub rejected: usize,
    pub flagged: usize,
    pub now: VirtualTime,
}

/// Single consumer of the ledger channel. Returns the final ledger when every
/// sender has been dropped — which, by shutdown design, means every claim has
/// reached a terminal state. When a watch sender is supplied, counters are
/// published every few events (and once at the end).
pub async fn run_fold(
    mut rx: mpsc::Receiver<StampedEvent>,
    progress: Option<watch::Sender<Progress>>,
) -> Ledger {
    let mut ledger = Ledger::default();
    let mut events: usize = 0;
    while let Some(event) = rx.recv().await {
        let at = event.at;
        ledger.apply(event);
        events += 1;
        if let Some(tx) = &progress
            && events.is_multiple_of(32)
        {
            let _ = tx.send(ledger.progress(at));
        }
    }
    if let Some(tx) = &progress {
        let last = ledger.event_log.last().map(|e| e.at).unwrap_or_default();
        let _ = tx.send(ledger.progress(last));
    }
    ledger
}

impl Ledger {
    /// Cheap counter snapshot — O(claims), called on a sampled cadence.
    fn progress(&self, now: VirtualTime) -> Progress {
        let mut p = Progress {
            claims: self.claims.len(),
            now,
            ..Progress::default()
        };
        for record in self.claims.values() {
            match record.state {
                ClaimState::Resolved => p.resolved += 1,
                ClaimState::Rejected { .. } => p.rejected += 1,
                ClaimState::Flagged { .. } => p.flagged += 1,
                _ => {}
            }
        }
        p
    }
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
                    state: ClaimState::Rejected {
                        reason: reason.clone(),
                    },
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
            ClaimEvent::Submitted {
                attempt,
                timeout_at,
            } => {
                record.attempts = *attempt;
                record.first_submitted_at.get_or_insert(stamped.at);
                record.state = ClaimState::AwaitingResponse {
                    timeout_at: *timeout_at,
                };
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
                record.state = ClaimState::Flagged {
                    reason: reason.clone(),
                };
                record.resolved_at = Some(stamped.at);
            }
            ClaimEvent::LateRemittance { remit } => apply_late_remittance(record, remit, stamped),
            ClaimEvent::DuplicateIngest { .. } => {} // history entry only; first doc wins
            ClaimEvent::GarbageRemittance => {}      // history entry only; silence owns it
            ClaimEvent::Ingested { .. }
            | ClaimEvent::Rejected { .. }
            | ClaimEvent::RemittanceQuarantined => unreachable!("handled in apply"),
        }
    }
}

/// Late-arrival policy (Decisions #5/#6). On a Resolved claim the duplicate is
/// ignored — the history entry (already pushed) is the logged idempotency. On
/// Flagged(RetriesExhausted), a complete and exactly-balanced late answer
/// transitions the claim to Resolved; anything less stays flagged. This lives
/// in the fold because the claim task is gone by definition — the transition
/// is pure record surgery, not lifecycle logic.
fn apply_late_remittance(
    record: &mut ClaimRecord,
    remit: &RemittanceAdvice,
    stamped: &StampedEvent,
) {
    let exhausted = ClaimState::Flagged {
        reason: FlagReason::RetriesExhausted,
    };
    if record.state != exhausted || !complete_and_balanced(&record.lines, remit) {
        return;
    }
    for rline in &remit.lines {
        let line = record
            .lines
            .iter_mut()
            .find(|l| l.service_line_id == rline.service_line_id)
            .expect("checked by complete_and_balanced");
        line.adjudication = Some(Adjudication {
            payer_paid: rline.payer_paid,
            coinsurance: rline.coinsurance,
            copay: rline.copay,
            deductible: rline.deductible,
            not_allowed: rline.not_allowed,
            denial_reason: rline.denial_reason,
            adjudicated_at: stamped.at,
        });
    }
    tracing::debug!(claim_id = %record.claim_id, "late remittance resolved a flagged claim");
    record.state = ClaimState::Resolved;
    record.resolved_at = Some(stamped.at);
}

/// Exact-answer check against the record's billable lines: every billable line
/// covered exactly once, every line's money balancing to the cent.
fn complete_and_balanced(lines: &[LineRecord], remit: &RemittanceAdvice) -> bool {
    let billable: Vec<&LineRecord> = lines.iter().filter(|l| !l.do_not_bill).collect();
    if remit.lines.len() != billable.len() {
        return false;
    }
    let mut seen = std::collections::HashSet::new();
    remit.lines.iter().all(|rline| {
        seen.insert(&rline.service_line_id)
            && billable.iter().any(|l| {
                l.service_line_id == rline.service_line_id && l.billed() == rline.accounted()
            })
    })
}

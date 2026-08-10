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

/// Both views of the books: final (every claim terminal — the correctness
/// guarantee) and frozen at the moment intake ended. A/R reports are a
/// mid-flight snapshot by nature — driving everything terminal first would
/// empty every young bucket by construction — so aging/chase report over the
/// snapshot while the terminal guarantee is asserted on the final ledger.
pub struct FoldOutput {
    pub ledger: Ledger,
    pub intake_snapshot: Ledger,
}

/// Single consumer of the ledger channel. Returns the final ledger when every
/// sender has been dropped — which, by shutdown design, means every claim has
/// reached a terminal state. When a watch sender is supplied, counters are
/// published every few events (and once at the end).
///
/// Under computed time (Decisions #23) the merged stream is NOT virtual-time
/// sorted — parallel claim tasks emit in wall completion order, so a claim
/// finished at virtual day 200 can land before another's day-10 event.
/// Per-claim causal order still holds (each claim emits through one FIFO
/// sender), which is all `apply` needs; global ordering and the intake
/// snapshot are restored in [`finalize`]. Progress `now` is therefore a
/// frontier — the max timestamp seen — not a clock.
pub async fn run_fold(
    mut rx: mpsc::Receiver<StampedEvent>,
    progress: Option<watch::Sender<Progress>>,
) -> Ledger {
    let mut ledger = Ledger::default();
    let mut frontier = VirtualTime::default();
    let mut events: usize = 0;
    while let Some(event) = rx.recv().await {
        frontier = frontier.max(event.at);
        ledger.apply(event);
        events += 1;
        // `% 32` rather than `is_multiple_of` (rustc 1.87+), and a match
        // rather than a let-chain (rustc 1.88+): the repo builds on 1.85.
        #[allow(clippy::manual_is_multiple_of)]
        match &progress {
            Some(tx) if events % 32 == 0 => {
                let _ = tx.send(ledger.progress(frontier));
            }
            _ => {}
        }
    }
    if let Some(tx) = &progress {
        let _ = tx.send(ledger.progress(frontier));
    }
    ledger
}

/// Restore virtual-time order and take the intake-end snapshot, the
/// event-sourcing way — in parallel, since both halves decompose:
///
/// - The log sort is an index argsort (rayon): key (at, claim_id, original
///   index) is a total order, so an unstable parallel sort reproduces the
///   stable sort exactly — original index preserves each claim's causal
///   order across identical timestamps — followed by one permutation pass
///   that moves each fat event once.
/// - The snapshot is a per-claim replay of that claim's own history up to
///   the mark. Claims are independent (the property the whole experiment
///   rests on), so the rebuild is embarrassingly parallel. Events AT the
///   mark are the final intake instant's own (the last Ingested rows land
///   exactly there) and belong inside.
pub fn finalize(mut ledger: Ledger, intake_mark: VirtualTime) -> FoldOutput {
    use rayon::prelude::*;

    let log = std::mem::take(&mut ledger.event_log);
    let mut order: Vec<u32> = (0..log.len() as u32).collect();
    order.par_sort_unstable_by(|&a, &b| {
        let (ea, eb) = (&log[a as usize], &log[b as usize]);
        (ea.at, &ea.claim_id.0, a).cmp(&(eb.at, &eb.claim_id.0, b))
    });
    let mut slots: Vec<Option<StampedEvent>> = log.into_iter().map(Some).collect();
    ledger.event_log = order
        .into_iter()
        .map(|i| {
            slots[i as usize]
                .take()
                .expect("permutation is a bijection")
        })
        .collect();

    let claims = ledger
        .claims
        .par_iter()
        .map(|(claim_id, record)| {
            // Replay this claim's history through the same `apply` the live
            // fold uses — correctness by construction, one claim at a time.
            let mut mini = Ledger::default();
            for event in record.history.iter().filter(|e| e.at <= intake_mark) {
                mini.apply(event.clone());
            }
            let rebuilt = mini
                .claims
                .remove(claim_id)
                .expect("every claim is ingested at or before the intake mark");
            (claim_id.clone(), rebuilt)
        })
        .collect::<Vec<_>>()
        .into_iter()
        .collect();
    let intake_snapshot = Ledger {
        claims,
        quarantine: ledger
            .quarantine
            .iter()
            .filter(|e| e.at <= intake_mark)
            .cloned()
            .collect(),
        event_log: Vec::new(),
    };
    FoldOutput {
        ledger,
        intake_snapshot,
    }
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

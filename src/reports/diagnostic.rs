//! God's-eye diagnostic: the biller's view vs the simulation's ground truth.
//! The gap between the two is the biller's blind spot — and the correctness
//! oracle in tests.
//!
//! Deliberate deviation from "reports depend only on ledger" (Decisions #16):
//! this report's entire point is comparing the two views, so it alone may
//! import sim-truth. The protected invariant — sim-truth never leaking INTO
//! the ledger — is untouched.

use std::fmt;

use crate::ledger::records::{ClaimState, FlagReason, Ledger};
use crate::sim::sim_truth::{FaultKind, SimTruth};

pub struct Diagnostic {
    // Biller's-knowledge view.
    pub retried_claims: usize,
    pub flagged_retries_exhausted: usize,
    pub flagged_reconciliation: usize,
    pub flagged_malformed: usize,
    pub quarantined: usize,
    pub late_resolved: usize,
    // Ground truth.
    pub truth: Vec<(FaultKind, usize)>,
}

pub fn diagnostic(ledger: &Ledger, sim_truth: &SimTruth) -> Diagnostic {
    let mut d = Diagnostic {
        retried_claims: 0,
        flagged_retries_exhausted: 0,
        flagged_reconciliation: 0,
        flagged_malformed: 0,
        quarantined: ledger.quarantine.len(),
        late_resolved: 0,
        truth: Vec::new(),
    };
    for record in ledger.claims.values() {
        d.retried_claims += usize::from(record.attempts > 1);
        match &record.state {
            ClaimState::Flagged { reason } => match reason {
                FlagReason::RetriesExhausted => d.flagged_retries_exhausted += 1,
                FlagReason::ReconciliationFailed { .. } => d.flagged_reconciliation += 1,
                FlagReason::MalformedRemittance => d.flagged_malformed += 1,
            },
            ClaimState::Resolved => {
                let was_flagged = record.history.iter().any(|e| {
                    matches!(
                        e.event,
                        crate::ledger::events::ClaimEvent::Flagged {
                            reason: FlagReason::RetriesExhausted
                        }
                    )
                });
                d.late_resolved += usize::from(was_flagged);
            }
            _ => {}
        }
    }

    for kind in [
        FaultKind::ForwardDrop,
        FaultKind::ReturnDrop,
        FaultKind::ExtraDelay,
        FaultKind::Duplicate,
        FaultKind::Dishonest,
        FaultKind::CorruptClaimId,
        FaultKind::LineDrop,
        FaultKind::CorruptRemittance,
    ] {
        let count = sim_truth.count(kind);
        if count > 0 {
            d.truth.push((kind, count));
        }
    }
    d
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "=== Sim-truth diagnostic (god's-eye view) ===")?;
        writeln!(f, "  biller's view:")?;
        writeln!(
            f,
            "    claims that needed retries:      {:>6}",
            self.retried_claims
        )?;
        writeln!(
            f,
            "    flagged, retries exhausted:      {:>6}",
            self.flagged_retries_exhausted
        )?;
        writeln!(
            f,
            "    flagged, reconciliation failed:  {:>6}",
            self.flagged_reconciliation
        )?;
        writeln!(
            f,
            "    flagged, malformed remittance:   {:>6}",
            self.flagged_malformed
        )?;
        writeln!(
            f,
            "    resolved late, after flagging:   {:>6}",
            self.late_resolved
        )?;
        writeln!(
            f,
            "    quarantined remittances:         {:>6}",
            self.quarantined
        )?;
        writeln!(f, "  what actually happened (invisible to the biller):")?;
        if self.truth.is_empty() {
            writeln!(f, "    no faults injected — honest run")?;
        }
        for (kind, count) in &self.truth {
            writeln!(f, "    {:<32} {:>6}", format!("{kind:?}"), count)?;
        }
        Ok(())
    }
}

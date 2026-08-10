//! End-of-run summary — the vertical slice's "trivial report". The full
//! report suite (AR aging, payer scorecard, chase list, …) lands in build
//! step 5.

use std::fmt;

use crate::domain::Money;
use crate::ledger::records::{ClaimState, Ledger};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub total_claims: usize,
    pub resolved: usize,
    pub rejected: usize,
    pub flagged: usize,
    pub non_terminal: usize,
    pub quarantined_remittances: usize,
    // Money aggregates over billable lines only (Decisions #9).
    pub billed: Money,
    pub payer_paid: Money,
    pub patient_responsibility: Money,
    pub not_allowed: Money,
}

pub fn summarize(ledger: &Ledger) -> Summary {
    summarize_for(ledger, None)
}

/// The same summary, restricted to one billing organization when `provider`
/// is given. Rejected-at-parse rows carry no identity, so they (and the
/// quarantine count) only appear in the whole-portfolio view.
pub fn summarize_for(ledger: &Ledger, provider: Option<&str>) -> Summary {
    let mut s = Summary::default();
    if provider.is_none() {
        s.quarantined_remittances = ledger.quarantine.len();
    }
    for record in ledger.claims.values() {
        if provider.is_some_and(|p| {
            record
                .identity
                .as_ref()
                .is_none_or(|id| id.organization_name != p)
        }) {
            continue;
        }
        s.total_claims += 1;
        match &record.state {
            ClaimState::Resolved => s.resolved += 1,
            ClaimState::Rejected { .. } => s.rejected += 1,
            ClaimState::Flagged { .. } => s.flagged += 1,
            ClaimState::Pending | ClaimState::AwaitingResponse { .. } => s.non_terminal += 1,
        }
        for line in record.lines.iter().filter(|l| !l.do_not_bill) {
            s.billed += line.billed();
            if let Some(adj) = &line.adjudication {
                s.payer_paid += adj.payer_paid;
                s.patient_responsibility += adj.patient_responsibility();
                s.not_allowed += adj.not_allowed;
            }
        }
    }
    s
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "=== Run summary ===")?;
        writeln!(f, "claims ingested:        {:>8}", self.total_claims)?;
        writeln!(f, "  resolved:             {:>8}", self.resolved)?;
        writeln!(f, "  rejected at ingest:   {:>8}", self.rejected)?;
        writeln!(f, "  flagged for review:   {:>8}", self.flagged)?;
        writeln!(f, "  non-terminal (BUG!):  {:>8}", self.non_terminal)?;
        writeln!(
            f,
            "quarantined remits:     {:>8}",
            self.quarantined_remittances
        )?;
        writeln!(f, "billed:                 {:>14}", self.billed.to_string())?;
        writeln!(
            f,
            "  payer paid:           {:>14}",
            self.payer_paid.to_string()
        )?;
        writeln!(
            f,
            "  patient responsibility:{:>13}",
            self.patient_responsibility.to_string()
        )?;
        write!(
            f,
            "  not allowed:          {:>14}",
            self.not_allowed.to_string()
        )
    }
}

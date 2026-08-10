//! Outstanding receivables (the chase list): where a biller's morning
//! starts. Default order (outstanding desc, age desc) — the most money
//! stuck the longest, first; the interactive provider-insights view re-sorts
//! by cost, age, or risk in either direction.

use std::fmt;
use std::time::Duration;

use crate::domain::{ClaimId, Money, PayerId, VirtualTime};
use crate::ledger::records::{ClaimRecord, ClaimState, FlagReason, Ledger};

pub struct ChaseItem {
    pub claim_id: ClaimId,
    pub payer_id: PayerId,
    /// Billing organization, from the claim's identity.
    pub provider: String,
    pub outstanding: Money,
    pub age: Duration,
    pub attempts: u32,
    pub status: String,
}

impl ChaseItem {
    /// Follow-up priority: dollars stuck × days stuck. Deliberately simple —
    /// a $10k claim aged 3 days and a $300 claim aged 100 days both lose to
    /// a $10k claim aged 100 days.
    pub fn risk(&self) -> f64 {
        (self.outstanding.cents() as f64 / 100.0) * (self.age.as_secs_f64() / 86_400.0)
    }
}

pub fn chase_list(ledger: &Ledger, now: VirtualTime, limit: usize) -> Vec<ChaseItem> {
    let mut items: Vec<ChaseItem> = ledger
        .claims
        .values()
        .filter(|r| r.payer_outstanding() > Money::ZERO)
        .filter_map(|r| {
            let identity = r.identity.as_ref()?;
            Some(ChaseItem {
                claim_id: r.claim_id.clone(),
                payer_id: identity.payer_id,
                provider: identity.organization_name.clone(),
                outstanding: r.payer_outstanding(),
                age: r.age(now).unwrap_or_default(),
                attempts: r.attempts,
                status: status_label(r),
            })
        })
        .collect();
    items.sort_by(|a, b| {
        b.outstanding
            .cmp(&a.outstanding)
            .then(b.age.cmp(&a.age))
            .then(a.claim_id.cmp(&b.claim_id))
    });
    items.truncate(limit);
    items
}

/// Plain-English status, derived from the whole record — the state alone
/// says "awaiting response", but the record's attempt count and answered
/// lines tell the operator *which kind* of waiting this is.
pub fn status_label(record: &ClaimRecord) -> String {
    let billable = record.lines.iter().filter(|l| !l.do_not_bill).count();
    let answered = record
        .lines
        .iter()
        .filter(|l| !l.do_not_bill && l.adjudication.is_some())
        .count();
    match &record.state {
        ClaimState::Pending => "pending submission".into(),
        ClaimState::AwaitingResponse { .. } if answered > 0 => {
            format!("partial remit ({answered}/{billable} lines)")
        }
        ClaimState::AwaitingResponse { .. } if record.attempts > 1 => {
            format!("retrying (attempt {})", record.attempts)
        }
        ClaimState::AwaitingResponse { .. } => "awaiting 1st response".into(),
        ClaimState::Resolved => "resolved".into(),
        ClaimState::Rejected { .. } => "rejected at ingest".into(),
        ClaimState::Flagged { reason } => format!("flagged: {}", flag_label(reason)),
    }
}

/// The flag's cause in operator language, with the money delta where the
/// reason carries one.
pub fn flag_label(reason: &FlagReason) -> String {
    match reason {
        FlagReason::RetriesExhausted => "no response, gave up".into(),
        FlagReason::ReconciliationFailed { billed, accounted } if accounted < billed => {
            format!("doesn't sum (short {})", *billed - *accounted)
        }
        FlagReason::ReconciliationFailed { billed, accounted } => {
            format!("doesn't sum (over {})", *accounted - *billed)
        }
        FlagReason::MalformedRemittance => "unusable remittance".into(),
    }
}

pub struct ChaseList(pub Vec<ChaseItem>);

impl fmt::Display for ChaseList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "=== Outstanding receivables (outstanding desc, age desc) ==="
        )?;
        if self.0.is_empty() {
            return writeln!(f, "  nothing to chase — all receivables booked");
        }
        writeln!(
            f,
            "  {:<12} {:<22} {:<26} {:>12} {:>9} {:>10} {:>4}  status",
            "claim", "payer", "provider", "outstanding", "age", "risk", "att"
        )?;
        for item in &self.0 {
            writeln!(
                f,
                "  {:<12} {:<22} {:<26} {:>12} {:>8.1}d {:>10.0} {:>4}  {}",
                item.claim_id.to_string(),
                item.payer_id.as_str(),
                item.provider,
                item.outstanding.to_string(),
                item.age.as_secs_f64() / 86_400.0,
                item.risk(),
                item.attempts,
                item.status,
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::records::{Adjudication, LineRecord};

    fn line(answered: bool) -> LineRecord {
        LineRecord {
            service_line_id: "L1".into(),
            procedure_code: "99213".into(),
            units: 1,
            unit_charge: Money::from_cents(1000),
            do_not_bill: false,
            adjudication: answered.then(|| Adjudication {
                payer_paid: Money::from_cents(1000),
                coinsurance: Money::ZERO,
                copay: Money::ZERO,
                deductible: Money::ZERO,
                not_allowed: Money::ZERO,
                denial_reason: None,
                adjudicated_at: VirtualTime::default(),
            }),
        }
    }

    fn record(state: ClaimState, attempts: u32, lines: Vec<LineRecord>) -> ClaimRecord {
        ClaimRecord {
            claim_id: ClaimId("c-1".into()),
            identity: None,
            state,
            attempts,
            ingested_at: VirtualTime::default(),
            first_submitted_at: None,
            resolved_at: None,
            lines,
            history: Vec::new(),
        }
    }

    fn awaiting() -> ClaimState {
        ClaimState::AwaitingResponse {
            timeout_at: VirtualTime::default(),
        }
    }

    #[test]
    fn awaiting_splits_into_first_wait_retrying_and_partial() {
        let first = record(awaiting(), 1, vec![line(false)]);
        assert_eq!(status_label(&first), "awaiting 1st response");

        let retrying = record(awaiting(), 3, vec![line(false)]);
        assert_eq!(status_label(&retrying), "retrying (attempt 3)");

        // Partially answered wins over the retry label: the evidence of a
        // half-arrived remittance is the more specific story.
        let partial = record(awaiting(), 2, vec![line(true), line(false)]);
        assert_eq!(status_label(&partial), "partial remit (1/2 lines)");
    }

    #[test]
    fn flags_render_cause_and_money_delta() {
        let short = record(
            ClaimState::Flagged {
                reason: FlagReason::ReconciliationFailed {
                    billed: Money::from_cents(266_206),
                    accounted: Money::from_cents(266_123),
                },
            },
            1,
            vec![line(true)],
        );
        assert_eq!(status_label(&short), "flagged: doesn't sum (short $0.83)");

        let exhausted = record(
            ClaimState::Flagged {
                reason: FlagReason::RetriesExhausted,
            },
            3,
            vec![line(false)],
        );
        assert_eq!(status_label(&exhausted), "flagged: no response, gave up");
    }
}

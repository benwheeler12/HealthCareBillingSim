//! Chase list: where a biller's morning starts. Open receivables sorted by
//! (outstanding desc, age desc) — the most money stuck the longest, first.

use std::fmt;
use std::time::Duration;

use crate::domain::{ClaimId, Money, PayerId, VirtualTime};
use crate::ledger::records::{ClaimState, Ledger};

pub struct ChaseItem {
    pub claim_id: ClaimId,
    pub payer_id: PayerId,
    pub outstanding: Money,
    pub age: Duration,
    pub attempts: u32,
    pub status: String,
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
                outstanding: r.payer_outstanding(),
                age: r.age(now).unwrap_or_default(),
                attempts: r.attempts,
                status: status_label(&r.state),
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

fn status_label(state: &ClaimState) -> String {
    match state {
        ClaimState::Pending => "pending".into(),
        ClaimState::AwaitingResponse { .. } => "awaiting response".into(),
        ClaimState::Resolved => "resolved".into(),
        ClaimState::Rejected { .. } => "rejected".into(),
        ClaimState::Flagged { reason } => format!("flagged: {reason:?}"),
    }
}

pub struct ChaseList(pub Vec<ChaseItem>);

impl fmt::Display for ChaseList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "=== Chase list (outstanding desc, age desc) ===")?;
        if self.0.is_empty() {
            return writeln!(f, "  nothing to chase — all receivables booked");
        }
        writeln!(
            f,
            "  {:<12} {:<20} {:>12} {:>10} {:>4}  status",
            "claim", "payer", "outstanding", "age", "att"
        )?;
        for item in &self.0 {
            writeln!(
                f,
                "  {:<12} {:<20} {:>12} {:>9.1}d {:>4}  {}",
                item.claim_id.to_string(),
                item.payer_id.as_str(),
                item.outstanding.to_string(),
                item.age.as_secs_f64() / 86_400.0,
                item.attempts,
                item.status,
            )?;
        }
        Ok(())
    }
}

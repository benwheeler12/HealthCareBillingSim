//! Outstanding receivables (the chase list): where a biller's morning
//! starts. Default order (outstanding desc, age desc) — the most money
//! stuck the longest, first; the interactive provider-insights view re-sorts
//! by cost, age, or risk in either direction.

use std::fmt;
use std::time::Duration;

use crate::domain::{ClaimId, Money, PayerId, VirtualTime};
use crate::ledger::records::{ClaimState, Ledger};

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

//! AR aging — the external projection of the biller's state machine: open
//! receivables ranked by how long they've been waiting. Payer A/R ages from
//! **first** submission (retries never refresh it); patient A/R ages from
//! `adjudicated_at`, the moment it became patient debt. Pure functions of a
//! snapshot + now.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use crate::domain::{Money, PayerId, VirtualTime};
use crate::ledger::records::Ledger;

const VIRTUAL_DAY: Duration = Duration::from_secs(86_400);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AgingBuckets {
    pub d0_30: Money,
    pub d31_60: Money,
    pub d61_90: Money,
    pub d90_plus: Money,
}

impl AgingBuckets {
    fn add(&mut self, age: Duration, amount: Money) {
        let days = age.as_secs_f64() / VIRTUAL_DAY.as_secs_f64();
        let bucket = match days {
            d if d <= 30.0 => &mut self.d0_30,
            d if d <= 60.0 => &mut self.d31_60,
            d if d <= 90.0 => &mut self.d61_90,
            _ => &mut self.d90_plus,
        };
        *bucket += amount;
    }

    pub fn total(&self) -> Money {
        self.d0_30 + self.d31_60 + self.d61_90 + self.d90_plus
    }
}

#[derive(Debug, Default)]
pub struct ArAging {
    pub payer: BTreeMap<PayerId, AgingBuckets>,
    pub patient: BTreeMap<PayerId, AgingBuckets>,
}

pub fn ar_aging(ledger: &Ledger, now: VirtualTime) -> ArAging {
    ar_aging_for(ledger, now, None)
}

/// The same aging report, restricted to one billing organization when
/// `provider` is given — the interactive UI's per-provider drill-down.
pub fn ar_aging_for(ledger: &Ledger, now: VirtualTime, provider: Option<&str>) -> ArAging {
    let mut report = ArAging::default();
    for record in ledger.claims.values() {
        let Some(identity) = &record.identity else {
            continue; // rejected-at-parse rows carry no payer
        };
        if provider.is_some_and(|p| identity.organization_name != p) {
            continue;
        }
        let outstanding = record.payer_outstanding();
        // No let-chain here: the repo builds on rustc 1.85 (edition 2024
        // minimum), which predates stabilized let-chains.
        match record.age(now) {
            Some(age) if outstanding > Money::ZERO => {
                report
                    .payer
                    .entry(identity.payer_id)
                    .or_default()
                    .add(age, outstanding);
            }
            _ => {}
        }
        for line in record.lines.iter().filter(|l| l.is_booked()) {
            let adj = line.adjudication.as_ref().expect("booked implies answered");
            let responsibility = adj.patient_responsibility();
            if responsibility > Money::ZERO {
                let age = now.saturating_since(adj.adjudicated_at);
                report
                    .patient
                    .entry(identity.payer_id)
                    .or_default()
                    .add(age, responsibility);
            }
        }
    }
    report
}

/// Days in A/R headline: total payer outstanding / (total billed / elapsed
/// virtual days). The single number a practice owner watches.
pub fn days_in_ar(ledger: &Ledger, now: VirtualTime) -> f64 {
    days_in_ar_for(ledger, now, None)
}

/// Days in A/R over one billing organization's book when `provider` is
/// given; the whole portfolio otherwise.
pub fn days_in_ar_for(ledger: &Ledger, now: VirtualTime, provider: Option<&str>) -> f64 {
    let records = ledger.claims.values().filter(|r| {
        provider.is_none()
            || r.identity
                .as_ref()
                .zip(provider)
                .is_some_and(|(id, p)| id.organization_name == p)
    });
    let mut outstanding = Money::ZERO;
    let mut billed = Money::ZERO;
    for record in records {
        outstanding += record.payer_outstanding();
        billed += record
            .lines
            .iter()
            .filter(|l| !l.do_not_bill)
            .map(|l| l.billed())
            .sum::<Money>();
    }
    let elapsed_days = now.as_duration().as_secs_f64() / VIRTUAL_DAY.as_secs_f64();
    if billed == Money::ZERO || elapsed_days == 0.0 {
        return 0.0;
    }
    outstanding.cents() as f64 / (billed.cents() as f64 / elapsed_days)
}

impl fmt::Display for ArAging {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        aging_table(
            f,
            "=== AR aging by payer (ages from first submission) ===",
            &self.payer,
        )?;
        writeln!(f)?;
        aging_table(
            f,
            "=== Patient responsibility aging (ages from adjudication) ===",
            &self.patient,
        )
    }
}

fn aging_table(
    f: &mut fmt::Formatter<'_>,
    title: &str,
    rows: &BTreeMap<PayerId, AgingBuckets>,
) -> fmt::Result {
    writeln!(f, "{title}")?;
    writeln!(
        f,
        "  {:<22} {:>12} {:>12} {:>12} {:>12} {:>12}",
        "payer", "0-30d", "31-60d", "61-90d", "90+d", "total"
    )?;
    let mut grand = AgingBuckets::default();
    for (payer, b) in rows {
        aging_row(f, payer.as_str(), b)?;
        grand.d0_30 += b.d0_30;
        grand.d31_60 += b.d31_60;
        grand.d61_90 += b.d61_90;
        grand.d90_plus += b.d90_plus;
    }
    aging_row(f, "TOTAL", &grand)
}

fn aging_row(f: &mut fmt::Formatter<'_>, label: &str, b: &AgingBuckets) -> fmt::Result {
    // Empty buckets print as a dash so the eye lands on the money.
    let cell = |m: Money| {
        if m == Money::ZERO {
            "–".to_string()
        } else {
            m.to_string()
        }
    };
    writeln!(
        f,
        "  {:<22} {:>12} {:>12} {:>12} {:>12} {:>12}",
        label,
        cell(b.d0_30),
        cell(b.d31_60),
        cell(b.d61_90),
        cell(b.d90_plus),
        b.total().to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_split_on_virtual_day_boundaries() {
        let mut b = AgingBuckets::default();
        b.add(VIRTUAL_DAY * 5, Money::from_cents(100));
        b.add(VIRTUAL_DAY * 45, Money::from_cents(200));
        b.add(VIRTUAL_DAY * 75, Money::from_cents(400));
        b.add(VIRTUAL_DAY * 400, Money::from_cents(800));
        assert_eq!(b.d0_30, Money::from_cents(100));
        assert_eq!(b.d31_60, Money::from_cents(200));
        assert_eq!(b.d61_90, Money::from_cents(400));
        assert_eq!(b.d90_plus, Money::from_cents(800));
        assert_eq!(b.total(), Money::from_cents(1500));
    }
}

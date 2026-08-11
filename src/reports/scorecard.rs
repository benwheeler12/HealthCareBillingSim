//! Payer scorecard and denial breakdown — derived purely from remittance
//! data in the ledger. The closed-loop demo: configure a payer slow and
//! denial-happy, and this report rediscovers it without ever seeing the
//! simulation's config.

use std::collections::BTreeMap;
use std::fmt;

use crate::domain::{DenialReason, Money, PayerId};

/// Compact virtual-duration for the table ("12.4 days", "3.2 hours", "45s").
fn human_response(secs: f64) -> String {
    match secs {
        s if s >= 86_400.0 => format!("{:.1} days", s / 86_400.0),
        s if s >= 3_600.0 => format!("{:.1} hours", s / 3_600.0),
        s if s >= 60.0 => format!("{:.1} min", s / 60.0),
        s => format!("{s:.1}s"),
    }
}
use crate::ledger::records::Ledger;

#[derive(Debug, Default, Clone, Copy)]
pub struct PayerScore {
    pub claims: usize,
    /// Mean virtual seconds from first submission to first booked answer —
    /// the response experience as the biller lived it, retries included.
    /// (Stored in seconds; displayed in human virtual units.)
    pub avg_response_secs: f64,
    /// Denied lines / adjudicated lines.
    pub denial_rate: f64,
    /// Payer paid / billed, over booked lines.
    pub paid_to_billed: f64,
}

pub fn payer_scorecard(ledger: &Ledger) -> BTreeMap<PayerId, PayerScore> {
    struct Acc {
        claims: usize,
        response_sum: f64,
        responses: usize,
        lines: usize,
        denied: usize,
        billed: Money,
        paid: Money,
    }
    let mut acc: BTreeMap<PayerId, Acc> = BTreeMap::new();

    for record in ledger.claims.values() {
        let Some(identity) = &record.identity else {
            continue;
        };
        let a = acc.entry(identity.payer_id).or_insert(Acc {
            claims: 0,
            response_sum: 0.0,
            responses: 0,
            lines: 0,
            denied: 0,
            billed: Money::ZERO,
            paid: Money::ZERO,
        });
        a.claims += 1;

        let first_answer = record
            .lines
            .iter()
            .filter_map(|l| l.adjudication.as_ref())
            .map(|adj| adj.adjudicated_at)
            .min();
        if let (Some(submitted), Some(answered)) = (record.first_submitted_at, first_answer) {
            a.response_sum += answered.saturating_since(submitted).as_secs_f64();
            a.responses += 1;
        }
        for line in record.lines.iter().filter(|l| !l.do_not_bill) {
            let Some(adj) = &line.adjudication else {
                continue;
            };
            a.lines += 1;
            a.denied += usize::from(adj.denial_reason.is_some());
            a.billed += line.billed();
            a.paid += adj.payer_paid;
        }
    }

    acc.into_iter()
        .map(|(payer, a)| {
            let score = PayerScore {
                claims: a.claims,
                avg_response_secs: if a.responses > 0 {
                    a.response_sum / a.responses as f64
                } else {
                    0.0
                },
                denial_rate: if a.lines > 0 {
                    a.denied as f64 / a.lines as f64
                } else {
                    0.0
                },
                paid_to_billed: if a.billed > Money::ZERO {
                    a.paid.cents() as f64 / a.billed.cents() as f64
                } else {
                    0.0
                },
            };
            (payer, score)
        })
        .collect()
}

/// Denied dollars and line counts by (payer, reason).
pub fn denial_breakdown(ledger: &Ledger) -> BTreeMap<(PayerId, DenialReason), (usize, Money)> {
    let mut breakdown: BTreeMap<(PayerId, DenialReason), (usize, Money)> = BTreeMap::new();
    for record in ledger.claims.values() {
        let Some(identity) = &record.identity else {
            continue;
        };
        for line in record.lines.iter().filter(|l| !l.do_not_bill) {
            let Some(reason) = line.adjudication.as_ref().and_then(|a| a.denial_reason) else {
                continue;
            };
            let entry = breakdown
                .entry((identity.payer_id, reason))
                .or_insert((0, Money::ZERO));
            entry.0 += 1;
            entry.1 += line.billed();
        }
    }
    breakdown
}

pub struct Scorecard(pub BTreeMap<PayerId, PayerScore>);

impl fmt::Display for Scorecard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "=== Payer scorecard (from remittance data alone) ===")?;
        writeln!(
            f,
            "  {:<22} {:>7} {:>16} {:>12} {:>14}",
            "payer", "claims", "avg response", "denial rate", "paid/billed"
        )?;
        for (payer, s) in &self.0 {
            writeln!(
                f,
                "  {:<22} {:>7} {:>16} {:>11.1}% {:>13.1}%",
                payer.as_str(),
                s.claims,
                human_response(s.avg_response_secs),
                s.denial_rate * 100.0,
                s.paid_to_billed * 100.0,
            )?;
        }
        Ok(())
    }
}

pub struct Denials(pub BTreeMap<(PayerId, DenialReason), (usize, Money)>);

impl fmt::Display for Denials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "=== Denial breakdown by (payer, reason) ===")?;
        if self.0.is_empty() {
            return writeln!(f, "  none");
        }
        // The full breakdown grows with the input; print the rows that carry
        // the money and roll the tail up into one line.
        const SHOWN: usize = 12;
        let mut rows: Vec<_> = self.0.iter().collect();
        rows.sort_by(|a, b| b.1.1.cmp(&a.1.1));
        writeln!(
            f,
            "  {:<22} {:<22} {:>7} {:>14}",
            "payer", "reason", "lines", "denied billed"
        )?;
        for ((payer, reason), (count, money)) in rows.iter().take(SHOWN) {
            writeln!(
                f,
                "  {:<22} {:<22} {:>7} {:>14}",
                payer.as_str(),
                reason.to_string(),
                count,
                money.to_string(),
            )?;
        }
        if rows.len() > SHOWN {
            let (lines, money) = rows
                .iter()
                .skip(SHOWN)
                .fold((0usize, Money::ZERO), |(lines, money), (_, (count, m))| {
                    (lines + count, money + *m)
                });
            writeln!(
                f,
                "  … and {} more (payer, reason) pairs · {} lines · {}",
                rows.len() - SHOWN,
                lines,
                money
            )?;
        }
        Ok(())
    }
}

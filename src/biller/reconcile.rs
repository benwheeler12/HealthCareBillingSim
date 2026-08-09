//! Reconciliation — the biller's semantic-fault detector. Pure function:
//! per billable line, billed must equal the payer's full accounting, exact,
//! in cents (fault 3.1).

use std::collections::HashMap;

use crate::domain::{Claim, Money, RemittanceAdvice, ServiceLine, VirtualTime};
use crate::ledger::records::Adjudication;

pub enum ReconcileOutcome {
    /// Complete and exact: the claim is Resolved (even if fully denied — fault 3.4).
    Balanced { lines: Vec<(String, Adjudication)> },
    /// Complete but the money doesn't sum: Flagged(ReconciliationFailed).
    Unbalanced {
        lines: Vec<(String, Adjudication)>,
        billed: Money,
        accounted: Money,
    },
    /// Unknown or duplicate service lines: Flagged(MalformedRemittance).
    /// TODO(fault 3.3): missing lines should leave the claim partially
    /// adjudicated and aging, not flagged; refine when that row lands.
    Malformed { detail: String },
}

pub fn reconcile(claim: &Claim, remit: &RemittanceAdvice, now: VirtualTime) -> ReconcileOutcome {
    let billable: HashMap<&str, &ServiceLine> = claim
        .billable_lines()
        .map(|l| (l.service_line_id.as_str(), l))
        .collect();

    let mut lines = Vec::with_capacity(remit.lines.len());
    let mut billed_total = Money::ZERO;
    let mut accounted_total = Money::ZERO;
    let mut all_balanced = true;

    for rline in &remit.lines {
        let Some(service_line) = billable.get(rline.service_line_id.as_str()) else {
            return ReconcileOutcome::Malformed {
                detail: format!("unknown service_line_id {:?}", rline.service_line_id),
            };
        };
        if lines.iter().any(|(id, _)| id == &rline.service_line_id) {
            return ReconcileOutcome::Malformed {
                detail: format!("duplicate service_line_id {:?}", rline.service_line_id),
            };
        }
        let billed = service_line.billed();
        let accounted = rline.accounted();
        billed_total += billed;
        accounted_total += accounted;
        all_balanced &= billed == accounted;
        lines.push((
            rline.service_line_id.clone(),
            Adjudication {
                payer_paid: rline.payer_paid,
                coinsurance: rline.coinsurance,
                copay: rline.copay,
                deductible: rline.deductible,
                not_allowed: rline.not_allowed,
                denial_reason: rline.denial_reason,
                adjudicated_at: now,
            },
        ));
    }

    if lines.len() < billable.len() {
        return ReconcileOutcome::Malformed {
            detail: format!(
                "remittance covers {} of {} billable lines",
                lines.len(),
                billable.len()
            ),
        };
    }
    if !all_balanced {
        return ReconcileOutcome::Unbalanced {
            lines,
            billed: billed_total,
            accounted: accounted_total,
        };
    }
    ReconcileOutcome::Balanced { lines }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ClaimId, DenialReason, PayerId, RemittanceLine};

    fn claim_with_lines(lines: Vec<ServiceLine>) -> Claim {
        Claim {
            claim_id: ClaimId("c-1".into()),
            payer_id: PayerId::Medicare,
            patient_member_id: "M1".into(),
            provider_npi: "1234567890".into(),
            organization_name: "Acme".into(),
            lines,
        }
    }

    fn line(id: &str, cents: i64, do_not_bill: bool) -> ServiceLine {
        ServiceLine {
            service_line_id: id.into(),
            procedure_code: "99213".into(),
            units: 1,
            unit_charge: Money::from_cents(cents),
            do_not_bill,
        }
    }

    fn remit_line(id: &str, paid: i64, not_allowed: i64) -> RemittanceLine {
        RemittanceLine {
            service_line_id: id.into(),
            payer_paid: Money::from_cents(paid),
            coinsurance: Money::ZERO,
            copay: Money::ZERO,
            deductible: Money::ZERO,
            not_allowed: Money::from_cents(not_allowed),
            denial_reason: None,
        }
    }

    fn remit(lines: Vec<RemittanceLine>) -> RemittanceAdvice {
        RemittanceAdvice {
            claim_id: ClaimId("c-1".into()),
            payer_id: PayerId::Medicare,
            lines,
        }
    }

    #[test]
    fn full_denial_that_sums_is_balanced_not_an_error() {
        let claim = claim_with_lines(vec![line("L1", 1000, false)]);
        let mut denial = remit_line("L1", 0, 1000);
        denial.denial_reason = Some(DenialReason::NotCovered);
        let outcome = reconcile(&claim, &remit(vec![denial]), VirtualTime::default());
        assert!(matches!(outcome, ReconcileOutcome::Balanced { .. }));
    }

    #[test]
    fn unknown_line_is_malformed() {
        let claim = claim_with_lines(vec![line("L1", 1000, false)]);
        let outcome = reconcile(
            &claim,
            &remit(vec![remit_line("L9", 1000, 0)]),
            VirtualTime::default(),
        );
        assert!(matches!(outcome, ReconcileOutcome::Malformed { .. }));
    }

    #[test]
    fn do_not_bill_lines_are_not_expected_in_the_remittance() {
        let claim = claim_with_lines(vec![line("L1", 1000, false), line("L2", 500, true)]);
        let outcome = reconcile(
            &claim,
            &remit(vec![remit_line("L1", 1000, 0)]),
            VirtualTime::default(),
        );
        assert!(matches!(outcome, ReconcileOutcome::Balanced { lines } if lines.len() == 1));
    }

    #[test]
    fn one_unbalanced_line_flags_the_claim_with_totals() {
        let claim = claim_with_lines(vec![line("L1", 1000, false), line("L2", 500, false)]);
        let outcome = reconcile(
            &claim,
            &remit(vec![remit_line("L1", 1000, 0), remit_line("L2", 499, 0)]),
            VirtualTime::default(),
        );
        assert!(matches!(
            outcome,
            ReconcileOutcome::Unbalanced { billed, accounted, .. }
                if billed == Money::from_cents(1500) && accounted == Money::from_cents(1499)
        ));
    }
}

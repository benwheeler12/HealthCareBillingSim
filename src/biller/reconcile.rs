//! Reconciliation — the biller's semantic-fault detector. Pure function:
//! per billable line, billed must equal the payer's full accounting, exact,
//! in cents (fault 3.1). Since fault 3.3, remittances may arrive with lines
//! missing: reconciliation accumulates — lines already answered are skipped
//! idempotently, new lines are applied, and the claim completes only when
//! every billable line has a balanced answer.

use std::collections::BTreeMap;
use std::collections::HashMap;

use crate::domain::{Claim, Money, RemittanceAdvice, ServiceLine, VirtualTime};
use crate::ledger::records::Adjudication;

pub enum ReconcileOutcome {
    /// Unknown or duplicated service lines: Flagged(MalformedRemittance).
    Malformed { detail: String },
    /// Some received line doesn't sum: Flagged(ReconciliationFailed).
    /// Totals cover everything received so far (answered ∪ new).
    Unbalanced {
        new_lines: Vec<(String, Adjudication)>,
        billed: Money,
        accounted: Money,
    },
    /// Balanced so far but billable lines remain unanswered (fault 3.3):
    /// the claim stays partially adjudicated and keeps aging.
    Partial {
        new_lines: Vec<(String, Adjudication)>,
    },
    /// Every billable line answered and exact: Resolved.
    Complete {
        new_lines: Vec<(String, Adjudication)>,
    },
}

pub fn reconcile(
    claim: &Claim,
    answered: &BTreeMap<String, Adjudication>,
    remit: &RemittanceAdvice,
    now: VirtualTime,
) -> ReconcileOutcome {
    let billable: HashMap<&str, &ServiceLine> = claim
        .billable_lines()
        .map(|l| (l.service_line_id.as_str(), l))
        .collect();

    let mut new_lines: Vec<(String, Adjudication)> = Vec::with_capacity(remit.lines.len());
    let mut all_balanced = true;

    for rline in &remit.lines {
        if !billable.contains_key(rline.service_line_id.as_str()) {
            return ReconcileOutcome::Malformed {
                detail: format!("unknown service_line_id {:?}", rline.service_line_id),
            };
        }
        if new_lines.iter().any(|(id, _)| id == &rline.service_line_id) {
            return ReconcileOutcome::Malformed {
                detail: format!("duplicate service_line_id {:?}", rline.service_line_id),
            };
        }
        // Already answered (duplicate delivery / re-sent line): skip
        // idempotently — determinism guarantees the copy is identical.
        if answered.contains_key(&rline.service_line_id) {
            continue;
        }
        all_balanced &= billable[rline.service_line_id.as_str()].billed() == rline.accounted();
        new_lines.push((
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

    if !all_balanced {
        let (billed, accounted) = totals(&billable, answered, &new_lines);
        return ReconcileOutcome::Unbalanced {
            new_lines,
            billed,
            accounted,
        };
    }
    if answered.len() + new_lines.len() < billable.len() {
        return ReconcileOutcome::Partial { new_lines };
    }
    ReconcileOutcome::Complete { new_lines }
}

/// Billed vs accounted over everything received so far.
fn totals(
    billable: &HashMap<&str, &ServiceLine>,
    answered: &BTreeMap<String, Adjudication>,
    new_lines: &[(String, Adjudication)],
) -> (Money, Money) {
    let received = answered
        .iter()
        .chain(new_lines.iter().map(|(id, adj)| (id, adj)));
    let mut billed = Money::ZERO;
    let mut accounted = Money::ZERO;
    for (id, adj) in received {
        billed += billable[id.as_str()].billed();
        accounted += adj.payer_paid + adj.patient_responsibility() + adj.not_allowed;
    }
    (billed, accounted)
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

    fn none_answered() -> BTreeMap<String, Adjudication> {
        BTreeMap::new()
    }

    #[test]
    fn full_denial_that_sums_is_complete_not_an_error() {
        let claim = claim_with_lines(vec![line("L1", 1000, false)]);
        let mut denial = remit_line("L1", 0, 1000);
        denial.denial_reason = Some(DenialReason::NotCovered);
        let outcome = reconcile(
            &claim,
            &none_answered(),
            &remit(vec![denial]),
            VirtualTime::default(),
        );
        assert!(matches!(outcome, ReconcileOutcome::Complete { .. }));
    }

    #[test]
    fn unknown_line_is_malformed() {
        let claim = claim_with_lines(vec![line("L1", 1000, false)]);
        let outcome = reconcile(
            &claim,
            &none_answered(),
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
            &none_answered(),
            &remit(vec![remit_line("L1", 1000, 0)]),
            VirtualTime::default(),
        );
        assert!(
            matches!(outcome, ReconcileOutcome::Complete { new_lines } if new_lines.len() == 1)
        );
    }

    #[test]
    fn one_unbalanced_line_flags_with_received_totals() {
        let claim = claim_with_lines(vec![line("L1", 1000, false), line("L2", 500, false)]);
        let outcome = reconcile(
            &claim,
            &none_answered(),
            &remit(vec![remit_line("L1", 1000, 0), remit_line("L2", 499, 0)]),
            VirtualTime::default(),
        );
        assert!(matches!(
            outcome,
            ReconcileOutcome::Unbalanced { billed, accounted, .. }
                if billed == Money::from_cents(1500) && accounted == Money::from_cents(1499)
        ));
    }

    #[test]
    fn missing_lines_are_partial_then_accumulate_to_complete() {
        let claim = claim_with_lines(vec![line("L1", 1000, false), line("L2", 500, false)]);

        let first = reconcile(
            &claim,
            &none_answered(),
            &remit(vec![remit_line("L1", 1000, 0)]),
            VirtualTime::default(),
        );
        let ReconcileOutcome::Partial { new_lines } = first else {
            panic!("expected partial");
        };
        assert_eq!(new_lines.len(), 1);

        let answered: BTreeMap<String, Adjudication> = new_lines.into_iter().collect();
        // Re-sent full remit: L1 skipped idempotently, L2 completes the claim.
        let second = reconcile(
            &claim,
            &answered,
            &remit(vec![remit_line("L1", 1000, 0), remit_line("L2", 500, 0)]),
            VirtualTime::default(),
        );
        assert!(matches!(
            second,
            ReconcileOutcome::Complete { new_lines } if new_lines.len() == 1 && new_lines[0].0 == "L2"
        ));
    }
}

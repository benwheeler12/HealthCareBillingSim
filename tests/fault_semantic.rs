//! Class-3 (semantic) fault rows: the payer's *content* is wrong, not the
//! transport. The reconciliation equation is the biller's only detector.

mod common;

use common::{run_sim_with, simple_claim, write_input};
use healthcare_billing_sim::RunConfig;
use healthcare_billing_sim::domain::PayerId;
use healthcare_billing_sim::ledger::records::{ClaimState, FlagReason};
use healthcare_billing_sim::sim::sim_truth::FaultKind;

/// 3.1: amounts don't sum. The dishonest payer reproduces the identical lie
/// on every re-delivery (adjudication-family key), so retries can't wash it
/// out — the claim lands in Flagged(ReconciliationFailed) with the exact
/// billed/accounted discrepancy for the human reviewer.
#[test]
fn dishonest_adjudication_is_flagged_with_exact_discrepancy() {
    let input: Vec<String> = (0..30).map(simple_claim).collect();
    let path = write_input("dishonest.jsonl", &input);
    let mut cfg = RunConfig::new(path, 42, 10.0);
    cfg.faults.dishonest_adjudication_rate = 0.3;

    let output = run_sim_with(cfg);
    let liars = output.sim_truth.claims_with(FaultKind::Dishonest);
    assert!(!liars.is_empty(), "seed must actually inject dishonesty");

    for record in output.ledger.claims.values() {
        assert!(
            record.state.is_terminal(),
            "claim {} stuck",
            record.claim_id
        );
        let lied_to = liars.contains(&record.claim_id);
        match &record.state {
            ClaimState::Flagged {
                reason: FlagReason::ReconciliationFailed { billed, accounted },
            } => {
                assert!(lied_to, "honest claim {} flagged", record.claim_id);
                let skim = *accounted - *billed;
                assert!(
                    (1..=99).contains(&skim.cents()),
                    "discrepancy is the injected skim, exactly"
                );
            }
            ClaimState::Resolved => assert!(!lied_to, "lie about {} undetected", record.claim_id),
            state => panic!("unexpected state {state:?} for {}", record.claim_id),
        }
    }
}

/// 3.2: the remittance's claim_id was mangled in transit. It must never
/// attach to any claim — it lands in quarantine — while the real claim hears
/// silence and recovers through the ordinary retry machinery.
#[test]
fn corrupt_claim_ids_quarantine_and_never_attach() {
    let input: Vec<String> = (0..30).map(simple_claim).collect();
    let path = write_input("corrupt_ids.jsonl", &input);
    let mut cfg = RunConfig::new(path, 42, 10.0);
    cfg.faults.corrupt_claim_id_rate = 0.25;

    let output = run_sim_with(cfg);
    let corrupted = output.sim_truth.claims_with(FaultKind::CorruptClaimId);
    assert!(!corrupted.is_empty(), "seed must actually corrupt ids");
    assert_eq!(
        output.ledger.quarantine.len(),
        output.sim_truth.count(FaultKind::CorruptClaimId),
        "every corrupted remittance quarantined, nothing else"
    );
    for q in &output.ledger.quarantine {
        assert!(q.claim_id.0.starts_with("corrupt/"));
        assert!(
            !output.ledger.claims.contains_key(&q.claim_id),
            "quarantined id must not be a ledger row"
        );
    }

    let mut recovered = 0;
    for record in output.ledger.claims.values() {
        assert!(
            record.state.is_terminal(),
            "claim {} stuck",
            record.claim_id
        );
        if corrupted.contains(&record.claim_id) && record.state == ClaimState::Resolved {
            assert!(record.attempts > 1, "recovery must have come from a retry");
            recovered += 1;
        }
    }
    assert!(recovered > 0, "at least one corrupted claim must recover");
}

/// 3.4: a full denial that sums correctly is NOT an error. Lifecycle only
/// asks "do I have a complete, consistent answer?" — denial is a financial
/// outcome, visible to reports, not a state.
#[test]
fn full_denial_that_sums_is_resolved_not_flagged() {
    let input: Vec<String> = (0..9).map(simple_claim).collect();
    let path = write_input("denials.jsonl", &input);
    let mut cfg = RunConfig::new(path, 42, 10.0);
    for payer in [
        PayerId::Medicare,
        PayerId::UnitedHealthGroup,
        PayerId::Anthem,
    ] {
        cfg.payers.get_mut(&payer).expect("config").denial_rate = 1.0;
    }

    let output = run_sim_with(cfg);
    for record in output.ledger.claims.values() {
        assert_eq!(
            record.state,
            ClaimState::Resolved,
            "claim {}",
            record.claim_id
        );
        for line in record.lines.iter().filter(|l| !l.do_not_bill) {
            let adj = line.adjudication.as_ref().expect("adjudicated");
            assert!(
                adj.denial_reason.is_some(),
                "denial reason recorded for reports"
            );
            assert_eq!(adj.payer_paid, healthcare_billing_sim::domain::Money::ZERO);
            assert_eq!(adj.not_allowed, line.billed(), "books still exact");
        }
    }
}

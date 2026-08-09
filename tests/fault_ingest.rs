//! Class-1 (ingest) fault rows. 1.1 and 1.2 (parse/schema rejection) are
//! covered in tests/happy_path.rs; this file owns 1.3 (duplicate claim_id)
//! and 1.4 (schema-valid but odd billable policy).

mod common;

use common::{claim_line, run_sim_with, service_line, simple_claim, write_input};
use healthcare_billing_sim::RunConfig;
use healthcare_billing_sim::domain::Money;
use healthcare_billing_sim::ledger::events::ClaimEvent;
use healthcare_billing_sim::ledger::records::ClaimState;
use serde_json::json;

/// 1.3: the same claim_id appears twice in the file. First document wins;
/// the duplicate becomes a history event on the existing record and is never
/// re-submitted.
#[test]
fn duplicate_claim_id_is_deduped_at_ingest() {
    let mut input = vec![simple_claim(0), simple_claim(1)];
    // Same id as claim 0, different money — must NOT overwrite or resubmit.
    input.push(claim_line(
        "c-000",
        "anthem",
        json!([service_line("L9", 9, 999.0, false)]),
    ));

    let path = write_input("dup_ingest.jsonl", &input);
    let output = run_sim_with(RunConfig::new(path, 42, 10.0));

    assert_eq!(output.ledger.claims.len(), 2, "dup must not create a row");
    let record = &output.ledger.claims[&healthcare_billing_sim::domain::ClaimId("c-000".into())];
    assert_eq!(record.state, ClaimState::Resolved);
    assert_eq!(record.lines.len(), 1, "first document won");
    assert_eq!(record.lines[0].service_line_id, "L1");
    assert!(
        record
            .history
            .iter()
            .any(|e| matches!(e.event, ClaimEvent::DuplicateIngest { line_no: 3 })),
        "dup recorded in history"
    );
    let submissions = output
        .ledger
        .event_log
        .iter()
        .filter(|e| e.claim_id.0 == "c-000" && matches!(e.event, ClaimEvent::Submitted { .. }))
        .count();
    assert_eq!(submissions, 1, "duplicate must never be submitted");
}

/// 1.4: schema-valid but odd claims. All-do_not_bill resolves trivially at
/// ingest with zero submissions (correct books, nothing to bill); zero-dollar
/// billable lines flow through the normal lifecycle and reconcile to zero.
#[test]
fn billable_policy_all_do_not_bill_and_zero_dollar() {
    let input = vec![
        claim_line(
            "nothing-billable",
            "medicare",
            json!([
                service_line("L1", 1, 50.0, true),
                service_line("L2", 2, 25.0, true)
            ]),
        ),
        claim_line(
            "zero-dollar",
            "united_health_group",
            json!([service_line("L1", 1, 0.0, false)]),
        ),
    ];
    let path = write_input("billable_policy.jsonl", &input);
    let output = run_sim_with(RunConfig::new(path, 42, 10.0));

    let trivial =
        &output.ledger.claims[&healthcare_billing_sim::domain::ClaimId("nothing-billable".into())];
    assert_eq!(trivial.state, ClaimState::Resolved);
    assert_eq!(trivial.attempts, 0, "never submitted");
    assert!(trivial.first_submitted_at.is_none());
    assert!(trivial.lines.iter().all(|l| l.adjudication.is_none()));

    let zero =
        &output.ledger.claims[&healthcare_billing_sim::domain::ClaimId("zero-dollar".into())];
    assert_eq!(zero.state, ClaimState::Resolved);
    assert_eq!(zero.attempts, 1, "zero-dollar still goes to the payer");
    let adj = zero.lines[0].adjudication.as_ref().expect("adjudicated");
    assert_eq!(
        adj.payer_paid + adj.coinsurance + adj.copay + adj.deductible + adj.not_allowed,
        Money::ZERO,
        "zero billed reconciles to zero, exactly"
    );
}

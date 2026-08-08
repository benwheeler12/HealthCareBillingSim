//! Build-step-3 vertical slice: happy path end-to-end with zero faults.
//! Asserts the global invariant early — every ingested claim reaches a
//! terminal state, none lost, stuck, or double-counted — plus the
//! reconciliation equation and outcome-determinism.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

use healthcare_billing_sim::domain::Money;
use healthcare_billing_sim::ledger::records::{ClaimState, Ledger};
use healthcare_billing_sim::{RunConfig, run};
use serde_json::json;

fn claim_line(claim_id: &str, payer: &str, lines: serde_json::Value) -> String {
    json!({
        "claim_id": claim_id,
        "place_of_service_code": 11,
        "insurance": {"payer_id": payer, "patient_member_id": format!("M-{claim_id}")},
        "patient": {"first_name": "Ada", "last_name": "Lovelace", "gender": "f", "dob": "1985-12-10"},
        "organization": {"name": "Analytical Engines LLC"},
        "rendering_provider": {"first_name": "Grace", "last_name": "Hopper", "npi": "1234567890"},
        "service_lines": lines,
    })
    .to_string()
}

fn service_line(id: &str, units: u32, dollars: f64, do_not_bill: bool) -> serde_json::Value {
    json!({
        "service_line_id": id,
        "procedure_code": "99213",
        "units": units,
        "details": "office visit",
        "unit_charge_currency": "USD",
        "unit_charge_amount": dollars,
        "do_not_bill": do_not_bill,
    })
}

fn write_input(name: &str, lines: &[String]) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let mut file = std::fs::File::create(&path).expect("create input");
    for line in lines {
        writeln!(file, "{line}").expect("write input");
    }
    path
}

fn happy_input() -> Vec<String> {
    vec![
        claim_line("med-1", "medicare", json!([service_line("L1", 2, 123.45, false)])),
        claim_line(
            "med-2",
            "medicare",
            json!([service_line("L1", 1, 80.00, false), service_line("L2", 3, 42.10, false)]),
        ),
        claim_line("uhg-1", "united_health_group", json!([service_line("L1", 1, 250.00, false)])),
        claim_line(
            "uhg-2",
            "united_health_group",
            json!([service_line("L1", 4, 55.25, false), service_line("L2", 1, 10.00, true)]),
        ),
        claim_line("ant-1", "anthem", json!([service_line("L1", 1, 999.99, false)])),
        claim_line("ant-2", "anthem", json!([service_line("L1", 2, 75.50, false)])),
    ]
}

fn run_sim(path: PathBuf, seed: u64) -> Ledger {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("runtime");
    runtime.block_on(run(RunConfig::new(path, seed, 1.0))).expect("run")
}

/// Comparable fingerprint of every claim's outcome: terminal state plus the
/// full money picture. Used for the determinism assertion.
fn outcomes(ledger: &Ledger) -> BTreeMap<String, String> {
    ledger
        .claims
        .values()
        .map(|record| {
            let money: Vec<String> = record
                .lines
                .iter()
                .map(|l| format!("{}:{:?}", l.service_line_id, l.adjudication.as_ref().map(|a| (
                    a.payer_paid, a.coinsurance, a.copay, a.deductible, a.not_allowed, a.denial_reason,
                ))))
                .collect();
            (record.claim_id.0.clone(), format!("{:?} {}", record.state, money.join(" ")))
        })
        .collect()
}

#[test]
fn every_claim_reaches_resolved_and_reconciles_exactly() {
    let path = write_input("happy.jsonl", &happy_input());
    let ledger = run_sim(path, 42);

    assert_eq!(ledger.claims.len(), 6);
    assert!(ledger.quarantine.is_empty());
    for record in ledger.claims.values() {
        assert_eq!(record.state, ClaimState::Resolved, "claim {}", record.claim_id);
        assert_eq!(record.attempts, 1, "zero faults means no retries");
        assert!(record.first_submitted_at.is_some());
        assert!(record.resolved_at.is_some());
        for line in &record.lines {
            if line.do_not_bill {
                assert!(line.adjudication.is_none(), "do_not_bill lines are never submitted");
                continue;
            }
            let adj = line.adjudication.as_ref().expect("billable line adjudicated");
            let accounted = adj.payer_paid + adj.patient_responsibility() + adj.not_allowed;
            assert_eq!(line.billed(), accounted, "reconciliation equation, exact, in cents");
        }
    }
}

#[test]
fn same_seed_same_outcomes_different_seed_diverges() {
    let path = write_input("determinism.jsonl", &happy_input());
    let first = outcomes(&run_sim(path.clone(), 7));
    let second = outcomes(&run_sim(path.clone(), 7));
    assert_eq!(first, second, "outcome-determinism given a seed");

    let other = outcomes(&run_sim(path, 8));
    assert_ne!(first, other, "different seed should adjudicate differently");
}

#[test]
fn malformed_lines_become_rejected_rows_and_do_not_stop_valid_ones() {
    let mut lines = vec![
        "{this is not json".to_string(),
        claim_line("bad-npi", "medicare", json!([service_line("L1", 1, 10.0, false)]))
            .replace("1234567890", "123"),
    ];
    // Structurally valid JSON, missing the insurance block entirely.
    lines.push(
        json!({
            "claim_id": "no-insurance",
            "place_of_service_code": 11,
            "patient": {"first_name": "A", "last_name": "B", "gender": "m", "dob": "1990-01-01"},
            "organization": {"name": "Org"},
            "rendering_provider": {"first_name": "C", "last_name": "D", "npi": "1234567890"},
            "service_lines": [service_line("L1", 1, 10.0, false)],
        })
        .to_string(),
    );
    lines.extend(happy_input());

    let path = write_input("rejects.jsonl", &lines);
    let ledger = run_sim(path, 42);

    assert_eq!(ledger.claims.len(), 9, "3 rejected rows + 6 resolved");
    let rejected: Vec<_> = ledger
        .claims
        .values()
        .filter(|r| matches!(r.state, ClaimState::Rejected { .. }))
        .collect();
    assert_eq!(rejected.len(), 3, "malformed lines are ledger rows, not silent drops");
    assert!(rejected.iter().any(|r| r.claim_id.0 == "bad-npi"));
    assert!(rejected.iter().any(|r| r.claim_id.0 == "no-insurance"));
    assert!(rejected.iter().any(|r| r.claim_id.0.starts_with("<unparseable-line-")));

    let resolved = ledger
        .claims
        .values()
        .filter(|r| r.state == ClaimState::Resolved)
        .count();
    assert_eq!(resolved, 6);

    // Rejected rows carry no money.
    for record in rejected {
        assert_eq!(record.lines.iter().map(|l| l.billed()).sum::<Money>(), Money::ZERO);
    }
}

//! Class-3 (semantic) fault rows: the payer's *content* is wrong, not the
//! transport. The reconciliation equation is the biller's only detector.

mod common;

use common::{run_sim_with, simple_claim, write_input};
use healthcare_billing_sim::RunConfig;
use healthcare_billing_sim::domain::PayerId;
use healthcare_billing_sim::ledger::events::ClaimEvent;
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

/// 3.3: service lines missing from the remittance. What arrived is applied
/// and balanced; the claim stays partially adjudicated and keeps aging until
/// a retry's re-delivery (fresh transport fates per attempt) fills the gaps —
/// or the budget runs out and the partial claim is flagged, partial books
/// intact. Unknown lines were covered as Malformed in the reconcile tests.
#[test]
fn missing_lines_stay_partial_and_accumulate_across_retries() {
    let input: Vec<String> = (0..30)
        .map(|i| {
            let payer = ["medicare", "united_health_group", "anthem"][i % 3];
            common::claim_line(
                &format!("c-{i:03}"),
                payer,
                serde_json::json!([
                    common::service_line("L1", 1, 100.0, false),
                    common::service_line("L2", 2, 55.5, false),
                    common::service_line("L3", 1, 20.25, false)
                ]),
            )
        })
        .collect();
    let path = write_input("line_drops.jsonl", &input);
    let mut cfg = RunConfig::new(path, 42, 10.0);
    cfg.faults.line_drop_rate = 0.25;

    let output = run_sim_with(cfg);
    assert!(output.sim_truth.count(FaultKind::LineDrop) > 0);

    let mut accumulated = 0;
    for record in output.ledger.claims.values() {
        assert!(
            record.state.is_terminal(),
            "claim {} stuck",
            record.claim_id
        );
        let applications = record
            .history
            .iter()
            .filter(|e| matches!(e.event, ClaimEvent::RemittanceApplied { .. }))
            .count();
        if applications >= 2 {
            accumulated += 1; // a partial was later completed by a re-delivery
        }
        match &record.state {
            ClaimState::Resolved => {
                for line in record.lines.iter().filter(|l| !l.do_not_bill) {
                    let adj = line.adjudication.as_ref().expect("all lines answered");
                    assert_eq!(
                        line.billed(),
                        adj.payer_paid + adj.patient_responsibility() + adj.not_allowed
                    );
                }
            }
            ClaimState::Flagged {
                reason: FlagReason::RetriesExhausted,
            } => {
                // Partial books are retained, and every answered line balances.
                for line in record.lines.iter().filter(|l| l.adjudication.is_some()) {
                    let adj = line.adjudication.as_ref().expect("checked");
                    assert_eq!(
                        line.billed(),
                        adj.payer_paid + adj.patient_responsibility() + adj.not_allowed
                    );
                }
            }
            state => panic!("unexpected state {state:?} for {}", record.claim_id),
        }
    }
    assert!(
        accumulated > 0,
        "seed must exercise partial-then-complete accumulation"
    );
}

/// 3.5: the remittance turns to unparseable garbage in transit. Epistemically
/// that is silence (DESIGN.md Decisions) — the timeout machinery owns recovery — but
/// a surviving claim_id fragment earns a "garbage received" history note,
/// and uncorrelatable wreckage lands in quarantine.
#[test]
fn garbage_remittances_are_silence_with_a_history_note() {
    let input: Vec<String> = (0..30).map(simple_claim).collect();
    let path = write_input("garbage.jsonl", &input);
    let mut cfg = RunConfig::new(path, 42, 10.0);
    cfg.faults.corrupt_remittance_rate = 0.3;

    let output = run_sim_with(cfg);
    let corrupted = output.sim_truth.claims_with(FaultKind::CorruptRemittance);
    assert!(
        !corrupted.is_empty(),
        "seed must actually corrupt remittances"
    );

    let noted: usize = output
        .ledger
        .claims
        .values()
        .map(|r| {
            r.history
                .iter()
                .filter(|e| matches!(e.event, ClaimEvent::GarbageRemittance))
                .count()
        })
        .sum();
    let quarantined = output.ledger.quarantine.len();
    assert_eq!(
        noted + quarantined,
        output.sim_truth.count(FaultKind::CorruptRemittance),
        "every garbage delivery is either noted on its claim or quarantined"
    );
    assert!(noted > 0, "seed must produce correlatable garbage");
    assert!(quarantined > 0, "seed must produce uncorrelatable garbage");

    let mut recovered = 0;
    for record in output.ledger.claims.values() {
        assert!(
            record.state.is_terminal(),
            "claim {} stuck",
            record.claim_id
        );
        // No garbage ever carries money: any adjudication present is balanced.
        for line in record.lines.iter().filter(|l| l.adjudication.is_some()) {
            let adj = line.adjudication.as_ref().expect("checked");
            assert_eq!(
                line.billed(),
                adj.payer_paid + adj.patient_responsibility() + adj.not_allowed
            );
        }
        if corrupted.contains(&record.claim_id) && record.state == ClaimState::Resolved {
            assert!(record.attempts > 1, "recovery must have come from a retry");
            recovered += 1;
        }
    }
    assert!(
        recovered > 0,
        "at least one garbage-struck claim must recover"
    );
}

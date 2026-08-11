//! Class-2 (transport) fault rows, one test per row of the DESIGN.md fault
//! table. Sim-truth is the oracle: every assertion about "what really
//! happened" comes from the recorder, never from the ledger.

mod common;

use common::{run_sim_with, simple_claim, write_input};
use std::time::Duration;

use healthcare_billing_sim::RunConfig;
use healthcare_billing_sim::biller::policy::RetryPolicy;
use healthcare_billing_sim::domain::{SubmittedClaim, SubmittedLine};
use healthcare_billing_sim::ledger::events::ClaimEvent;
use healthcare_billing_sim::ledger::records::{ClaimState, FlagReason};
use healthcare_billing_sim::sim::payer::{adjudicate, default_payer_configs};
use healthcare_billing_sim::sim::rng::RngFactory;
use healthcare_billing_sim::sim::sim_truth::FaultKind;

/// 2.1: claims dropped on the forward hop are pure silence. The biller's
/// timeout + bounded retry with backoff recovers them; `first_submitted_at`
/// is set once and never touched by retries.
#[test]
fn forward_drops_are_recovered_by_retry() {
    let input: Vec<String> = (0..40).map(simple_claim).collect();
    let path = write_input("forward_drops.jsonl", &input);
    let mut cfg = RunConfig::new(path, 42, 10.0);
    cfg.faults.forward_drop_rate = 0.3;

    let output = run_sim_with(cfg);
    let dropped = output.sim_truth.claims_with(FaultKind::ForwardDrop);
    assert!(!dropped.is_empty(), "seed must actually inject drops");

    let mut recovered = 0;
    for record in output.ledger.claims.values() {
        // Global invariant first: nothing non-terminal, ever.
        assert!(
            record.state.is_terminal(),
            "claim {} stuck",
            record.claim_id
        );

        if !dropped.contains(&record.claim_id) {
            assert_eq!(
                record.attempts, 1,
                "undropped claim {} retried",
                record.claim_id
            );
            continue;
        }
        assert!(
            record.attempts > 1,
            "dropped claim {} never retried",
            record.claim_id
        );
        if record.state == ClaimState::Resolved {
            recovered += 1;
        }
        // Aging keys off first submission: the recorded first_submitted_at
        // must predate the last Submitted event's stamp.
        let submits: Vec<_> = record
            .history
            .iter()
            .filter(|e| {
                matches!(
                    e.event,
                    healthcare_billing_sim::ledger::events::ClaimEvent::Submitted { .. }
                )
            })
            .collect();
        assert!(submits.len() > 1);
        assert_eq!(record.first_submitted_at, Some(submits[0].at));
    }
    assert!(
        recovered > 0,
        "at least one dropped claim must recover via retry"
    );
}

/// 2.2: the payer adjudicated but the remittance was lost. Indistinguishable
/// from 2.1 biller-side; the retry provokes a re-delivery, and statelessness
/// guarantees the re-derived remittance is identical — so the recovered
/// claim's books are exactly what the first (lost) remittance said.
#[test]
fn return_drops_recover_with_identical_rederived_remittance() {
    let input: Vec<String> = (0..40).map(simple_claim).collect();
    let path = write_input("return_drops.jsonl", &input);
    let mut cfg = RunConfig::new(path, 42, 10.0);
    cfg.faults.return_drop_rate = 0.3;

    let output = run_sim_with(cfg);
    let dropped = output.sim_truth.claims_with(FaultKind::ReturnDrop);
    assert!(
        !dropped.is_empty(),
        "seed must actually inject return drops"
    );

    let mut recovered = 0;
    for record in output.ledger.claims.values() {
        assert!(
            record.state.is_terminal(),
            "claim {} stuck",
            record.claim_id
        );
        if dropped.contains(&record.claim_id) && record.state == ClaimState::Resolved {
            assert!(record.attempts > 1);
            recovered += 1;
            // Reconciliation held on the re-derived remittance: exact books.
            for line in record.lines.iter().filter(|l| !l.do_not_bill) {
                let adj = line.adjudication.as_ref().expect("adjudicated");
                assert_eq!(
                    line.billed(),
                    adj.payer_paid + adj.patient_responsibility() + adj.not_allowed
                );
            }
        }
    }
    assert!(
        recovered > 0,
        "at least one return-dropped claim must recover"
    );
}

/// 2.6: enough consecutive drops exhaust the retry budget — the claim is
/// Flagged(RetriesExhausted), a terminal state for the human-review queue.
/// With a 100% drop rate every claim lands there after exactly max_attempts.
#[test]
fn drops_exhausting_retry_budget_flag_the_claim() {
    let input: Vec<String> = (0..10).map(simple_claim).collect();
    let path = write_input("exhausted.jsonl", &input);
    let mut cfg = RunConfig::new(path, 42, 10.0);
    cfg.faults.forward_drop_rate = 1.0;

    let output = run_sim_with(cfg);
    assert_eq!(output.sim_truth.count(FaultKind::ForwardDrop), 10 * 3);
    for record in output.ledger.claims.values() {
        assert_eq!(
            record.state,
            ClaimState::Flagged {
                reason: FlagReason::RetriesExhausted
            },
            "claim {}",
            record.claim_id
        );
        assert_eq!(record.attempts, 3, "bounded: exactly max_attempts");
        assert!(record.first_submitted_at.is_some());
    }
}

/// 2.3: delay > biller timeout. The timeout is emergent — payer latency plus
/// injected transport delay crossing the biller's policy line. Retries whose
/// delays also cross it exhaust the budget; the delayed remittances then
/// arrive anyway, and a complete, balanced late answer is allowed to
/// transition Flagged(RetriesExhausted) → Resolved (DESIGN.md Decisions). A second
/// late copy on the now-Resolved claim is ignored idempotency (DESIGN.md Decisions).
#[test]
fn delays_beyond_timeout_flag_then_late_remittance_resolves() {
    let input: Vec<String> = (0..40).map(simple_claim).collect();
    let path = write_input("delays.jsonl", &input);
    let mut cfg = RunConfig::new(path, 42, 10.0);
    cfg.policy = RetryPolicy {
        max_attempts: 2,
        timeout: Duration::from_secs(45), // above every honest payer's max latency
        backoff_base: Duration::from_secs(5),
    };
    cfg.faults.extra_delay_rate = 0.4;
    cfg.faults.max_extra_delay_secs = 600.0;

    let output = run_sim_with(cfg);
    assert!(output.sim_truth.count(FaultKind::ExtraDelay) > 0);

    let mut flagged_then_resolved = 0;
    let mut ignored_second_late = 0;
    for record in output.ledger.claims.values() {
        assert!(
            record.state.is_terminal(),
            "claim {} stuck",
            record.claim_id
        );
        let was_flagged = record.history.iter().any(|e| {
            matches!(
                e.event,
                ClaimEvent::Flagged {
                    reason: FlagReason::RetriesExhausted
                }
            )
        });
        let late_count = record
            .history
            .iter()
            .filter(|e| matches!(e.event, ClaimEvent::LateRemittance { .. }))
            .count();
        if was_flagged && record.state == ClaimState::Resolved {
            flagged_then_resolved += 1;
            assert!(
                late_count >= 1,
                "resolution must have come from a late remit"
            );
            // Books are exact even though the answer came after the flag.
            for line in record.lines.iter().filter(|l| !l.do_not_bill) {
                let adj = line.adjudication.as_ref().expect("late-adjudicated");
                assert_eq!(
                    line.billed(),
                    adj.payer_paid + adj.patient_responsibility() + adj.not_allowed
                );
            }
        }
        if record.state == ClaimState::Resolved && late_count >= 2 {
            ignored_second_late += 1;
        }
    }
    assert!(
        flagged_then_resolved > 0,
        "seed must exercise Flagged→Resolved"
    );
    assert!(
        ignored_second_late > 0,
        "seed must exercise the ignored duplicate late remit"
    );
}

/// 2.4: duplicate delivery. Both copies share the attempt number, so the
/// stateless payer derives the identical remittance twice; the biller applies
/// at most one adjudication and the surplus copy is logged, never lost.
#[test]
fn duplicate_deliveries_apply_at_most_once_and_are_logged() {
    let input: Vec<String> = (0..30).map(simple_claim).collect();
    let path = write_input("duplicates.jsonl", &input);
    let mut cfg = RunConfig::new(path, 42, 10.0);
    cfg.faults.duplicate_rate = 0.5;

    let output = run_sim_with(cfg);
    let duplicated = output.sim_truth.claims_with(FaultKind::Duplicate);
    assert!(
        !duplicated.is_empty(),
        "seed must actually inject duplicates"
    );
    assert!(output.ledger.quarantine.is_empty());

    for record in output.ledger.claims.values() {
        assert!(
            record.state.is_terminal(),
            "claim {} stuck",
            record.claim_id
        );
        let applied = record
            .history
            .iter()
            .filter(|e| matches!(e.event, ClaimEvent::RemittanceApplied { .. }))
            .count();
        assert!(applied <= 1, "claim {} double-counted", record.claim_id);
        if duplicated.contains(&record.claim_id) {
            assert_eq!(record.state, ClaimState::Resolved);
            assert_eq!(record.attempts, 1, "duplicates are not retries");
            let logged_dups = record
                .history
                .iter()
                .filter(|e| matches!(e.event, ClaimEvent::LateRemittance { .. }))
                .count();
            assert!(logged_dups >= 1, "claim {} dup not logged", record.claim_id);
        }
    }
}

/// 2.5: reordering. There is no mechanism to add — correlation is by
/// claim_id, never arrival order — so this row is a pure regression test.
/// Remittances demonstrably arrive out of submission order (mixed payer
/// latencies guarantee it), and every claim's ledger money still matches
/// what the pure payer function says *that* claim's payer owes it: no
/// cross-attachment possible.
#[test]
fn scrambled_arrival_order_never_misattaches_remittances() {
    let input: Vec<String> = (0..30).map(simple_claim).collect();
    let path = write_input("reorder.jsonl", &input);
    let seed = 42;
    let output = run_sim_with(RunConfig::new(path, seed, 10.0));

    // Arrival order really was scrambled relative to submission order.
    let submitted: Vec<&str> = output
        .ledger
        .event_log
        .iter()
        .filter(|e| matches!(e.event, ClaimEvent::Submitted { .. }))
        .map(|e| e.claim_id.0.as_str())
        .collect();
    let applied: Vec<&str> = output
        .ledger
        .event_log
        .iter()
        .filter(|e| matches!(e.event, ClaimEvent::RemittanceApplied { .. }))
        .map(|e| e.claim_id.0.as_str())
        .collect();
    assert_eq!(submitted.len(), applied.len());
    assert_ne!(
        submitted, applied,
        "seed must actually scramble arrival order"
    );

    // Closed loop: each claim's ledger adjudications equal the pure payer
    // function's answer for that exact claim — correlation was by id.
    let rng = RngFactory::new(seed);
    let payers = default_payer_configs();
    for record in output.ledger.claims.values() {
        assert_eq!(record.state, ClaimState::Resolved);
        let identity = record.identity.as_ref().expect("ingested");
        let submitted_claim = SubmittedClaim {
            claim_id: record.claim_id.clone(),
            payer_id: identity.payer_id,
            lines: record
                .lines
                .iter()
                .filter(|l| !l.do_not_bill)
                .map(|l| SubmittedLine {
                    service_line_id: l.service_line_id.clone(),
                    procedure_code: l.procedure_code.clone(),
                    units: l.units,
                    unit_charge: l.unit_charge,
                })
                .collect(),
        };
        let expected = adjudicate(&submitted_claim, &payers[&identity.payer_id], &rng);
        for expected_line in &expected.lines {
            let line = record
                .lines
                .iter()
                .find(|l| l.service_line_id == expected_line.service_line_id)
                .expect("line exists");
            let adj = line.adjudication.as_ref().expect("adjudicated");
            assert_eq!(
                adj.payer_paid, expected_line.payer_paid,
                "claim {}",
                record.claim_id
            );
            assert_eq!(adj.not_allowed, expected_line.not_allowed);
            assert_eq!(
                adj.coinsurance + adj.copay + adj.deductible,
                expected_line.patient_responsibility()
            );
        }
    }
}

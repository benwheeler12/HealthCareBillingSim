//! Class-2 (transport) fault rows, one test per row of the DESIGN.md fault
//! table. Sim-truth is the oracle: every assertion about "what really
//! happened" comes from the recorder, never from the ledger.

mod common;

use common::{run_sim_with, simple_claim, write_input};
use std::time::Duration;

use healthcare_billing_sim::RunConfig;
use healthcare_billing_sim::biller::policy::RetryPolicy;
use healthcare_billing_sim::ledger::events::ClaimEvent;
use healthcare_billing_sim::ledger::records::{ClaimState, FlagReason};
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
/// transition Flagged(RetriesExhausted) → Resolved (Decisions #6). A second
/// late copy on the now-Resolved claim is ignored idempotency (Decisions #5).
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

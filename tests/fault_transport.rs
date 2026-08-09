//! Class-2 (transport) fault rows, one test per row of the DESIGN.md fault
//! table. Sim-truth is the oracle: every assertion about "what really
//! happened" comes from the recorder, never from the ledger.

mod common;

use common::{run_sim_with, simple_claim, write_input};
use healthcare_billing_sim::RunConfig;
use healthcare_billing_sim::ledger::records::ClaimState;
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

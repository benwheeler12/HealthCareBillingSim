//! Class-4 rows — reclassified from faults to architectural invariants
//! (DESIGN.md): properties the architecture makes true by construction,
//! each held down by one proving test. Plus the capstone: every fault at
//! once, and the two global guarantees still hold.

mod common;

use std::time::Duration;

use common::{run_sim_with, simple_claim, write_input};
use healthcare_billing_sim::RunConfig;
use healthcare_billing_sim::domain::PayerId;
use healthcare_billing_sim::ledger::records::ClaimState;

/// No claim lost under burst ingest: bounded channels + backpressure, not
/// buffering heroics. 300 claims at 1000/s virtual — every one becomes a
/// ledger row and reaches a terminal state.
#[test]
fn burst_ingest_loses_nothing() {
    let input: Vec<String> = (0..300).map(simple_claim).collect();
    let path = write_input("burst.jsonl", &input);
    let output = run_sim_with(RunConfig::new(path, 42, 1000.0));

    assert_eq!(
        output.ledger.claims.len(),
        300,
        "every claim is a ledger row"
    );
    for record in output.ledger.claims.values() {
        assert_eq!(
            record.state,
            ClaimState::Resolved,
            "claim {}",
            record.claim_id
        );
    }
}

/// A slow payer cannot delay other payers' claims — true by construction in
/// task-per-claim (no shared queue ⇒ no head-of-line blocking). Anthem made
/// 100× slower; medicare's response latency stays inside medicare's own
/// honest bounds.
#[test]
fn slow_payer_does_not_delay_other_payers() {
    let input: Vec<String> = (0..30).map(simple_claim).collect();
    let path = write_input("slow_payer.jsonl", &input);
    let mut cfg = RunConfig::new(path, 42, 10.0);
    {
        let anthem = cfg.payers.get_mut(&PayerId::Anthem).expect("config");
        anthem.min_response_time_secs *= 100.0; // 500s
        anthem.max_response_time_secs *= 100.0; // 3000s
    }
    cfg.policy.timeout = Duration::from_secs(5000); // let anthem answer honestly

    let output = run_sim_with(cfg);
    for record in output.ledger.claims.values() {
        assert_eq!(
            record.state,
            ClaimState::Resolved,
            "claim {}",
            record.claim_id
        );
        let identity = record.identity.as_ref().expect("ingested");
        let latency = record
            .resolved_at
            .expect("resolved")
            .saturating_since(record.first_submitted_at.expect("submitted"));
        match identity.payer_id {
            PayerId::Medicare => assert!(
                latency <= Duration::from_secs(6),
                "medicare claim {} took {latency:?} — head-of-line blocked?",
                record.claim_id
            ),
            PayerId::Anthem => assert!(
                latency >= Duration::from_secs(500),
                "anthem claim {} was supposed to be slow",
                record.claim_id
            ),
            PayerId::UnitedHealthGroup => {}
        }
    }
}

/// Capstone: the whole fault table at once. The two global guarantees hold
/// under any seeded fault schedule: every ingested claim reaches a terminal
/// state (none lost, stuck, or double-counted), and the run is
/// outcome-deterministic given the seed.
#[test]
fn all_faults_at_once_every_claim_terminal_and_deterministic() {
    let input: Vec<String> = (0..60).map(simple_claim).collect();
    let path = write_input("chaos.jsonl", &input);
    let chaos = |seed: u64| {
        let mut cfg = RunConfig::new(path.clone(), seed, 10.0);
        cfg.faults.forward_drop_rate = 0.15;
        cfg.faults.return_drop_rate = 0.15;
        cfg.faults.duplicate_rate = 0.15;
        cfg.faults.extra_delay_rate = 0.2;
        cfg.faults.max_extra_delay_secs = 400.0;
        cfg.faults.dishonest_adjudication_rate = 0.1;
        cfg.faults.corrupt_claim_id_rate = 0.1;
        cfg.faults.line_drop_rate = 0.1;
        cfg.faults.corrupt_remittance_rate = 0.1;
        run_sim_with(cfg)
    };

    let output = chaos(42);
    assert_eq!(output.ledger.claims.len(), 60);
    assert!(
        output.sim_truth.injected.len() > 30,
        "chaos must actually be chaotic"
    );
    for record in output.ledger.claims.values() {
        assert!(
            record.state.is_terminal(),
            "claim {} stuck",
            record.claim_id
        );
        let applications = record
            .history
            .iter()
            .filter(|e| {
                matches!(
                    e.event,
                    healthcare_billing_sim::ledger::events::ClaimEvent::Resolved
                )
            })
            .count();
        assert!(
            applications <= 1,
            "claim {} double-counted",
            record.claim_id
        );
    }

    // Outcome-determinism: identical terminal states and money across runs.
    let rerun = chaos(42);
    let fingerprint = |o: &healthcare_billing_sim::RunOutput| {
        let mut v: Vec<String> = o
            .ledger
            .claims
            .values()
            .map(|r| format!("{} {:?} {:?}", r.claim_id, r.state, r.lines))
            .collect();
        v.sort();
        v
    };
    assert_eq!(fingerprint(&output), fingerprint(&rerun));
    assert_ne!(fingerprint(&output), fingerprint(&chaos(7)), "seeds differ");
}

/// Decisions #23: computed time on the multi-thread scheduler. Not just
/// outcomes — the *entire finalized event log*, timestamps and order
/// included, must be identical across runs no matter how the worker threads
/// interleave. This is the test that replaces the paused-clock spike as the
/// foundation the design leans on.
#[test]
fn full_event_log_is_identical_across_parallel_runs() {
    let input: Vec<String> = (0..80).map(simple_claim).collect();
    let path = write_input("parallel_determinism.jsonl", &input);
    let run_once = || {
        let mut cfg = RunConfig::new(path.clone(), 42, 10.0);
        cfg.faults.forward_drop_rate = 0.1;
        cfg.faults.duplicate_rate = 0.1;
        cfg.faults.extra_delay_rate = 0.2;
        cfg.faults.max_extra_delay_secs = 300.0;
        run_sim_with(cfg)
    };
    let log = |o: &healthcare_billing_sim::RunOutput| -> Vec<String> {
        o.ledger
            .event_log
            .iter()
            .map(|e| format!("{:?} {} {:?}", e.at, e.claim_id, e.event))
            .collect()
    };
    let (a, b) = (run_once(), run_once());
    assert!(!log(&a).is_empty());
    assert_eq!(log(&a), log(&b), "event logs diverged across parallel runs");
    assert_eq!(a.finished_at, b.finished_at);
    assert_eq!(a.intake_finished_at, b.intake_finished_at);
}

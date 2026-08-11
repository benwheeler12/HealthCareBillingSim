//! The product path end to end: a `Generated` claim source streamed straight
//! into the simulation — no file anywhere — must satisfy the same guarantees
//! as file ingest: every document accounted for, every claim terminal, and
//! outcomes an exact function of the configuration.

mod common;

use healthcare_billing_sim::claimgen::{self, GenConfig};
use healthcare_billing_sim::ledger::records::ClaimState;
use healthcare_billing_sim::simconfig::{Preset, SimConfig};

use common::run_sim_with;

fn small_sim(seed: u64, count: usize, malformed_rate: f64) -> SimConfig {
    let mut cfg = SimConfig {
        seed,
        // Fast intake so the run drains in milliseconds of wall time.
        rate_per_sec: 10.0,
        generator: GenConfig {
            count,
            malformed_rate,
            ..GenConfig::default()
        },
        ..SimConfig::default()
    };
    cfg.apply_preset(Preset::Messy);
    cfg
}

/// Every generated document lands in the ledger as a claim record or a
/// recorded duplicate — nothing silently dropped between the generator and
/// the books — and every claim reaches a terminal state.
#[test]
fn generated_run_accounts_for_every_document_and_drains() {
    let cfg = small_sim(42, 500, 0.05);
    let output = run_sim_with(cfg.to_run_config());

    let duplicates = output
        .ledger
        .event_log
        .iter()
        .filter(|e| {
            matches!(
                e.event,
                healthcare_billing_sim::ledger::events::ClaimEvent::DuplicateIngest { .. }
            )
        })
        .count();
    assert_eq!(
        output.ledger.claims.len() + duplicates,
        cfg.generator.count,
        "claims + duplicate-ingest events must equal the generated count"
    );
    for (id, record) in &output.ledger.claims {
        assert!(
            matches!(
                record.state,
                ClaimState::Resolved | ClaimState::Rejected { .. } | ClaimState::Flagged { .. }
            ),
            "claim {id} finished non-terminal: {:?}",
            record.state
        );
    }
    // The malformed mix must actually produce ingest rejections.
    assert!(
        output
            .ledger
            .claims
            .values()
            .any(|record| matches!(record.state, ClaimState::Rejected { .. })),
        "a 5% malformed stream must reject at ingest"
    );
}

/// Same SimConfig, same books: the generated source keeps the byte-identical
/// determinism guarantee file ingest had.
#[test]
fn generated_runs_are_deterministic_per_config() {
    let a = run_sim_with(small_sim(7, 300, 0.02).to_run_config());
    let b = run_sim_with(small_sim(7, 300, 0.02).to_run_config());
    assert_eq!(a.ledger.event_log.len(), b.ledger.event_log.len());
    for (x, y) in a.ledger.event_log.iter().zip(b.ledger.event_log.iter()) {
        assert_eq!(x.at, y.at);
        assert_eq!(x.claim_id, y.claim_id);
    }
    assert_eq!(a.finished_at, b.finished_at);

    let c = run_sim_with(small_sim(8, 300, 0.02).to_run_config());
    assert_ne!(
        a.finished_at, c.finished_at,
        "a different master seed must reroll the world"
    );
}

/// A pinned generator seed fixes the claim population while the master seed
/// rerolls adjudication and transport luck.
#[test]
fn pinned_generator_seed_fixes_the_population() {
    let mut cfg_a = small_sim(1, 200, 0.0);
    cfg_a.generator.seed = Some(99);
    let mut cfg_b = small_sim(2, 200, 0.0);
    cfg_b.generator.seed = Some(99);

    let lines_a: Vec<String> = claimgen::stream(&cfg_a.generator, cfg_a.seed).collect();
    let lines_b: Vec<String> = claimgen::stream(&cfg_b.generator, cfg_b.seed).collect();
    assert_eq!(lines_a, lines_b, "same population under different masters");

    let a = run_sim_with(cfg_a.to_run_config());
    let b = run_sim_with(cfg_b.to_run_config());
    let ids = |o: &healthcare_billing_sim::RunOutput| {
        let mut ids: Vec<String> = o.ledger.claims.keys().map(|id| id.to_string()).collect();
        ids.sort();
        ids
    };
    assert_eq!(ids(&a), ids(&b), "identical claim ids in both ledgers");
    assert_ne!(
        a.finished_at, b.finished_at,
        "different master seeds must still vary the simulation"
    );
}

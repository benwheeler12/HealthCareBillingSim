//! Build-step-1 spike, kept as a permanent regression test (Decisions log #12).
//!
//! Proves the three properties the entire design leans on before anything is
//! built on top of them:
//!
//! 1. Under `start_paused(true)`, hour-scale virtual sleeps across 1,000
//!    concurrent tasks complete in milliseconds of wall time (auto-advance).
//! 2. Virtual time advances to the *next armed timer*, not the sum of sleeps:
//!    total virtual elapsed equals the single longest per-task race, exactly.
//! 3. Outcomes are deterministic across runs given a seed.
//!
//! Findings recorded in DESIGN.MD: `start_paused` needs tokio's `test-util`
//! feature and a current-thread runtime, and a `select!` between two timers
//! that fire at the same virtual instant needs `biased;` to stay deterministic.

use std::collections::BTreeMap;
use std::time::Duration;
use tokio::task::JoinSet;
use tokio::time::sleep;

const TASKS: u64 = 1000;
const TIMEOUT: Duration = Duration::from_secs(1800);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Responded { latency_secs: u64 },
    TimedOut,
}

/// Stable seeded draw (splitmix64) — no shared RNG, per the design's RNG rules.
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

fn latency_secs(seed: u64, task: u64) -> u64 {
    splitmix64(seed ^ splitmix64(task)) % 3600
}

/// One full scenario on a fresh paused runtime: every task virtually sleeps a
/// seeded latency raced against a fixed timeout, exactly like a claim task
/// racing a payer response against its retry deadline.
fn run_scenario(seed: u64) -> (BTreeMap<u64, Outcome>, Duration, Duration) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .expect("runtime");
    let wall_start = std::time::Instant::now();
    let (outcomes, virtual_elapsed) = rt.block_on(async {
        let virtual_start = tokio::time::Instant::now();
        let mut set = JoinSet::new();
        for task in 0..TASKS {
            let latency = Duration::from_secs(latency_secs(seed, task));
            set.spawn(async move {
                let outcome = tokio::select! {
                    // At an identical virtual instant both branches are ready and
                    // unbiased select! picks randomly — biased keeps runs identical.
                    biased;
                    _ = sleep(latency) => Outcome::Responded { latency_secs: latency.as_secs() },
                    _ = sleep(TIMEOUT) => Outcome::TimedOut,
                };
                (task, outcome)
            });
        }
        let mut outcomes = BTreeMap::new();
        while let Some(joined) = set.join_next().await {
            let (task, outcome) = joined.expect("task panicked");
            outcomes.insert(task, outcome);
        }
        (outcomes, virtual_start.elapsed())
    });
    (outcomes, virtual_elapsed, wall_start.elapsed())
}

#[test]
fn hour_scale_virtual_sleeps_finish_in_ms_wall_time() {
    let (outcomes, virtual_elapsed, wall_elapsed) = run_scenario(42);

    assert_eq!(outcomes.len() as u64, TASKS);
    // Generous CI bound; locally this is single-digit milliseconds.
    assert!(
        wall_elapsed < Duration::from_secs(5),
        "expected ms-scale wall time, got {wall_elapsed:?}"
    );
    // Auto-advance jumps to the next armed timer: total virtual elapsed is the
    // longest single race, not the sum of 1,000 sleeps.
    let expected_virtual = (0..TASKS)
        .map(|task| latency_secs(42, task).min(TIMEOUT.as_secs()))
        .max()
        .unwrap();
    assert_eq!(virtual_elapsed, Duration::from_secs(expected_virtual));
}

#[test]
fn outcomes_are_deterministic_across_runs_given_a_seed() {
    let (first, _, _) = run_scenario(7);
    let (second, _, _) = run_scenario(7);
    assert_eq!(first, second);

    let timeouts = first.values().filter(|o| **o == Outcome::TimedOut).count();
    assert!(
        timeouts > 0 && timeouts < TASKS as usize,
        "seed should exercise both select! branches, got {timeouts} timeouts"
    );

    let (other_seed, _, _) = run_scenario(8);
    assert_ne!(first, other_seed, "different seeds should diverge");
}

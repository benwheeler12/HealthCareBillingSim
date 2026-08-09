# Healthcare Billing Lifecycle Simulation

Rust + Tokio simulation of the healthcare billing lifecycle — biller ↔
clearinghouse ↔ insurance payers — built for the Brace Health take-home.
The converged design lives in [DESIGN.MD](DESIGN.MD) (spec + fault table);
reasoning traces behind the decisions are in
[docs/RATIONALE.MD](docs/RATIONALE.MD).

**Status: fault table complete.** The full burn-down has landed, one commit
per fault row (git history reads as the fault table, in order): ingest faults
(1.1–1.4), transport faults (2.1–2.6: drops both directions, emergent
timeouts with retry/backoff, late-remittance recovery, duplicates,
reordering, retry exhaustion), semantic faults (3.1–3.5: dishonest payers,
corrupt claim ids, partial remittances, full denials, garbage-as-silence),
and the class-4 architectural invariants (burst ingest loses nothing; a slow
payer cannot delay other payers). The capstone test runs every fault at once
and asserts the global guarantee: every ingested claim reaches a terminal
state — none lost, stuck, or double-counted — deterministically per seed.
Next: the report suite (AR aging, payer scorecard) and CLI fault-profile
configuration.

## Run

```sh
cargo run -- data/sample_claims.jsonl            # required arg: input file
cargo run -- data/sample_claims.jsonl --seed 7 --rate 2.0
```

The input file contains one PayerClaim JSON object per line (schema in
`docs/TAKE_HOME_PROMPT.MD`). Malformed lines become `Rejected` ledger rows,
never silent drops. `RUST_LOG=healthcare_billing_sim=debug` for more detail.

## Test / lint

```sh
cargo test
cargo clippy --all-targets -- -D warnings
```

Notable tests:

- `tests/virtual_time.rs` — the build-step-1 spike, kept as a regression
  proof: 1,000 tasks with hour-scale virtual sleeps finish in milliseconds of
  wall time, deterministically per seed.
- `tests/happy_path.rs` — end-to-end: every ingested claim reaches a terminal
  state, per-line money reconciles exactly in integer cents, same seed ⇒ same
  outcomes.

## Design in one paragraph

The clearinghouse + payers are a deliberately unreliable, seedable,
**memoryless** adversary (pure functions of seed + config); the biller is the
system under test and holds all the state. All time is virtual
(`tokio::time` paused with auto-advance — hence a current-thread runtime and
the `test-util` feature in the main dependency set), so hour-scale payer
latencies simulate in milliseconds and timeouts are *emergent* (payer latency
vs biller policy), never injected as events. Money is integer cents; the
per-line reconciliation equation
`billed == payer_paid + coinsurance + copay + deductible + not_allowed`
is exact. Every random decision draws from an RNG stream keyed by
(seed, claim_id, decision_point[, attempt]) — transport fates include the
attempt (retries get fresh luck), adjudication content does not (re-delivery
reproduces the identical remittance). The run is **outcome-deterministic**
given a seed. Key decisions and deviations are logged in DESIGN.MD's
Decisions section.

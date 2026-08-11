# Healthcare Billing Lifecycle Simulation

Rust + Tokio simulation of the healthcare billing lifecycle — biller ↔
clearinghouse ↔ insurance payers — built for the Brace Health take-home.

The clearinghouse + payers are a deliberately unreliable, seedable, memoryless
adversary; the biller is the system under test. Correctness has one definition:
**every ingested claim reaches a correct terminal state** — Resolved, Rejected,
or Flagged — with no claim lost, stuck, or double-counted, under any seeded
fault schedule.

## Quickstart

```sh
# Run the simulation on a default configuration
cargo run -- data/sample_claims_10k.jsonl

# Roll your own input at any size:
cargo run --bin generate-claims -- 10000 --seed 7 --malformed-rate 0.02 --out claims.jsonl

# Scale test — the exact 1M-record input BENCHMARKS.MD was measured on
# (~1 min wall, ~10 GiB peak on a 4-core machine):
cargo run --release --bin generate-claims -- 1000000 --seed 7 --malformed-rate 0.02 --out claims_1m.jsonl
cargo run --release -- claims_1m.jsonl

# Tests (56) and lints:
cargo test
cargo clippy --all-targets -- -D warnings
```

The input file is the only required argument: one PayerClaim JSON object per
line (schema in `docs/TAKE_HOME_PROMPT.MD`).

## The TUI

On a real terminal, a run opens an interactive UI: four panes, money views
first — **A/R Aging** (the books as of a scrubbable virtual day: outcome bars,
colored aging tables, per-provider), **Payer Scorecard** (payers graded A–F,
with the configured personality next to what the scorecard rediscovered from
remittance data alone), **Provider Insights** (the open claims a human would
have to work at run end, with plain-English statuses and per-claim audit
trails), and **Timeline** (per-day claim flow and the in-flight backlog
draining to zero — the correctness guarantee as a picture).

The whole key grammar: **←/→** move between panes, **↑/↓** select, **Enter**
steps down a layer, **Esc** steps back up, **`?`** shows the full keyboard map,
and **Ctrl-C** quits — printing the plain report so a record stays in your
scrollback. When stdout is not a terminal (pipes, CI) or with `--no-tui`, you
get the plain sequential report instead; that path also prints the sim-truth
diagnostic — the god's-eye view of injected faults.

## Configuration

Every run starts by printing the full parameter set it actually used.
Layers, later wins: **defaults → `--preset` → `--fault-profile` file →
individual flags** (see `cargo run -- --help` for the full set).

- `--preset honest|messy|chaos` — defaults to `messy` (drops, duplicates,
  delays, and per-payer route personalities); `honest` is the lossless
  baseline; `chaos` is everything at once.
- `--fault-profile FILE` — scenario JSON for precise control, including
  per-payer fault profiles (`data/demo_scenario.json` is a template).
- `--seed` reproduces outcomes exactly; `--rate` is claims per *virtual*
  second — the default spreads the 10k sample across ~9.5 virtual months so
  receivables genuinely age.

## Architecture in one screen

```
input file ──▶ ingest (parallel validate, computed arrival times, dedup)
                 │ Rejected rows            │ valid claims
                 ▼                          ▼
             ledger fold ◀──events── claim task ×N (one per claim_id,
             (single-consumer         │        ▲     in parallel across workers)
              mpsc, folds +           │ transact(submission) → Transaction:
              raw event log)          ▼        │  every arrival, with times
                                  clearinghouse (sync Transactor call —
                                  payer adjudication executes on the
                                  calling task's thread; stateless, seeded)
                                      │
              quarantine clerk ◀──────┘ strays        sim-truth recorder
              (uncorrelatable deliveries)             (ground truth oracle)
```

The properties that make it work — each one section in
[DESIGN.MD](DESIGN.MD):

- **Structural shutdown**: claim tasks own the whole lifecycle; task completion
  == terminal state, so input exhausted + tasks drained ⇒ reports ⇒ exit. No
  shutdown signals anywhere.
- **Virtual time is computed, not slept**: durations cross the biller ↔ sim
  boundary as data, timeouts are arithmetic, silence is the failure — and
  because nothing sleeps, claim tasks run in true parallel with a
  byte-identical event log at any thread count.
- **Outcome-deterministic given a seed**: no shared RNG; every decision draws
  from a keyed stream. Retries reroll transport luck; re-delivered claims
  reproduce identical remittances (idempotency by derivation).
- **Money is integer cents**, and the per-line equation
  `billed == paid + coinsurance + copay + deductible + not_allowed` (exact) is
  simultaneously the honest payer's constraint, the biller's semantic-fault
  detector, and a test invariant.
- **Two ledgers**: what remittances revealed vs. what the adversary actually
  did. The gap is the biller's blind spot; sim-truth is the oracle in every
  fault test.

## Verification and performance

Seven mechanisms (validation, timeout + bounded retry, idempotency,
correlation-by-ID, exact reconciliation, garbage == silence, backpressure)
cover the ~20-row fault table in DESIGN.MD; each fault row landed as one
commit with a seeded regression test, so `git log --oneline` reads as the
fault table. Invariant tests assert every-claim-terminal under full chaos,
slow-payer isolation, and cross-run determinism. Benchmarked at 1M records:
**2.1× end-to-end** over the previous paused-clock architecture, with
byte-identical reports — method and numbers in [BENCHMARKS.MD](BENCHMARKS.MD).

## Docs

Reading order for reviewers:

1. This README (2 min) — then run the TUI.
2. [DESIGN.MD](DESIGN.MD) (10 min) — the design as built: architecture, fault
   table, ledger schema, litigated decisions.
3. [docs/RATIONALE.MD](docs/RATIONALE.MD) — rejected alternatives, design
   history, honest weaknesses. [BENCHMARKS.MD](BENCHMARKS.MD) — the 1M-record
   measurement.

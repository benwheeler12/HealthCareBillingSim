# Healthcare Billing Lifecycle Simulation

Rust + Tokio simulation of the healthcare billing lifecycle — biller ↔
clearinghouse ↔ insurance payers — originally built for the Brace Health
take-home, since restructured into a fully interactive instrument.

The clearinghouse + payers are a deliberately unreliable, seedable, memoryless
adversary; the biller is the system under test. Correctness has one definition:
**every ingested claim reaches a correct terminal state** — Resolved, Rejected,
or Flagged — with no claim lost, stuck, or double-counted, under any seeded
fault schedule.

## Quickstart

```sh
# Opens the interactive configuration screen; Enter runs the simulation
cargo run --release

# Headless (pipes, CI, --no-tui): one run of the flag-built config,
# plain sequential reports — flags set the same knobs the screen edits
cargo run --release -- --no-tui --count 100000 --seed 7 --malformed-rate 0.02

# Tests and lints:
cargo test
cargo clippy --all-targets -- -D warnings
```

There is no input file. The program opens on a configuration form — claim
generation (count, malformed rate, payer-mix drift, seed), simulation (seed,
ingest rate), the clearinghouse fault profile, the biller's retry policy, and
all ten payer personalities. **↑/↓** move between fields, **←/→** adjust the
selected value, **Enter** on a payer row expands its personality and route
faults, **Enter** anywhere else starts the run. Claims are then generated in
memory and streamed straight into the simulation — handed to biller tasks as
they are minted, no file on disk anywhere.

## The dashboard

When the books drain, the run opens onto five panes, money views first —
**A/R Aging** (the books as of a scrubbable virtual day: outcome bars,
colored aging tables, per-provider), **Provider Insights** (a per-provider A/R
analysis in plain English — where the money is stuck, what the open claims are
doing, payer signals, the top chase items by risk, and recommended actions,
every figure computed from the ledger), **Timeline** (per-day claim flow and
the in-flight backlog draining to zero — the correctness guarantee as a
picture), **Payer Scorecard** (payers graded A–F, with the configured
personality next to what the scorecard rediscovered from remittance data
alone), and **Configuration** — the startup form again, live: adjust any
value and press Enter to kick off the next run without leaving the program.

The whole key grammar: **←/→** move between panes, **↑/↓** select, **Enter**
steps down a layer, **Esc** steps back up, **`?`** shows the full keyboard map,
and **Ctrl-C** quits — printing the plain report of the last completed run so
a record stays in your scrollback. When stdout is not a terminal (pipes, CI)
or with `--no-tui`, you get the plain sequential report instead; that path
also prints the sim-truth diagnostic — the god's-eye view of injected faults.

## Configuration

The configuration screen is the source of truth; CLI flags set its initial
values. Layers, later wins: **defaults → `--preset` → `--fault-profile` file →
individual flags** (see `cargo run -- --help` for the full set).

- `--preset honest|messy|chaos` — defaults to `messy` (drops, duplicates,
  delays, and per-payer route personalities); `honest` is the lossless
  baseline; `chaos` is everything at once. On the screen, presets rewrite the
  fault fields in place — any manual edit flips the label to `custom`.
- `--fault-profile FILE` — scenario JSON for precise control, including
  per-payer fault profiles (`data/demo_scenario.json` is a template).
- `--seed` reproduces outcomes exactly; the generator follows it unless
  `--gen-seed` pins the claim population separately, so you can hold the
  world fixed while rerolling fault luck. `--rate` is claims per *virtual*
  second — the default spreads 10k claims across ~9.5 virtual months so
  receivables genuinely age.

## Architecture in one screen

```
config form ──▶ claim generator (seeded, in memory, valid + malformed mix)
 (edit, Enter)   │ JSON lines, streamed as minted
                 ▼
               ingest (parallel validate, computed arrival times, dedup)
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
                 │ RunOutput
                 ▼
             dashboard (5 panes; pane 5 is the config form again ─▶ next run)
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

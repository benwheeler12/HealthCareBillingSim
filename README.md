# Healthcare Billing Lifecycle Simulation

Rust + Tokio simulation of the healthcare billing lifecycle — biller ↔
clearinghouse ↔ insurance payers — built for the Brace Health take-home.

The clearinghouse + payers are a deliberately unreliable, seedable,
**memoryless** adversary (pure functions of seed + config). The biller is the
system under test and holds all the state. Correctness has one definition:
**every ingested claim reaches a correct terminal state** — Resolved,
Rejected, or Flagged — with no claim lost, stuck, or double-counted, under
any seeded fault schedule. The fault table in [DESIGN.MD](DESIGN.MD) is the
spec, the test matrix, and (deliberately) the git history.

## Quickstart

```sh
# The default run already has weather in it — drops, duplicates, delays,
# and per-payer route personalities (preset 'messy'), payers answering in
# days-to-weeks, and the 5k sample spread across ~4.5 virtual months, so
# the AR aging report fills every bucket with a distinct profile per payer:
cargo run -- data/sample_claims_5k.jsonl

# The showcase: every fault class at once plus a denial-happy anthem:
cargo run -- data/sample_claims_5k.jsonl --preset chaos

# Lossless baseline, if you want a true happy path:
cargo run -- data/sample_claims.jsonl --preset honest

# Roll your own input at any size:
cargo run --bin generate-claims -- 10000 --seed 7 --malformed-rate 0.02 --out claims.jsonl

# Tests (55) and lints:
cargo test
cargo clippy --all-targets -- -D warnings
```

## CLI

On a real terminal, runs open an **interactive UI**: the reports are panes on
a static screen — ←/→ to move between Overview, Timeline, AR Aging, Patient,
Scorecard, Denials, Chase, and Diagnostic; ↑/↓ scrolls (or selects rows in
the chase list); **Enter on a chase-list row opens that claim's full audit
trail** — every event with virtual timestamps, straight from the
event-sourced ledger. The **Timeline pane charts the run**: a 2×2 grid of
small multiples — per-virtual-day rates for ingested, submitted, remitted,
and settled on one shared scale, so submitted riding above ingested reads
as retry traffic (the fault injection made visible) — over a chart of the
in-flight backlog, which rises through intake and drains to zero: the
correctness guarantee as a picture. All replayed from the retained event
log. A live status box pinned to the bottom shows
claims/resolved/rejected/flagged, the virtual clock, and wall time while the
run is in flight. `q` quits and prints the plain report so a record stays in
your scrollback. When stdout is not a terminal (pipes, CI) or with
`--no-tui`, output is the plain sequential report.

The input file is the single required argument (one PayerClaim JSON object
per line; schema in `docs/TAKE_HOME_PROMPT.MD`). Everything else is an
optional flag with a sensible default, and every run starts by printing the
full parameter set it actually used — seed, rate, retry policy, fault rates,
payer personalities, and where each value came from.

Configuration layers, later wins: **defaults → `--preset` →
`--fault-profile` file → individual flags.**

- `--preset honest|messy|chaos` — **defaults to `messy`** (drops, duplicates,
  delays: the assessment's "bake in a little unreliability"). `honest` is the
  lossless baseline; `chaos` is everything at once with a slow, denial-happy
  anthem.
- `--fault-profile FILE` — scenario JSON for precise control
  (`data/demo_scenario.json` is a template; unknown fields are rejected).
- Individual knobs: `--forward-drop-rate`, `--return-drop-rate`,
  `--duplicate-rate`, `--delay-rate`, `--max-delay-secs`,
  `--dishonest-rate`, `--line-drop-rate`, `--corrupt-id-rate`,
  `--garbage-rate`, `--max-attempts`, `--timeout-secs`, `--backoff-secs`.
- `--seed` reproduces outcomes exactly; `--rate` is claims per *virtual*
  second — the default (0.0004, one claim every ~42 virtual minutes) spreads
  the 5k sample across ~4.5 virtual months so receivables genuinely age;
  `--chase` sizes the chase list.
- Scenario files and presets can set **per-payer fault profiles** (a payer
  entry's `faults` section) — different clearinghouse routes fail
  differently, which is what gives each payer its own aging profile.
- Long runs show a live progress line (claims/resolved/flagged + the virtual
  clock) on stderr; disable with `--no-progress`. Colors follow the terminal
  and `NO_COLOR`; force off with `--no-color`. Reports go to stdout, logs to
  stderr (`RUST_LOG=healthcare_billing_sim=debug` for the per-claim story).
- `cargo run -- --help` is grouped by Simulation / Fault injection / Retry
  policy / Output.

## Architecture in one screen

```
input file ──▶ ingest (validate, rate-limit, dedup)
                 │ Rejected rows            │ valid claims
                 ▼                          ▼
             ledger fold ◀──events── claim task ×N (one per claim_id)
             (single-consumer         │ submit    ▲ remittance
              mpsc, folds +           ▼           │
              raw event log)      clearinghouse ──┴─▶ remittance dispatcher
                                      │  ▲            (correlate by claim_id;
                                      ▼  │             quarantine unknowns)
                                payer task per delivery
                                (stateless, seeded)          sim-truth recorder
                                                             (ground truth oracle)
```

- **Claim tasks** own the whole lifecycle as linear async code: submit,
  `select!` response-vs-deadline, bounded retry with backoff, reconcile,
  emit terminal event, return. Task completion == terminal state, so
  **shutdown is structural**: input exhausted + tasks drained ⇒ channels
  close in dependency order ⇒ fold returns ⇒ reports print ⇒ exit. No
  shutdown signals anywhere.
- **Functional core, imperative shell**: every lifecycle decision is the pure
  `machine::next(state, event, claim, policy, now) → (state, actions)`;
  adjudication and reconciliation are pure functions. The async fns only move
  messages and sleep.
- **Two ledgers**: the biller's-knowledge ledger (only what remittances
  revealed) vs the sim-truth recorder (every fault actually injected). The
  gap is the biller's blind spot; sim-truth is the oracle in every fault test
  and powers the god's-eye diagnostic report.
- `sim/` and `biller/` never import each other — shared types live in
  `domain/`. One documented exception: the diagnostic report reads sim-truth,
  because comparing the views is its purpose (Decisions #16).

## Virtual time

All delays and timeouts run on tokio's paused clock with auto-advance: time
jumps straight to the next armed timer whenever every task is parked, so a
115-virtual-day run finishes in ~300ms of wall time. Timeouts are **not
messages** — silence is the failure. Latency is a payer property, the timeout
is a biller policy, and a "timeout fault" only ever *emerges* from their
interaction. The load-bearing assumptions (ms wall time, exact
next-timer advance, cross-run determinism) are pinned by
`tests/virtual_time.rs`, the build-step-1 spike kept as a permanent proof.

## Determinism

**Outcome-deterministic given a seed** (same claims → same terminal states →
same money) — not wall-clock- or attribution-deterministic. No shared RNG:
every decision draws from a stream keyed as
`hash(seed, claim_id, decision_point[, attempt])`. Transport fates include
the attempt (retries reroll their luck); adjudication content does **not**,
so a re-delivered claim reproduces the identical remittance — idempotency by
derivation instead of payer state, which is what makes the payers safely
stateless. The same discipline covers the *lies*: dishonest adjudication is
adjudication-family-keyed, so duplicates of a lie are consistent too.

## Money and reconciliation

Money is integer cents (`Money(i64)`), never floats; fractional-cent inputs
are schema violations. The per-line reconciliation equation

```
billed == payer_paid + coinsurance + copay + deductible + not_allowed   (exact)
```

is simultaneously the honest payer's construction constraint, the biller's
semantic-fault detector, and a test invariant. A fully denied claim that sums
correctly is **Resolved** — denial is a line-level financial outcome and a
report, not a lifecycle state.

## Fault tolerance

Seven mechanisms cover the ~20-row fault table: ingest validation,
timeout + bounded retry with backoff, idempotency/dedup, correlation-by-ID,
exact reconciliation, response validation (garbage == silence), and
backpressure. Each fault row landed as one commit with a seeded regression
test — `git log --oneline` reads as the fault table. Highlights:

- Drops on either hop are indistinguishable silence; the retry that recovers
  a dropped claim is also what *causes* duplicates when the truth was a
  dropped remittance — which is why idempotency is not optional.
- Late remittances can resolve an already-flagged claim (recorded
  transition); duplicates of a resolved claim are ignored-but-logged.
- Partial remittances accumulate across retries; partially answered claims
  keep aging rather than looking fresh.
- Head-of-line blocking is impossible by construction (task per claim, no
  shared queue) — held down by a test that makes anthem 100× slower and
  asserts medicare doesn't notice.
- The capstone test switches on *every* fault at once and asserts the global
  guarantee plus outcome-determinism.

## Reports (all pure functions over a ledger snapshot + now)

The A/R views (aging, chase list, days in A/R) report over a **snapshot
frozen at the moment intake ends** — a real A/R report is mid-flight by
nature; on the final books every young receivable has been deliberately
driven terminal, which would empty the young buckets by construction. The
summary and diagnostic still assert the terminal guarantee on the final
ledger, and the output labels both moments.

`AR aging by payer` (ages from **first** submission — retries never make a
stuck claim look fresh) · `patient responsibility aging` (ages from
adjudication, when it became patient debt) · `days in A/R` headline ·
`payer scorecard` — avg response, denial rate, paid/billed, derived from
remittance data alone; run the demo scenario and watch it rediscover
anthem's injected slow/denial-happy personality · `denial breakdown` by
(payer, reason) · `chase list` (outstanding desc, age desc) · `sim-truth
diagnostic` — the biller's view side by side with what the adversary
actually did.

Outstanding amounts are derived, never stored: a line is booked once
answered *and* balanced; anything else — unanswered, in dispute — is open
A/R at full billed value.

## Testing

- **Unit**: the pure core (state machine transitions, reconciliation,
  validation, money, RNG stream separation, adjudication balance).
- **Integration, per fault row**: enable one fault against a seeded run,
  assert the ledger consequence — with sim-truth as ground truth, so tests
  assert *why*, not just *what*.
- **Invariant/property**: every-claim-terminal under chaos, burst-ingest
  losslessness, slow-payer isolation, cross-run determinism, closed-loop
  scorecard rediscovery.
- The virtual-time spike is a permanent regression test.

## Monitoring

Structured `tracing` on stderr: per-claim spans (claim_id + payer), ingest
and rejection events, timeout/retry decisions, late/garbage/quarantine
warnings, end-of-run counts. The ledger also retains the raw event log —
live state for runtime monitoring, replayable history for audit — and every
claim record carries its full append-only event history.

## Design docs

- [DESIGN.MD](DESIGN.MD) — the converged design: architecture, fault table,
  ledger schema, decisions log (every deviation recorded with rationale).
- [docs/RATIONALE.MD](docs/RATIONALE.MD) — reasoning traces behind rejected
  alternatives, and honest weaknesses.

Named scope cuts (deliberate, defensible): duplicate-claim denials by
stateful payers (would require a claim-status-inquiry protocol — EDI
276/277 — to stay resolvable), clearinghouse misrouting, payer crash as
distinct from infinite latency. Adjudication amounts are plausible policy,
not contract pricing — the *shape* is right. Single process by assignment;
the seams (domain document shapes, ledger API, channel boundaries) are where
network transport and persistence would slot in.

# Brace Health billing simulation — project memory

Rust + Tokio simulation of the healthcare billing lifecycle (biller ↔ clearinghouse ↔
payers) for a take-home assessment. The full converged design lives in @DESIGN.md —
read it before proposing architecture changes. Reasoning traces behind rejected
alternatives + interview prep live in docs/RATIONALE.md (read on demand when a
design decision is questioned; deliberately not auto-loaded). The original prompt
is in docs/assessment.pdf (if present).

## Non-negotiable design rules

- The fault table in DESIGN.md is the spec AND the test matrix. Every fault row ships
  as one commit: enable fault → add biller mechanism → add seeded regression test.
- Money is integer cents. Never floats. Reconciliation is exact equality:
  billed == payer_paid + coinsurance + copay + deductible + not_allowed, per line.
- All time is virtual (`tokio::time` under paused/auto-advance). Never wall-clock
  sleeps. Timeouts are emergent (latency vs biller policy), never injected as events.
- Determinism: no shared RNG. Transport-fault draws keyed by
  (seed, claim_id, attempt, decision_point); adjudication content keyed by
  (seed, claim_id, decision_point) — no attempt component, so re-delivery reproduces
  identical remittances. We claim outcome-determinism only.
- `sim/` and `biller/` must never import each other — shared types live in `domain/`.
  Simulation ground truth never leaks into the ledger.
- Channels over locks. Ledger is a single-consumer mpsc fold; authoritative path is
  never a broadcast channel.
- Ledger stores raw facts only; every report/aggregate is a pure function of a
  snapshot. No stored "outstanding" fields.
- Every ingested claim must reach a terminal state (Resolved / Rejected / Flagged) —
  no claim lost, stuck, or double-counted, under any seed. Tests assert this globally.

## Rust style

- Functional core, imperative shell: task bodies do only I/O (channels, select!,
  timers) and call pure functions for every decision. The claim state machine is
  `fn next(state, event, policy) -> (state, Vec<Action>)` — no I/O, no time, no
  channels in it. Fault-table unit tests target `next()`, not the runtime.
- Early-return error cases; happy path stays unindented. `?` over nested match.
- Match arms are short expressions producing values — no side effects in arms;
  extract helpers. Match tuples instead of nesting.
- Compute-then-commit: derive immutably, mutate in one obvious place. No `&mut`
  threaded through layers; short-lived local `mut` only.
- Core returns events/values instead of logging; the shell emits them.
- Logging macros don't alter control flow — follow with explicit return/`?` if
  execution must stop.
- When finishing a fault-row commit, review the diff against this section.

## Workflow rules

- Record any deviation from DESIGN.md in its Decisions log with a rationale, in the
  same commit as the change.
- Prefer small commits named after fault-table rows (e.g. "2.4: remittance dedup").
- Keep README's design narrative in sync as features land; it is a graded deliverable.

## Commands

- Build: `cargo build`   Test: `cargo test`   Lint: `cargo clippy -- -D warnings`
- Run: `cargo run -- <claims_file> [--seed N] [--rate R] [--fault-profile path]`
  (CLI shape may evolve; keep `<input file>` as the single required positional arg —
  assessment requirement.)
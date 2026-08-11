# Computed-time experiment: benchmarks

Old (paused-clock, single-thread by constraint) vs new (computed-time,
multi-thread) on **1,000,000 records**, measuring whether escaping tokio's
`start_paused` current-thread constraint yields real wall-clock gains, and
how close multi-core scaling gets to linear.

## Verdict up front

- **Correctness parity is exact**: old and new binaries produce
  **byte-identical reports** on the 5k sample bundled at the time (since
  replaced by a 10k sample) for both the `messy` and `chaos` presets, all 56 tests pass, and a new invariant test pins that the
  full finalized event log (timestamps and order included) is identical
  across parallel runs at any worker count.
- **End-to-end speedup on this 4-core box: 2.1×** (158.7s → 76.1s median).
  Computed time alone — before any parallelism — is worth **1.29×**.
- **Scaling is real but strongly sub-linear** (4 cores → 2.1×, ~52%
  parallel efficiency). The reason is not the simulation: the claim
  lifecycles themselves finish in **under 2 seconds** at every thread count
  — they parallelize essentially perfectly behind ingest. The wall time is
  the serial data plumbing around them. Details below.

## Hardware profile

```
Linux 6.18.5-fc-v20 x86_64
Intel(R) Xeon(R) Processor @ 2.80GHz (virtualized)
4 CPUs · 1 socket · 4 cores · 1 thread/core (no SMT)
L1d 128 KiB ×4 · L2 4 MiB ×4 · L3 33 MiB shared
15 GiB RAM · no cgroup CPU quota
```

Note the ceiling: **4 physical cores**, so the scaling curve can only be
measured to 4 workers, and the box is a shared VM (one cold-start outlier
was observed and discarded, see Method).

> **Historical note:** these numbers were measured on the file-ingest CLI
> (`<sim> claims_1m.jsonl`). The CLI has since moved to in-memory claim
> generation — the equivalent run today is
> `cargo run --release -- --no-tui --count 1000000 --seed 42 --gen-seed 7
> --malformed-rate 0.02`; the file path survives library-only as
> `ClaimSource::File` for replaying a fixed dataset.

## Method

- Input: 1,000,000 generated claims (`generate-claims 1000000 --seed 7
  --malformed-rate 0.02`), 756 MB JSONL.
- Both binaries built `--release` from the same toolchain; run as
  `<sim> claims_1m.jsonl --seed 42 --no-color --no-progress --no-tui`
  with `RUST_LOG=off`, stdout to /dev/null (reports still computed and
  formatted — only terminal I/O elided).
- Measured with a Python wrapper: monotonic wall clock +
  `getrusage(RUSAGE_CHILDREN)` for CPU seconds and peak RSS.
- `--threads N` sets tokio worker threads; `RAYON_NUM_THREADS=N` pins the
  finalize pool to the same width, so each row is an honest N-core run.
- Warm page cache for all reported runs. The very first 1M run of the
  session (old, 277.5s, 96% CPU with 147s of system time) was a cold-start
  outlier — first-touch page faults on an 8.7 GiB heap plus cold file cache
  — and is excluded; three warm repeats of the old binary agree closely.

## How the measurements work

Three layers of instrumentation, each answering a different question:

**1. Process level — "how much, in total?"** A wrapper starts a monotonic
wall clock, runs the binary to completion, then reads
`getrusage(RUSAGE_CHILDREN)`: the kernel's own accounting of CPU seconds
(user + system) and peak resident memory for the finished child. Two
derived numbers do a lot of work here:

- **utilization = CPU ÷ wall.** A single-threaded process caps at 100%;
  188% on a 4-thread run says the process averaged fewer than 2 busy cores
  — parallelism is leaking somewhere, before you know where.
- **user vs system split.** ~65–90s of *system* time per run is the kernel
  cost of coordination — channel wakeups, task switches, page faults on a
  10 GiB heap. High sys time is the signature of orchestration overhead
  rather than useful compute.

**2. Phase level — "where did it go?"** `run()` has natural barriers, and
`SIM_PHASES=1` stamps a monotonic clock at each one: after `ingest()`
returns (last line read, validated, emitted, spawned), after the claim-task
JoinSet drains and the fold returns (every claim terminal, books folded),
and after `finalize()` (log sorted, snapshot rebuilt); the binary prints
the three deltas on stderr, and `print_reports` times itself the same way.
This is how "the simulation is not the bottleneck" became measurable: claim
tasks are spawned *as lines are ingested* and run concurrently on the other
workers, so by the time ingest finishes, almost all lifecycle work has
already happened in its shadow. The **drain** phase — everything between
"last claim spawned" and "every claim terminal, fold caught up" — is the
residual simulation work on the critical path, and it measures **1–2s at
every thread count**. If claim processing or the fold were the bottleneck,
drain is exactly where the time would pile up; it doesn't.

**3. Differential level — "prove the attribution."** Change one stage,
hold everything else fixed, re-measure: parallelizing JSON validation cut
the ingest phase 76s → ~40s (so parsing was most of ingest); deepening the
ledger channel 1024 → 64k cut several more seconds (so producers were
parking on the fold's buffer, not on the fold's speed); adding worker
threads moved ingest and finalize but never moved drain (so the lifecycle
work was never on the critical path to begin with). Each optimization
doubles as evidence for the phase attribution — the needle moves exactly
where the phase timings said the time was.

The same rigor applies to the baseline: the first 1M run of the session
measured 277.5s with an anomalous 147s of system time — a cold-start
outlier (first-touch faults on a fresh 8.7 GiB heap, cold file cache) —
and three warm repeats that agree within 8% replaced it. Deeper tools
(perf/flamegraphs, tokio-console, criterion) would refine the picture
per-function, but phase barriers + kernel accounting + differential runs
were enough to locate every second of this pipeline.

## Results (1M records)

| implementation | threads | wall (s) | CPU (s) | peak RSS | vs old |
|---|---|---|---|---|---|
| old (paused clock) | 1 (its max) | 149.8 / 158.7 / 162.1 → **median 158.7** | ~157 | 8.7 GiB | 1.00× |
| new (computed time) | 1 | 122.5 / 122.9 → **122.7** | ~145 | 10.1 GiB | **1.29×** |
| new | 2 | 90.3 / 90.8 → **90.5** | ~142 | 10.1 GiB | **1.75×** |
| new | 4 | 75.2 / 77.0 → **76.1** | ~143 | 10.1 GiB | **2.08×** |

Phase breakdown (new, medians of instrumented runs; `SIM_PHASES=1`):

| phase | t=1 | t=2 | t=4 | scaling 1→4 | nature |
|---|---|---|---|---|---|
| ingest (read + validate + emit + spawn) | 58.8s | 43.4s | 40.4s | 1.45× | JSON parse parallelized; ordered emit/spawn loop serial |
| drain (claim lifecycles after intake) | 1.9s | 1.4s | 1.7s | — | **the simulation itself**; overlaps ingest, parallel |
| finalize (log argsort + snapshot rebuild) | 40.9s | 23.9s | 17.8s | 2.30× | rayon index sort + per-claim parallel replay |
| reports | 2.7s | 2.9s | 2.7s | 1.0× | serial |
| (remainder: startup, file I/O tail, freeing a 10 GiB heap at exit) | ~18s | ~19s | ~13s | — | serial |

## Analysis

**Where the 1.29× single-thread win comes from.** Computed time deletes the
timer machinery entirely: 1M rate-limiter sleeps, ~1.4M retry/backoff/
timeout timers, the auto-advance bookkeeping on every park, and the
dispatcher's registration protocol (2M control messages + acks). Same
decisions, same books — minus the choreography.

**Where the parallelism went.** The intuitive picture — "1M independent
claims, N cores, N× faster" — is right about the claims and wrong about the
pipeline. The claim lifecycles (submit/retry/adjudicate/reconcile) are
~1–2s of wall at every thread count: they parallelize essentially
perfectly and hide completely behind ingest. What remains on the critical
path is serial plumbing:

1. **The ordered ingest loop** (~35–40s floor): one task must consume
   verdicts in input order to honor first-document-wins dedup, emit each
   Ingested event, and spawn each claim task. Parsing is fanned out to all
   cores (that took ingest from 76s to ~40s), but the ordered consumer —
   1M channel sends + 1M task spawns — is inherently sequential.
2. **The single-consumer ledger fold** (by design, Decisions #10): every
   one of ~6.5M events funnels through one task doing clone-heavy record
   updates. It keeps pace with today's producers (drain ≈ 1s after intake
   ends), but it bounds any future producer-side speedup — Amdahl's serial
   term waiting to bind.
3. **Finalize + reports + teardown** (~33s at t=4): partially parallelized
   (sort 2.3× on 4 cores; memory bandwidth limits it — these are fat,
   String-heavy events), reports serial, and ~10s simply freeing a 10 GiB
   heap at exit.

Coordination cost is visible in the CPU column: ~65–90s of *system* time
per run — mpsc wakeups, task spawning, allocator traffic on 6.5M events ×
2 clones (log + history). That overhead is why t=1 utilization reads >100%
and why adding threads buys less than their share.

**Is the linear-cores assumption wrong, then?** Not for the simulation —
for the pipeline. The assumption holds asymptotically for the part people
picture (independent claim lifecycles), and this codebase proves it: the
drain phase is flat and tiny at every width. But end-to-end wall time is
governed by Amdahl's law over the serial stages, and with the current
architecture the curve flattens toward a ~55–60s floor on any core count
(projected ~2.7× ceiling). Linear end-to-end scaling would require
sharding the *whole* pipeline per claim-hash — per-shard ingest cursors,
per-shard folds merged at the end, batched emission, arena-allocated
events, and parallel report computation. All of it is possible (claims are
independent — the same property this experiment already exploits), and all
of it is more machinery. It is also worth naming the workload: at ~40 µs
of real compute per claim, this is bookkeeping-bound; if adjudication were
priced like real contract pricing (heavier per-claim compute), the
parallel fraction — and the payoff from cores — would grow accordingly
(Gustafson's law working in our favor).

## What changed vs mainline (summary)

Time crosses the biller/sim boundary as data: `Submission.sent_at` +
`Transaction`/`Arrival` declared durations; sync `Transactor` trait in
`domain`; per-claim event loop replaces `select!`; dispatcher → quarantine
clerk; explicit-timestamp `LedgerTx`; `finalize` restores global order and
rebuilds the intake snapshot per claim in parallel; parallel batched
ingest validation; `--threads` flag. `machine::next`, `payer.rs`,
`reconcile.rs`, all reports, and the TUI are untouched. Full narrative in
DESIGN.MD Decisions #23.

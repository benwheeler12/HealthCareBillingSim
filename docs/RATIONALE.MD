# Rationale & Interview Notes

Companion to DESIGN.md. Not auto-loaded — read on demand. Two purposes:
(1) full reasoning traces behind decisions, so settled questions aren't relitigated
without the context that settled them; (2) prep for the live discussion.

## Rejected alternatives, with the reasoning that killed them

**Hot-swappable plugin interfaces (original concept).** Rejected as primary framing:
risk of spending the budget on abstraction machinery while shipping thin core
behavior. Replaced with "seams, not SDK": clean boundaries with one solid default
implementation. The pluggable thing that survived is the biller's *resilience
policy* (retry/timeout config), which is honest — we can name the axes a second
implementation would vary on.

**Timeout as an explicit message from the clearinghouse.** Rejected because a
timeout is not an event you receive — silence IS the failure. A biller that
"handles timeouts" by parsing a timeout message demonstrates nothing; the hard
problem is detecting absence under concurrency. This forced real virtual time.
Corollary framing: latency is a payer property, timeout is a biller policy, and a
timeout is the emergent interaction of the two — never injected directly.

**Hand-rolled virtual clock.** Deferred, not rejected. tokio::time::pause() +
auto-advance implements the crux invariant for us: time advances only to the next
armed timer and only when all tasks are parked. Getting that invariant wrong in a
bespoke clock means time races past in-flight messages => spurious timeouts. If the
spike (build step 1) surprises us, revisit; the custom clock is more inspectable
but carries the correctness burden.

**Long-lived payer actors (v1 architecture).** Rejected once we noticed the only
state they held (dedup memory) could be replaced by determinism: adjudication as a
pure function keyed by (seed, claim_id) means re-delivery reproduces the identical
remittance with zero storage. Idempotency via derivation, not memory. The load-
bearing subtlety: transport-fault draws include `attempt` in the key (retries get
fresh fates); adjudication content does NOT (re-delivery is reproducible). Mixing
these up either makes retries pointless or makes duplicates conflict.

**Payer seen-set with duplicate-claim denials (v3, reverted).** Designed fully,
then cut. The trap: under fault 2.2 (payer adjudicated, remittance dropped), the
biller's retry hits the seen-set and gets a bare duplicate-denial carrying no
amounts — forever. The claim becomes unresolvable without a claim-status-inquiry
protocol (EDI 276/277), a second transaction type. Real-world grounding: CARC 18.
The fix (attach re-derived remittance to the dup response) worked but added a
biller obligation — distinguishing protocol-level responses from financial-level
responses — for a fault class whose biller mechanism (remittance dedup) was already
fully exercised. Cut on grit-budget grounds. Status inquiry is the named natural
extension if interviewers add requirements.

**Per-claim private reply channels without a dispatcher.** Rejected because it
makes correlation-by-ID untestable *by construction*: responses physically cannot
misroute, so fault 3.2 (unknown claim_id) is unreachable and the "correlate by ID,
never by order" mechanism is never exercised. The dispatcher makes correlation a
real component with a routing map and a quarantine path.

**Broadcast channel as the ledger's primary path.** Rejected: Tokio broadcast
drops messages on lagging receivers — disqualifying for the system of record.
Pattern: lossless mpsc into the fold task; the fold may re-publish to a broadcast
tap for best-effort observers.

**Slow-payer pileup as a fault (4.2).** Reclassified, not deleted: in task-per-
claim there is no shared queue, so head-of-line blocking is impossible by
construction. It's an architectural invariant with one proving test (anthem 100x
slower; medicare latency unaffected). "A fault made impossible beats a fault
handled."

**Fold-at-end pure event log.** Considered (maximally simple, beautifully
testable); chose fold-as-you-go WITH retained event log — live state enables
runtime monitoring, retained log keeps replay/audit testability. Both, cheaply.

## Framings worth saying out loud in the discussion

- The fault table is the spec: "a correct biller" is defined by the adversary.
  Test-first thinking applied to system design.
- Asymmetry by design: the adversary is memoryless and reproducible; the party
  under test holds all the state.
- Loss, delay, and duplication are one causal chain, not three faults: 2.1 and 2.2
  are indistinguishable to the biller, which is exactly why the retry that recovers
  from 2.1 *causes* 2.4 when the truth was 2.2. Idempotency is therefore not
  optional.
- Seven mechanisms cover ~20 faults: ingest validation, timeout+bounded retry,
  idempotency/dedup, correlation-by-ID, reconciliation, response validation,
  backpressure. Mechanisms per fault *class*, not patches per case.
- Success is terminal-state, not payment: full denial that sums correctly is
  Resolved (correct books), not an error. No Denied lifecycle state — denial is a
  line-level financial outcome, a query, not a state.
- Two ledgers: biller's-knowledge vs sim-truth. The gap between them is the
  biller's blind spot; the sim-truth view is the correctness oracle. If ground
  truth leaks into the ledger, the biller cheats.
- Determinism claim, precisely: outcome-deterministic given a seed; NOT wall-clock
  or attribution deterministic. Per-entity RNG streams; no shared RNG.
- Reconciliation equation (billed == paid + coins + copay + deduct + not_allowed,
  exact, integer cents) is simultaneously: the biller's semantic-fault detector,
  the honest payer's construction constraint, and a test invariant.
- AR aging is the external projection of the biller's state machine: a view over
  non-terminal claims ranked by time-since-first-submission. Aging keys off first
  submission — retries must not make stuck claims look fresh.
- Closed-loop demo: configure anthem slow/denial-happy; the payer scorecard
  rediscovers it from remittance data alone.
- Git history == fault table, in order.

## Live-discussion prep

Likely "new requirement" curveballs, with a sketch each:

- **Claim status inquiry (276/277).** Most likely, given our cut. Sketch: new
  message pair through the clearinghouse; biller policy gains a "query before
  retry" step; payer answers from... its statelessness problem returns — this is
  where the seen-set (or a status function derived like adjudication) re-enters.
  We've already designed both halves; say so.
- **Secondary insurance / coordination of benefits.** Claim gains an ordered payer
  list; terminal state requires the chain to complete; patient responsibility
  cascades. Ledger: lines gain per-payer adjudication slots.
- **Corrected-claim resubmission / appeals.** New claim referencing original
  (frequency code); ledger links records; aging question: original date or
  resubmission date (answer: original — same principle as retries).
- **Persistence / crash recovery.** Event log is already the answer: serialize the
  mpsc event stream to disk; recovery = replay. Fold-as-you-go + retained log was
  chosen partly for this.
- **Scale beyond one machine.** Ledger API unchanged, internals become a DB;
  clearinghouse channels become network transport; the design's seams are where
  the cuts would go. Name the seams.
- **Real EDI 837/835 formats.** Domain module isolates document shapes; swap
  serialization at the boundary.

Cuts to defend (all deliberate, all named in DESIGN.md): duplicate-claim denials
(above), clearinghouse misrouting to wrong payer (mostly undetectable by the
biller unless payer-id echoes mismatch — mention, don't build), payer hard-crash
as distinct from infinite latency (indistinguishable in-process; collapsed into
drop faults).

Honest weaknesses, admitted before asked: adjudication amounts are arbitrary
policy, not realistic contract pricing (out of scope — the *shape* is right);
single-process by assignment; snapshot-on-read reporting is O(n) clone (fine at
this scale; the API hides it).
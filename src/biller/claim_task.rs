//! One task per claim: owns the claim's entire lifecycle as linear async
//! code. Task completion == terminal state, which is what makes shutdown
//! trivial. The body is pure shell: every decision comes from `machine::next`.
//!
//! Time is computed, not slept (Decisions #23). The task keeps its own local
//! virtual clock and a min-heap of future arrivals declared by the sim; where
//! the executed implementation raced `recv()` against `sleep_until(deadline)`,
//! this shell compares the next arrival's timestamp against the deadline —
//! the same decision, as arithmetic. `machine::next` is untouched: it never
//! knew where `now` came from.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::biller::machine::{self, Action, TaskEvent, TaskState};
use crate::biller::policy::RetryPolicy;
use crate::domain::{Arrival, Claim, Delivery, Transactor, VirtualTime};
use crate::ledger::events::{ClaimEvent, LedgerTx};

#[derive(Clone)]
pub struct ClaimTaskDeps {
    pub policy: RetryPolicy,
    pub ledger: LedgerTx,
    pub clearinghouse: Arc<dyn Transactor>,
    /// Deliveries that do not correlate to this claim (mangled claim_ids,
    /// garbage without a fragment) — handed to the quarantine clerk with
    /// their arrival times. Correlation-by-ID stays a biller mechanism: the
    /// task matches payloads against its own claim_id, never sim knowledge.
    pub strays_tx: mpsc::Sender<Arrival>,
}

/// A future arrival with a deterministic tiebreak: identical timestamps are
/// processed in the order the sim declared them (attempt order, then
/// declaration order within a transaction).
struct Pending {
    at: VirtualTime,
    seq: u64,
    delivery: Delivery,
}

impl PartialEq for Pending {
    fn eq(&self, other: &Self) -> bool {
        (self.at, self.seq) == (other.at, other.seq)
    }
}
impl Eq for Pending {}
impl PartialOrd for Pending {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Pending {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.at, self.seq).cmp(&(other.at, other.seq))
    }
}

#[tracing::instrument(skip_all, fields(claim_id = %claim.claim_id, payer = %claim.payer_id))]
pub async fn run_claim(claim: Claim, ingested_at: VirtualTime, deps: ClaimTaskDeps) {
    let mut now = ingested_at;
    let mut deadline = now;
    let mut pending: BinaryHeap<Reverse<Pending>> = BinaryHeap::new();
    let mut seq: u64 = 0;

    let mut state = drive(
        TaskState::Init,
        TaskEvent::Start,
        &claim,
        &deps,
        &mut now,
        &mut deadline,
        &mut pending,
        &mut seq,
    )
    .await;

    while state != TaskState::Done {
        // The select! of the executed implementation, as a comparison: take
        // the next declared arrival if it beats the deadline, else time out.
        // `<=` keeps the executed version's biased-select semantics — at an
        // identical virtual instant the response wins over the timeout.
        let event = match pending.peek() {
            Some(Reverse(next)) if next.at <= deadline => {
                let Reverse(next) = pending.pop().expect("peeked");
                // The biller reads its inbox when it is listening: an arrival
                // that landed during a backoff sleep is processed — and its
                // machine events stamped — when the task returns to the loop,
                // exactly as the executed select! behaved. Ledger notes for
                // garbage and strays keep the true arrival time (those went
                // through the dispatcher in the executed version, which was
                // never asleep). Keeps the per-claim clock monotone.
                let arrived = next.at;
                now = now.max(arrived);
                match correlate(next.delivery, &claim, &deps, arrived).await {
                    Some(event) => event,
                    None => continue, // stray or garbage note; keep waiting
                }
            }
            _ => {
                now = deadline;
                tracing::debug!("silence past deadline; timeout");
                TaskEvent::Timeout
            }
        };
        state = drive(
            state,
            event,
            &claim,
            &deps,
            &mut now,
            &mut deadline,
            &mut pending,
            &mut seq,
        )
        .await;
    }

    // Arrivals still pending when the claim went terminal are the late
    // remittances (fault 2.3/2.4 tails), stamped at their true arrival times.
    // The executed implementation needed a drain-the-channel race note and a
    // dispatcher retirement set for this; here it is just the rest of the heap.
    while let Some(Reverse(late)) = pending.pop() {
        match late.delivery {
            Delivery::Intact(remit) if remit.claim_id == claim.claim_id => {
                deps.ledger
                    .emit(
                        late.at,
                        claim.claim_id.clone(),
                        ClaimEvent::LateRemittance { remit },
                    )
                    .await;
            }
            Delivery::Garbage {
                correlatable: Some(id),
            } if id == claim.claim_id => {
                deps.ledger
                    .emit(
                        late.at,
                        claim.claim_id.clone(),
                        ClaimEvent::GarbageRemittance,
                    )
                    .await;
            }
            delivery => {
                let _ = deps
                    .strays_tx
                    .send(Arrival {
                        at: late.at,
                        delivery,
                    })
                    .await;
            }
        }
    }
}

/// Correlation-by-ID, exactly as the dispatcher did it: payloads addressed to
/// this claim become machine events or history notes; everything else is a
/// stray for the quarantine clerk. Returns None when no machine event results.
async fn correlate(
    delivery: Delivery,
    claim: &Claim,
    deps: &ClaimTaskDeps,
    now: VirtualTime,
) -> Option<TaskEvent> {
    match delivery {
        Delivery::Intact(remit) if remit.claim_id == claim.claim_id => {
            Some(TaskEvent::Remittance(remit))
        }
        // Fault 3.5 with a readable fragment: epistemically silence — leave
        // the history note and keep waiting; the timeout machinery owns it.
        Delivery::Garbage {
            correlatable: Some(id),
        } if id == claim.claim_id => {
            tracing::debug!("garbage remittance received; treating as silence");
            deps.ledger
                .emit(now, claim.claim_id.clone(), ClaimEvent::GarbageRemittance)
                .await;
            None
        }
        delivery => {
            let _ = deps.strays_tx.send(Arrival { at: now, delivery }).await;
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn drive(
    state: TaskState,
    event: TaskEvent,
    claim: &Claim,
    deps: &ClaimTaskDeps,
    now: &mut VirtualTime,
    deadline: &mut VirtualTime,
    pending: &mut BinaryHeap<Reverse<Pending>>,
    seq: &mut u64,
) -> TaskState {
    let (next_state, actions) = machine::next(state, event, claim, &deps.policy, *now);
    for action in actions {
        match action {
            Action::Submit { attempt } => {
                // Backoff before a retry (fault 2.1) — computed, not slept.
                *now = *now + deps.policy.backoff(attempt);
                *deadline = *now + deps.policy.timeout;
                let submission = claim.to_submission(attempt, *now);
                let transaction = deps.clearinghouse.transact(&submission);
                for arrival in transaction.arrivals {
                    pending.push(Reverse(Pending {
                        at: arrival.at,
                        seq: *seq,
                        delivery: arrival.delivery,
                    }));
                    *seq += 1;
                }
            }
            Action::Emit(event) => {
                deps.ledger.emit(*now, claim.claim_id.clone(), event).await;
            }
            Action::Finish => {} // loop exit is driven by TaskState::Done
        }
    }
    next_state
}

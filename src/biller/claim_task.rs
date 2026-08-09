//! One task per claim: owns the claim's entire lifecycle as linear async
//! code. Task completion == terminal state, which is what makes shutdown
//! trivial. The body is pure shell: every decision comes from `machine::next`.

use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::biller::dispatcher::DispatcherControl;
use crate::biller::machine::{self, Action, TaskEvent, TaskState};
use crate::biller::policy::RetryPolicy;
use crate::domain::{Claim, Clock, Submission};
use crate::ledger::events::LedgerTx;

#[derive(Clone)]
pub struct ClaimTaskDeps {
    pub policy: RetryPolicy,
    pub clock: Clock,
    pub ledger: LedgerTx,
    pub clearinghouse_tx: mpsc::Sender<Submission>,
    pub dispatcher_tx: mpsc::Sender<DispatcherControl>,
}

pub async fn run_claim(claim: Claim, deps: ClaimTaskDeps) {
    // Register before first submit (and await the ack) so the remittance can
    // never beat the routing-map entry.
    let (remit_tx, mut remit_rx) = mpsc::channel(4);
    let (ack_tx, ack_rx) = oneshot::channel();
    let register = DispatcherControl::Register {
        claim_id: claim.claim_id.clone(),
        remit_tx,
        ack: ack_tx,
    };
    deps.dispatcher_tx
        .send(register)
        .await
        .expect("dispatcher gone");
    ack_rx.await.expect("dispatcher dropped registration ack");

    let mut state = TaskState::Init;
    let mut deadline = Instant::now();
    state = drive(state, TaskEvent::Start, &claim, &deps, &mut deadline).await;

    while state != TaskState::Done {
        let event = tokio::select! {
            // At an identical virtual instant prefer the response over the
            // timeout — biased keeps outcomes deterministic (Decisions #12).
            biased;
            remit = remit_rx.recv() => {
                TaskEvent::Remittance(remit.expect("dispatcher holds our sender while registered"))
            }
            _ = tokio::time::sleep_until(deadline) => TaskEvent::Timeout,
        };
        state = drive(state, event, &claim, &deps, &mut deadline).await;
    }

    let deregister = DispatcherControl::Deregister {
        claim_id: claim.claim_id.clone(),
    };
    deps.dispatcher_tx
        .send(deregister)
        .await
        .expect("dispatcher gone");
}

async fn drive(
    state: TaskState,
    event: TaskEvent,
    claim: &Claim,
    deps: &ClaimTaskDeps,
    deadline: &mut Instant,
) -> TaskState {
    let (next_state, actions) = machine::next(state, event, claim, &deps.policy, deps.clock.now());
    for action in actions {
        match action {
            Action::Submit { attempt } => {
                *deadline = Instant::now() + deps.policy.timeout;
                let submission = claim.to_submission(attempt);
                deps.clearinghouse_tx
                    .send(submission)
                    .await
                    .expect("clearinghouse gone");
            }
            Action::Emit(event) => deps.ledger.emit(claim.claim_id.clone(), event).await,
            Action::Finish => {} // loop exit is driven by TaskState::Done
        }
    }
    next_state
}

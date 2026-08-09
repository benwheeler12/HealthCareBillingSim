//! The claim lifecycle state machine — pure functional core. No I/O, no
//! channels, no clock reads: the shell (`claim_task`) feeds in events and a
//! `now`, and executes the returned actions. Fault-table unit tests target
//! `next()`, not the runtime.

use crate::biller::policy::RetryPolicy;
use crate::biller::reconcile::{self, ReconcileOutcome};
use crate::domain::{Claim, RemittanceAdvice, VirtualTime};
use crate::ledger::events::ClaimEvent;
use crate::ledger::records::FlagReason;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TaskState {
    Init,
    Awaiting { attempt: u32 },
    Done,
}

#[derive(Debug)]
pub enum TaskEvent {
    Start,
    Remittance(RemittanceAdvice),
    Timeout,
}

#[derive(Debug)]
pub enum Action {
    /// Send the claim to the clearinghouse and arm the response deadline.
    Submit {
        attempt: u32,
    },
    Emit(ClaimEvent),
    Finish,
}

pub fn next(
    state: TaskState,
    event: TaskEvent,
    claim: &Claim,
    policy: &RetryPolicy,
    now: VirtualTime,
) -> (TaskState, Vec<Action>) {
    match (state, event) {
        (TaskState::Init, TaskEvent::Start) => submit(1, policy, now),
        (TaskState::Awaiting { .. }, TaskEvent::Remittance(remit)) => {
            settle(reconcile::reconcile(claim, &remit, now))
        }
        (TaskState::Awaiting { attempt }, TaskEvent::Timeout) if attempt < policy.max_attempts => {
            submit(attempt + 1, policy, now)
        }
        (TaskState::Awaiting { .. }, TaskEvent::Timeout) => (
            TaskState::Done,
            vec![
                Action::Emit(ClaimEvent::Flagged {
                    reason: FlagReason::RetriesExhausted,
                }),
                Action::Finish,
            ],
        ),
        // Init never sees network events; Done tasks have left the select loop.
        (state, event) => {
            debug_assert!(false, "invalid transition: {state:?} on {event:?}");
            (state, Vec::new())
        }
    }
}

fn submit(attempt: u32, policy: &RetryPolicy, now: VirtualTime) -> (TaskState, Vec<Action>) {
    // The shell sleeps the backoff before sending; the recorded deadline
    // reflects that.
    let timeout_at = now + policy.backoff(attempt) + policy.timeout;
    (
        TaskState::Awaiting { attempt },
        vec![
            Action::Submit { attempt },
            Action::Emit(ClaimEvent::Submitted {
                attempt,
                timeout_at,
            }),
        ],
    )
}

fn settle(outcome: ReconcileOutcome) -> (TaskState, Vec<Action>) {
    let actions = match outcome {
        ReconcileOutcome::Balanced { lines } => vec![
            Action::Emit(ClaimEvent::RemittanceApplied { lines }),
            Action::Emit(ClaimEvent::Resolved),
            Action::Finish,
        ],
        ReconcileOutcome::Unbalanced {
            lines,
            billed,
            accounted,
        } => vec![
            Action::Emit(ClaimEvent::RemittanceApplied { lines }),
            Action::Emit(ClaimEvent::Flagged {
                reason: FlagReason::ReconciliationFailed { billed, accounted },
            }),
            Action::Finish,
        ],
        ReconcileOutcome::Malformed { .. } => vec![
            Action::Emit(ClaimEvent::Flagged {
                reason: FlagReason::MalformedRemittance,
            }),
            Action::Finish,
        ],
    };
    (TaskState::Done, actions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ClaimId, Money, PayerId, RemittanceLine, ServiceLine};

    fn claim() -> Claim {
        Claim {
            claim_id: ClaimId("c-1".into()),
            payer_id: PayerId::Medicare,
            patient_member_id: "M1".into(),
            provider_npi: "1234567890".into(),
            organization_name: "Acme".into(),
            lines: vec![ServiceLine {
                service_line_id: "L1".into(),
                procedure_code: "99213".into(),
                units: 1,
                unit_charge: Money::from_cents(1000),
                do_not_bill: false,
            }],
        }
    }

    fn balanced_remit() -> RemittanceAdvice {
        RemittanceAdvice {
            claim_id: ClaimId("c-1".into()),
            payer_id: PayerId::Medicare,
            lines: vec![RemittanceLine {
                service_line_id: "L1".into(),
                payer_paid: Money::from_cents(800),
                coinsurance: Money::from_cents(200),
                copay: Money::ZERO,
                deductible: Money::ZERO,
                not_allowed: Money::ZERO,
                denial_reason: None,
            }],
        }
    }

    #[test]
    fn happy_path_start_then_balanced_remit_resolves() {
        let (claim, policy, now) = (claim(), RetryPolicy::default(), VirtualTime::default());
        let (state, actions) = next(TaskState::Init, TaskEvent::Start, &claim, &policy, now);
        assert_eq!(state, TaskState::Awaiting { attempt: 1 });
        assert!(matches!(actions[0], Action::Submit { attempt: 1 }));

        let (state, actions) = next(
            state,
            TaskEvent::Remittance(balanced_remit()),
            &claim,
            &policy,
            now,
        );
        assert_eq!(state, TaskState::Done);
        assert!(matches!(actions[1], Action::Emit(ClaimEvent::Resolved)));
        assert!(matches!(actions[2], Action::Finish));
    }

    #[test]
    fn timeout_retries_until_attempts_exhausted_then_flags() {
        let (claim, policy, now) = (claim(), RetryPolicy::default(), VirtualTime::default());
        let mut state = TaskState::Awaiting { attempt: 1 };
        for expected in 2..=policy.max_attempts {
            let (next_state, actions) = next(state, TaskEvent::Timeout, &claim, &policy, now);
            assert_eq!(next_state, TaskState::Awaiting { attempt: expected });
            assert!(matches!(actions[0], Action::Submit { attempt } if attempt == expected));
            state = next_state;
        }
        let (state, actions) = next(state, TaskEvent::Timeout, &claim, &policy, now);
        assert_eq!(state, TaskState::Done);
        assert!(matches!(
            actions[0],
            Action::Emit(ClaimEvent::Flagged {
                reason: FlagReason::RetriesExhausted
            })
        ));
    }

    #[test]
    fn unbalanced_remit_flags_reconciliation_failure() {
        let (claim, policy, now) = (claim(), RetryPolicy::default(), VirtualTime::default());
        let mut remit = balanced_remit();
        remit.lines[0].payer_paid = Money::from_cents(700); // short by $1
        let (state, actions) = next(
            TaskState::Awaiting { attempt: 1 },
            TaskEvent::Remittance(remit),
            &claim,
            &policy,
            now,
        );
        assert_eq!(state, TaskState::Done);
        assert!(matches!(
            actions[1],
            Action::Emit(ClaimEvent::Flagged {
                reason: FlagReason::ReconciliationFailed { billed, accounted }
            }) if billed == Money::from_cents(1000) && accounted == Money::from_cents(900)
        ));
    }
}

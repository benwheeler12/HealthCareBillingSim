//! Claim lifecycle events. Every biller component holds a `LedgerTx` clone;
//! the fold task is the single consumer. The channel is lossless mpsc —
//! never broadcast (Decisions #10).

use tokio::sync::mpsc;

use crate::domain::{ClaimId, RemittanceAdvice, ValidationError, VirtualTime};
use crate::ledger::records::{Adjudication, ClaimIdentity, FlagReason, LineRecord};

#[derive(Clone, Debug)]
pub enum ClaimEvent {
    Ingested {
        identity: ClaimIdentity,
        lines: Vec<LineRecord>,
    },
    Rejected {
        reason: ValidationError,
    },
    /// Fault 1.3: a later input line reused this claim_id. First document
    /// wins; the duplicate is recorded on the existing row, never resubmitted.
    DuplicateIngest {
        line_no: usize,
    },
    Submitted {
        attempt: u32,
        timeout_at: VirtualTime,
    },
    RemittanceApplied {
        lines: Vec<(String, Adjudication)>,
    },
    Resolved,
    Flagged {
        reason: FlagReason,
    },
    /// Remittance for a claim the biller has never known (fault 3.2).
    RemittanceQuarantined,
    /// Fault 3.5: unparseable garbage whose claim_id fragment correlated to a
    /// known claim. History note only — epistemically this is silence.
    GarbageRemittance,
    /// Remittance for a claim already terminal. Carries the full remittance:
    /// on a Resolved claim it is ignored (logged idempotency, Decisions #5);
    /// on Flagged(RetriesExhausted), a complete, balanced late answer is
    /// allowed to transition the claim to Resolved (Decisions #6).
    LateRemittance {
        remit: RemittanceAdvice,
    },
}

#[derive(Clone, Debug)]
pub struct StampedEvent {
    pub at: VirtualTime,
    pub claim_id: ClaimId,
    pub event: ClaimEvent,
}

/// Sender half of the ledger channel. The caller owns time (Decisions #23):
/// every emission carries the emitting claim's computed virtual timestamp —
/// there is no clock to read.
#[derive(Clone)]
pub struct LedgerTx {
    tx: mpsc::Sender<StampedEvent>,
}

impl LedgerTx {
    pub fn new(tx: mpsc::Sender<StampedEvent>) -> LedgerTx {
        LedgerTx { tx }
    }

    pub async fn emit(&self, at: VirtualTime, claim_id: ClaimId, event: ClaimEvent) {
        let stamped = StampedEvent {
            at,
            claim_id,
            event,
        };
        // The fold task outlives every sender by construction (shutdown drains
        // senders first); a send failure would be a wiring bug.
        self.tx
            .send(stamped)
            .await
            .expect("ledger fold task gone while senders alive");
    }
}

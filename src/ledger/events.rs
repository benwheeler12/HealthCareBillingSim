//! Claim lifecycle events. Every biller component holds a `LedgerTx` clone;
//! the fold task is the single consumer. The channel is lossless mpsc —
//! never broadcast (Decisions #10).

use tokio::sync::mpsc;

use crate::domain::{ClaimId, Clock, ValidationError, VirtualTime};
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
    /// Remittance for a claim already terminal (Decisions #5/#6 territory).
    LateRemittance,
}

#[derive(Clone, Debug)]
pub struct StampedEvent {
    pub at: VirtualTime,
    pub claim_id: ClaimId,
    pub event: ClaimEvent,
}

/// Sender half of the ledger channel, paired with the clock so events are
/// stamped at emission, not at fold.
#[derive(Clone)]
pub struct LedgerTx {
    tx: mpsc::Sender<StampedEvent>,
    clock: Clock,
}

impl LedgerTx {
    pub fn new(tx: mpsc::Sender<StampedEvent>, clock: Clock) -> LedgerTx {
        LedgerTx { tx, clock }
    }

    pub fn now(&self) -> VirtualTime {
        self.clock.now()
    }

    pub async fn emit(&self, claim_id: ClaimId, event: ClaimEvent) {
        let stamped = StampedEvent {
            at: self.clock.now(),
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

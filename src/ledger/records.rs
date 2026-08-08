//! Ledger schema (DESIGN.md). Raw facts only: every aggregate is a pure
//! function of a snapshot, never a stored field.

use std::collections::HashMap;

use crate::domain::{ClaimId, DenialReason, Money, PayerId, ValidationError, VirtualTime};
use crate::ledger::events::StampedEvent;

#[derive(Debug, Default)]
pub struct Ledger {
    /// No secondary indexes; scans are fine at this scale.
    pub claims: HashMap<ClaimId, ClaimRecord>,
    /// Remittances that never matched a known claim (fault 3.2).
    pub quarantine: Vec<StampedEvent>,
    /// Raw event log, retained for replay/audit alongside the live fold.
    pub event_log: Vec<StampedEvent>,
}

#[derive(Clone, Debug)]
pub struct ClaimRecord {
    // identity (immutable after ingest)
    pub claim_id: ClaimId,
    /// `None` only for rejected rows whose document never yielded an identity
    /// (parse errors, missing insurance block) — see Decisions #13.
    pub identity: Option<ClaimIdentity>,
    // lifecycle
    pub state: ClaimState,
    pub attempts: u32,
    pub ingested_at: VirtualTime,
    /// Set once; retries never touch it (aging keys off this).
    pub first_submitted_at: Option<VirtualTime>,
    pub resolved_at: Option<VirtualTime>,
    // money
    pub lines: Vec<LineRecord>,
    // audit / monitoring — append-only
    pub history: Vec<StampedEvent>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ClaimIdentity {
    pub payer_id: PayerId,
    pub provider_npi: String,
    pub organization_name: String,
    pub patient_member_id: String,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ClaimState {
    /// Ingested, not yet submitted (Decisions #13). Gap is ms-scale.
    Pending,
    /// Terminal at ingest; still a ledger row.
    Rejected { reason: ValidationError },
    /// State carries its own deadline.
    AwaitingResponse { timeout_at: VirtualTime },
    /// All lines adjudicated AND reconciled.
    Resolved,
    /// Human-review queue; terminal.
    Flagged { reason: FlagReason },
}

impl ClaimState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, ClaimState::Rejected { .. } | ClaimState::Resolved | ClaimState::Flagged { .. })
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FlagReason {
    RetriesExhausted,
    ReconciliationFailed { billed: Money, accounted: Money },
    MalformedRemittance,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LineRecord {
    pub service_line_id: String,
    pub procedure_code: String,
    pub units: u32,
    pub unit_charge: Money,
    /// Recorded but excluded from submission and money aggregates (Decisions #9).
    pub do_not_bill: bool,
    /// None = unanswered; partial claims have a mix.
    pub adjudication: Option<Adjudication>,
}

impl LineRecord {
    pub fn billed(&self) -> Money {
        self.unit_charge * self.units
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Adjudication {
    pub payer_paid: Money,
    pub coinsurance: Money,
    pub copay: Money,
    pub deductible: Money,
    pub not_allowed: Money,
    pub denial_reason: Option<DenialReason>,
    pub adjudicated_at: VirtualTime,
}

impl Adjudication {
    pub fn patient_responsibility(&self) -> Money {
        self.coinsurance + self.copay + self.deductible
    }
}

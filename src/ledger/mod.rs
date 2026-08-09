//! The biller's system of record: a single-consumer mpsc fold over a lossless
//! event stream. Holds only what the biller could know from remittances —
//! simulation ground truth never leaks in here.

pub mod events;
pub mod fold;
pub mod records;

pub use events::{ClaimEvent, LedgerTx, StampedEvent};
pub use fold::run_fold;
pub use records::{
    Adjudication, ClaimIdentity, ClaimRecord, ClaimState, FlagReason, Ledger, LineRecord,
};

//! Shared message and domain types — the only module `sim/` and `biller/` may
//! both depend on. No simulation internals, no biller state.

pub mod claim;
pub mod messages;
pub mod money;
pub mod remittance;
pub mod time;
pub mod validation;

pub use claim::{Claim, ClaimId, PayerId, ServiceLine};
pub use messages::{
    Arrival, Delivery, Submission, SubmittedClaim, SubmittedLine, Transaction, Transactor,
};
pub use money::Money;
pub use remittance::{DenialReason, RemittanceAdvice, RemittanceLine};
pub use time::{VirtualTime, human_virtual};
pub use validation::ValidationError;

//! Reports: pure functions over a ledger snapshot + now. No stored
//! aggregates anywhere — everything here is derived.
//!
//! Dependency rule: reports read only the ledger — with one deliberate,
//! documented exception: `diagnostic` compares the ledger against sim-truth
//! (Decisions #16); that is the report's entire purpose.

pub mod aging;
pub mod chase;
pub mod diagnostic;
pub mod scorecard;
pub mod summary;

pub use aging::{ArAging, ar_aging, ar_aging_for, days_in_ar, days_in_ar_for};
pub use chase::{ChaseList, chase_list};
pub use diagnostic::{Diagnostic, diagnostic};
pub use scorecard::{Denials, Scorecard, denial_breakdown, payer_scorecard};
pub use summary::{Summary, summarize};

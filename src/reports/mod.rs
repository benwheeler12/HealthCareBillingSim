//! Reports: pure functions over a ledger snapshot + now. Depends only on
//! `ledger` (and `domain`). No stored aggregates anywhere — everything here
//! is derived.

pub mod summary;

pub use summary::{Summary, summarize};

//! The biller — the system under test. Holds all the state (claim tasks,
//! routing map, ledger senders). Must never import `sim/`.

pub mod policy;

pub use policy::RetryPolicy;

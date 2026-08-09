//! Ground truth of injected faults, kept strictly apart from the ledger: the
//! ledger holds only what the biller could know from remittances; the gap
//! between the two views is the biller's blind spot and the correctness
//! oracle in tests.
//!
//! Populated by the fault-injection commits (classes 2 and 3); the zero-fault
//! vertical slice has no faults to record.

#[derive(Debug, Default)]
pub struct SimTruth {
    /// (claim_id, attempt, decision_point) of every fault actually injected.
    pub injected: Vec<InjectedFault>,
}

#[derive(Debug, Clone)]
pub struct InjectedFault {
    pub claim_id: String,
    pub attempt: u32,
    pub kind: &'static str,
}

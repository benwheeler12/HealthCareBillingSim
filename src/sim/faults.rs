//! Transport/semantic fault configuration for the clearinghouse and payers.
//!
//! All-zero (`Default`) means an honest, lossless adversary — the happy path
//! runs there. Each field is switched on by its fault-table commit
//! (DESIGN.md classes 2 and 3).

#[derive(Clone, Copy, Debug, Default)]
pub struct FaultProfile {
    /// Fault 2.1: probability a claim is dropped on the biller → payer hop.
    pub forward_drop_rate: f64,
    /// Fault 2.2: probability a remittance is dropped on the payer → biller hop.
    pub return_drop_rate: f64,
    /// Fault 2.3: probability a remittance is delayed in transit, and the
    /// delay's upper bound. Interacts with the biller timeout to produce
    /// emergent timeouts — never injected as a "timeout event".
    pub extra_delay_rate: f64,
    pub max_extra_delay_secs: f64,
    /// Fault 2.4: probability a delivery is duplicated.
    pub duplicate_rate: f64,
    /// Fault 3.1: probability a payer lies (amounts don't sum).
    pub dishonest_adjudication_rate: f64,
    /// Fault 3.2: probability a remittance's claim_id is mangled in transit.
    pub corrupt_claim_id_rate: f64,
}

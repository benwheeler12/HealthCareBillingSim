//! Transport/semantic fault configuration for the clearinghouse and payers.
//!
//! All-zero (`Default`) means an honest, lossless adversary — the happy path
//! runs there. Each field is switched on by its fault-table commit
//! (DESIGN.md classes 2 and 3).

#[derive(Clone, Copy, Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
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
    /// Fault 3.3: per-line probability that a service line vanishes from the
    /// remittance in transit, leaving the claim partially adjudicated.
    pub line_drop_rate: f64,
    /// Fault 3.2: probability a remittance's claim_id is mangled in transit.
    pub corrupt_claim_id_rate: f64,
    /// Fault 3.5: probability a remittance turns to unparseable garbage in
    /// transit. Epistemically silence (DESIGN.md Decisions); timeout machinery owns it.
    pub corrupt_remittance_rate: f64,
}

impl FaultProfile {
    /// One-line human summary of the enabled faults — shared by the stdout
    /// banner and the TUI's payer detail.
    pub fn summary(&self) -> String {
        let pct = |x: f64| format!("{:.0}%", x * 100.0);
        let human = |secs: f64| match secs {
            s if s >= 86_400.0 => format!("{:.0}d", s / 86_400.0),
            s if s >= 3_600.0 => format!("{:.1}h", s / 3_600.0),
            s if s >= 60.0 => format!("{:.1}m", s / 60.0),
            s => format!("{s:.0}s"),
        };
        let mut parts = Vec::new();
        if self.forward_drop_rate > 0.0 {
            parts.push(format!("fwd drops {}", pct(self.forward_drop_rate)));
        }
        if self.return_drop_rate > 0.0 {
            parts.push(format!("ret drops {}", pct(self.return_drop_rate)));
        }
        if self.duplicate_rate > 0.0 {
            parts.push(format!("dups {}", pct(self.duplicate_rate)));
        }
        if self.extra_delay_rate > 0.0 {
            parts.push(format!(
                "delays {} (≤{})",
                pct(self.extra_delay_rate),
                human(self.max_extra_delay_secs)
            ));
        }
        if self.dishonest_adjudication_rate > 0.0 {
            parts.push(format!(
                "dishonest {}",
                pct(self.dishonest_adjudication_rate)
            ));
        }
        if self.line_drop_rate > 0.0 {
            parts.push(format!("line drops {}", pct(self.line_drop_rate)));
        }
        if self.corrupt_claim_id_rate > 0.0 {
            parts.push(format!("corrupt ids {}", pct(self.corrupt_claim_id_rate)));
        }
        if self.corrupt_remittance_rate > 0.0 {
            parts.push(format!("garbage {}", pct(self.corrupt_remittance_rate)));
        }
        if parts.is_empty() {
            "none — honest, lossless transport".to_string()
        } else {
            parts.join(" · ")
        }
    }
}

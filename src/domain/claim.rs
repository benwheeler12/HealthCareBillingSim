//! Claim documents: the raw wire shape (`PayerClaimDoc`, serde mirror of the
//! assessment's JSON Schema) and the validated domain `Claim` the biller works
//! with. Validation lives in `validation.rs`; nothing downstream of ingest
//! ever touches a raw doc.

use std::fmt;

use serde::Deserialize;

use crate::domain::money::Money;

#[derive(Clone, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct ClaimId(pub String);

impl fmt::Display for ClaimId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PayerId {
    Medicare,
    UnitedHealthGroup,
    Anthem,
}

impl PayerId {
    pub const ALL: [PayerId; 3] = [
        PayerId::Medicare,
        PayerId::UnitedHealthGroup,
        PayerId::Anthem,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            PayerId::Medicare => "medicare",
            PayerId::UnitedHealthGroup => "united_health_group",
            PayerId::Anthem => "anthem",
        }
    }
}

impl fmt::Display for PayerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Validated claim. Identity fields are immutable after ingest; patient
/// demographics are validated at the boundary but not carried further (the
/// ledger only needs what its reports key on).
#[derive(Clone, Debug)]
pub struct Claim {
    pub claim_id: ClaimId,
    pub payer_id: PayerId,
    pub patient_member_id: String,
    pub provider_npi: String,
    pub organization_name: String,
    pub lines: Vec<ServiceLine>,
}

#[derive(Clone, Debug)]
pub struct ServiceLine {
    pub service_line_id: String,
    pub procedure_code: String,
    pub units: u32,
    pub unit_charge: Money,
    /// Recorded in the ledger, excluded from submission and money aggregates
    /// (Decisions #9).
    pub do_not_bill: bool,
}

impl Claim {
    pub fn billable_lines(&self) -> impl Iterator<Item = &ServiceLine> {
        self.lines.iter().filter(|l| !l.do_not_bill)
    }
}

impl ServiceLine {
    pub fn billed(&self) -> Money {
        self.unit_charge * self.units
    }
}

// ---- Raw wire shape (assessment JSON Schema) -------------------------------

#[derive(Debug, Deserialize)]
pub struct PayerClaimDoc {
    pub claim_id: String,
    #[allow(dead_code)] // required by schema; unused downstream
    pub place_of_service_code: i64,
    pub insurance: InsuranceDoc,
    pub patient: PatientDoc,
    pub organization: OrganizationDoc,
    pub rendering_provider: ProviderDoc,
    pub service_lines: Vec<ServiceLineDoc>,
}

#[derive(Debug, Deserialize)]
pub struct InsuranceDoc {
    pub payer_id: PayerId,
    pub patient_member_id: String,
}

#[derive(Debug, Deserialize)]
pub struct PatientDoc {
    pub first_name: String,
    pub last_name: String,
    pub gender: String,
    pub dob: String,
}

#[derive(Debug, Deserialize)]
pub struct OrganizationDoc {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct ProviderDoc {
    pub first_name: String,
    pub last_name: String,
    pub npi: String,
}

#[derive(Debug, Deserialize)]
pub struct ServiceLineDoc {
    pub service_line_id: String,
    pub procedure_code: String,
    pub units: i64,
    pub details: String,
    pub unit_charge_currency: String,
    pub unit_charge_amount: f64,
    #[serde(default)]
    pub do_not_bill: bool,
}

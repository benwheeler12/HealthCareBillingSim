//! Payer remittance advice (EDI 835-inspired): per service line, the payer's
//! full accounting of the billed amount. The reconciliation equation
//! `billed == payer_paid + coinsurance + copay + deductible + not_allowed`
//! is checked by the biller, guaranteed by honest payers, and violated only by
//! dishonest mode (fault 3.1).

use std::fmt;

use crate::domain::claim::{ClaimId, PayerId};
use crate::domain::money::Money;

#[derive(Clone, Debug)]
pub struct RemittanceAdvice {
    pub claim_id: ClaimId,
    pub payer_id: PayerId,
    pub lines: Vec<RemittanceLine>,
}

#[derive(Clone, Debug)]
pub struct RemittanceLine {
    pub service_line_id: String,
    pub payer_paid: Money,
    pub coinsurance: Money,
    pub copay: Money,
    pub deductible: Money,
    pub not_allowed: Money,
    pub denial_reason: Option<DenialReason>,
}

impl RemittanceLine {
    /// Everything the payer accounted for; reconciliation compares this to billed.
    pub fn accounted(&self) -> Money {
        self.payer_paid + self.coinsurance + self.copay + self.deductible + self.not_allowed
    }

    pub fn patient_responsibility(&self) -> Money {
        self.coinsurance + self.copay + self.deductible
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum DenialReason {
    NotCovered,
    PriorAuthRequired,
    MedicalNecessity,
    OutOfNetwork,
}

impl fmt::Display for DenialReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            DenialReason::NotCovered => "not_covered",
            DenialReason::PriorAuthRequired => "prior_auth_required",
            DenialReason::MedicalNecessity => "medical_necessity",
            DenialReason::OutOfNetwork => "out_of_network",
        };
        f.write_str(s)
    }
}

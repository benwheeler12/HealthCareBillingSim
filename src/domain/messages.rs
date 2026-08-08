//! Channel message types crossing the biller ↔ simulation boundary.

use crate::domain::claim::{Claim, ClaimId, PayerId};
use crate::domain::money::Money;

/// What actually goes over the wire to the clearinghouse: billable lines only
/// (Decisions #9), plus the attempt number so transport-fault RNG streams can
/// give retries fresh fates.
#[derive(Clone, Debug)]
pub struct Submission {
    pub claim: SubmittedClaim,
    pub attempt: u32,
}

#[derive(Clone, Debug)]
pub struct SubmittedClaim {
    pub claim_id: ClaimId,
    pub payer_id: PayerId,
    pub lines: Vec<SubmittedLine>,
}

#[derive(Clone, Debug)]
pub struct SubmittedLine {
    pub service_line_id: String,
    pub procedure_code: String,
    pub units: u32,
    pub unit_charge: Money,
}

impl SubmittedLine {
    pub fn billed(&self) -> Money {
        self.unit_charge * self.units
    }
}

impl Claim {
    pub fn to_submission(&self, attempt: u32) -> Submission {
        let lines = self
            .billable_lines()
            .map(|l| SubmittedLine {
                service_line_id: l.service_line_id.clone(),
                procedure_code: l.procedure_code.clone(),
                units: l.units,
                unit_charge: l.unit_charge,
            })
            .collect();
        Submission {
            claim: SubmittedClaim {
                claim_id: self.claim_id.clone(),
                payer_id: self.payer_id,
                lines,
            },
            attempt,
        }
    }
}

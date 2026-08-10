//! Message types crossing the biller ↔ simulation boundary. Time crosses it
//! as *data* (Decisions #23): the biller declares when it sent a submission;
//! the sim declares when (and whether) each thing comes back. Nobody sleeps.

use crate::domain::claim::{Claim, ClaimId, PayerId};
use crate::domain::money::Money;
use crate::domain::remittance::RemittanceAdvice;
use crate::domain::time::VirtualTime;

/// What the clearinghouse actually hands the biller on the return hop.
/// Fault 3.5 can turn a remittance into unparseable garbage in transit;
/// epistemically that is silence (Decisions #8) — the timeout machinery owns
/// it — but a correlatable claim_id fragment still earns a history note.
#[derive(Clone, Debug)]
pub enum Delivery {
    Intact(RemittanceAdvice),
    Garbage { correlatable: Option<ClaimId> },
}

/// What actually goes over the wire to the clearinghouse: billable lines only
/// (Decisions #9), plus the attempt number so transport-fault RNG streams can
/// give retries fresh fates. The biller's timeout intent is deliberately NOT
/// on the wire — the claim task owns its deadline and does the comparison;
/// the payer never learns the biller's patience.
#[derive(Clone, Debug)]
pub struct Submission {
    pub claim: SubmittedClaim,
    pub attempt: u32,
    /// Virtual instant the biller put this attempt on the wire.
    pub sent_at: VirtualTime,
}

/// The sim's complete account of one submission: everything that will ever
/// come back from it, each stamped with its virtual arrival time. An empty
/// vec IS the drop faults — pure silence, with no explanation attached; the
/// biller is never told which hop failed, or that anything failed at all.
#[derive(Clone, Debug)]
pub struct Transaction {
    pub arrivals: Vec<Arrival>,
}

#[derive(Clone, Debug)]
pub struct Arrival {
    pub at: VirtualTime,
    pub delivery: Delivery,
}

/// The biller-facing face of the simulation (Decisions #23). Lives in
/// `domain` so `biller` and `sim` still never import each other: the sim
/// implements it, the biller holds it as `Arc<dyn Transactor>`. Sync and
/// `&self` on purpose — calls execute on the calling claim task's worker
/// thread, which is what makes adjudication parallel across threads. This is
/// the seam where a real network transport would slot in.
pub trait Transactor: Send + Sync {
    fn transact(&self, submission: &Submission) -> Transaction;
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
    pub fn to_submission(&self, attempt: u32, sent_at: VirtualTime) -> Submission {
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
            sent_at,
        }
    }
}

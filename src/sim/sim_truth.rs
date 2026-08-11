//! Ground truth of injected faults, kept strictly apart from the ledger: the
//! ledger holds only what the biller could know from remittances; the gap
//! between the two views is the biller's blind spot and this recorder is the
//! correctness oracle in tests.
//!
//! Same single-consumer mpsc-fold shape as the ledger: fault injectors send,
//! the recorder folds, `run()` returns the final `SimTruth` alongside the
//! ledger. Never visible to any biller component.

use std::collections::HashSet;

use tokio::sync::mpsc;

use crate::domain::ClaimId;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FaultKind {
    /// 2.1: claim never reached the payer.
    ForwardDrop,
    /// 2.2: payer adjudicated but the remittance was lost.
    ReturnDrop,
    /// 2.3: extra transport delay on the return hop.
    ExtraDelay,
    /// 2.4: delivery duplicated.
    Duplicate,
    /// 3.1: payer lied — amounts deliberately don't sum.
    Dishonest,
    /// 3.2: remittance claim_id mangled in transit.
    CorruptClaimId,
    /// 3.3: a service line vanished from the remittance.
    LineDrop,
    /// 3.5: remittance turned to unparseable garbage.
    CorruptRemittance,
}

#[derive(Clone, Debug)]
pub struct InjectedFault {
    /// The claim's real id (pre-corruption, for CorruptClaimId).
    pub claim_id: ClaimId,
    pub attempt: u32,
    pub kind: FaultKind,
}

#[derive(Debug, Default)]
pub struct SimTruth {
    pub injected: Vec<InjectedFault>,
}

impl SimTruth {
    pub fn claims_with(&self, kind: FaultKind) -> HashSet<&ClaimId> {
        self.injected
            .iter()
            .filter(|f| f.kind == kind)
            .map(|f| &f.claim_id)
            .collect()
    }

    pub fn count(&self, kind: FaultKind) -> usize {
        self.injected.iter().filter(|f| f.kind == kind).count()
    }
}

/// Single consumer of the fault channel; returns when every injector has
/// dropped its sender (structurally: when the clearinghouse handle is gone).
/// Unbounded because `Clearinghouse::transact` is a sync call on the biller's
/// thread (DESIGN.md 'Virtual time') — recording must not block, and fault volume is
/// bounded by the fault table itself.
pub async fn run_recorder(mut rx: mpsc::UnboundedReceiver<InjectedFault>) -> SimTruth {
    let mut truth = SimTruth::default();
    while let Some(fault) = rx.recv().await {
        tracing::debug!(claim_id = %fault.claim_id, attempt = fault.attempt,
            kind = ?fault.kind, "fault injected");
        truth.injected.push(fault);
    }
    truth
}

//! Healthcare billing lifecycle simulation (Brace Health take-home).
//! See DESIGN.md for the converged design; module dependency rules:
//! `sim` and `biller` never import each other — shared types live in `domain`.

pub mod biller;
pub mod domain;
pub mod ledger;
pub mod reports;
pub mod sim;

use std::collections::HashMap;
use std::io::BufRead;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::biller::claim_task::{ClaimTaskDeps, run_claim};
use crate::biller::dispatcher::run_dispatcher;
use crate::biller::policy::RetryPolicy;
use crate::domain::{Claim, ClaimId, Clock, PayerId, validation};
use crate::ledger::events::{ClaimEvent, LedgerTx};
use crate::ledger::fold::run_fold;
use crate::ledger::records::{ClaimIdentity, Ledger, LineRecord};
use crate::sim::clearinghouse::Clearinghouse;
use crate::sim::faults::FaultProfile;
use crate::sim::payer::{PayerConfig, default_payer_configs};
use crate::sim::rng::RngFactory;
use crate::sim::sim_truth::{SimTruth, run_recorder};

pub struct RunConfig {
    pub input_path: PathBuf,
    pub seed: u64,
    /// Claims ingested per virtual second.
    pub rate_per_sec: f64,
    pub policy: RetryPolicy,
    pub payers: HashMap<PayerId, PayerConfig>,
    pub faults: FaultProfile,
}

impl RunConfig {
    pub fn new(input_path: PathBuf, seed: u64, rate_per_sec: f64) -> RunConfig {
        RunConfig {
            input_path,
            seed,
            rate_per_sec,
            policy: RetryPolicy::default(),
            payers: default_payer_configs(),
            faults: FaultProfile::default(),
        }
    }
}

/// Everything a run produces: the biller's-knowledge ledger and the
/// simulation's ground truth of injected faults. The gap between the two is
/// the biller's blind spot — tests use `sim_truth` as the oracle.
pub struct RunOutput {
    pub ledger: Ledger,
    pub sim_truth: SimTruth,
}

/// Run the whole simulation to completion.
///
/// Must run inside a current-thread runtime with `start_paused(true)`
/// (Decisions #12). Shutdown is structural: input exhausted + claim-task
/// JoinSet drained ⇒ channels close in dependency order ⇒ the ledger fold
/// returns ⇒ done. No shutdown signals anywhere.
pub async fn run(cfg: RunConfig) -> anyhow::Result<RunOutput> {
    let clock = Clock::start();

    let (ledger_tx, ledger_rx) = mpsc::channel(1024);
    let (submission_tx, submission_rx) = mpsc::channel(256);
    let (remit_tx, remit_rx) = mpsc::channel(256);
    let (dispatcher_tx, dispatcher_rx) = mpsc::channel(256);
    let (sim_truth_tx, sim_truth_rx) = mpsc::channel(256);
    let ledger_tx = LedgerTx::new(ledger_tx, clock.clone());

    let fold = tokio::spawn(run_fold(ledger_rx));
    let recorder = tokio::spawn(run_recorder(sim_truth_rx));
    let dispatcher = tokio::spawn(run_dispatcher(dispatcher_rx, remit_rx, ledger_tx.clone()));
    let clearinghouse = Clearinghouse {
        payers: cfg.payers.clone(),
        rng: RngFactory::new(cfg.seed),
        faults: cfg.faults,
        sim_truth: sim_truth_tx,
    };
    let clearinghouse = tokio::spawn(clearinghouse.run(submission_rx, remit_tx));

    let deps = ClaimTaskDeps {
        policy: cfg.policy,
        clock,
        ledger: ledger_tx.clone(),
        clearinghouse_tx: submission_tx,
        dispatcher_tx,
    };
    let mut claim_tasks = ingest(&cfg, &ledger_tx, &deps).await?;
    drop(deps);

    while let Some(joined) = claim_tasks.join_next().await {
        joined.context("claim task panicked")?;
    }
    clearinghouse.await.context("clearinghouse panicked")?;
    dispatcher.await.context("dispatcher panicked")?;
    drop(ledger_tx);
    let ledger = fold.await.context("ledger fold panicked")?;
    let sim_truth = recorder.await.context("sim-truth recorder panicked")?;

    tracing::info!(
        claims = ledger.claims.len(),
        injected_faults = sim_truth.injected.len(),
        "simulation complete"
    );
    Ok(RunOutput { ledger, sim_truth })
}

/// Read the input file at the configured rate, validate each line, and spawn
/// one claim task per valid claim. Malformed lines become Rejected ledger
/// rows — never silent drops.
async fn ingest(
    cfg: &RunConfig,
    ledger: &LedgerTx,
    deps: &ClaimTaskDeps,
) -> anyhow::Result<JoinSet<()>> {
    let file = std::fs::File::open(&cfg.input_path)
        .with_context(|| format!("opening {}", cfg.input_path.display()))?;
    let reader = std::io::BufReader::new(file);
    let interval = Duration::from_secs_f64(1.0 / cfg.rate_per_sec);

    let mut tasks = JoinSet::new();
    for (line_no, line) in reader.lines().enumerate() {
        let line = line.context("reading input")?;
        tokio::time::sleep(interval).await;
        match validation::validate_line(&line) {
            Ok(claim) => {
                tracing::info!(claim_id = %claim.claim_id, payer = %claim.payer_id, "claim ingested");
                ledger
                    .emit(claim.claim_id.clone(), ingested_event(&claim))
                    .await;
                tasks.spawn(run_claim(claim, deps.clone()));
            }
            Err((claim_id, reason)) => {
                let claim_id = claim_id
                    .unwrap_or_else(|| ClaimId(format!("<unparseable-line-{}>", line_no + 1)));
                tracing::warn!(%claim_id, ?reason, "claim rejected at ingest");
                ledger.emit(claim_id, ClaimEvent::Rejected { reason }).await;
            }
        }
    }
    Ok(tasks)
}

fn ingested_event(claim: &Claim) -> ClaimEvent {
    let identity = ClaimIdentity {
        payer_id: claim.payer_id,
        provider_npi: claim.provider_npi.clone(),
        organization_name: claim.organization_name.clone(),
        patient_member_id: claim.patient_member_id.clone(),
    };
    let lines = claim
        .lines
        .iter()
        .map(|l| LineRecord {
            service_line_id: l.service_line_id.clone(),
            procedure_code: l.procedure_code.clone(),
            units: l.units,
            unit_charge: l.unit_charge,
            do_not_bill: l.do_not_bill,
            adjudication: None,
        })
        .collect();
    ClaimEvent::Ingested { identity, lines }
}

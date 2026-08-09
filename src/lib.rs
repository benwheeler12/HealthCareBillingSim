//! Healthcare billing lifecycle simulation (Brace Health take-home).
//! See DESIGN.md for the converged design; module dependency rules:
//! `sim` and `biller` never import each other — shared types live in `domain`.

pub mod biller;
pub mod domain;
pub mod ledger;
pub mod reports;
pub mod scenario;
pub mod sim;

use std::collections::HashMap;
use std::io::BufRead;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Context;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;

use crate::biller::claim_task::{ClaimTaskDeps, run_claim};
use crate::biller::dispatcher::run_dispatcher;
use crate::biller::policy::RetryPolicy;
use crate::domain::{Claim, ClaimId, Clock, PayerId, VirtualTime, validation};
use crate::ledger::events::{ClaimEvent, LedgerTx};
use crate::ledger::fold::{FoldOutput, Progress, run_fold};
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
    /// Per-payer fault overrides; payers absent here use `faults`. Different
    /// clearinghouse routes really do fail differently.
    pub payer_faults: HashMap<PayerId, FaultProfile>,
    /// Optional live-counter tap for progress display; None in tests.
    pub progress: Option<watch::Sender<Progress>>,
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
            payer_faults: HashMap::new(),
            progress: None,
        }
    }
}

/// Everything a run produces: the biller's-knowledge ledger and the
/// simulation's ground truth of injected faults. The gap between the two is
/// the biller's blind spot — tests use `sim_truth` as the oracle.
pub struct RunOutput {
    /// Final books: every claim terminal (the correctness guarantee).
    pub ledger: Ledger,
    /// The books frozen at the moment intake ended — the mid-flight view a
    /// biller actually lives in. A/R reports run over this: on the final
    /// ledger every young receivable has been driven terminal by design.
    pub intake_ledger: Ledger,
    pub sim_truth: SimTruth,
    /// Virtual time when the last input line was ingested.
    pub intake_finished_at: VirtualTime,
    /// Virtual time at completion — every claim terminal by here.
    pub finished_at: VirtualTime,
}

/// Run the whole simulation to completion.
///
/// Must run inside a current-thread runtime with `start_paused(true)`
/// (Decisions #12). Shutdown is structural: input exhausted + claim-task
/// JoinSet drained ⇒ channels close in dependency order ⇒ the ledger fold
/// returns ⇒ done. No shutdown signals anywhere.
pub async fn run(mut cfg: RunConfig) -> anyhow::Result<RunOutput> {
    let clock = Clock::start();

    let (ledger_tx, ledger_rx) = mpsc::channel(1024);
    let (submission_tx, submission_rx) = mpsc::channel(256);
    let (remit_tx, remit_rx) = mpsc::channel(256);
    let (dispatcher_tx, dispatcher_rx) = mpsc::channel(256);
    let (sim_truth_tx, sim_truth_rx) = mpsc::channel(256);
    let ledger_tx = LedgerTx::new(ledger_tx, clock.clone());

    let intake_done: Arc<OnceLock<VirtualTime>> = Arc::new(OnceLock::new());
    let fold = tokio::spawn(run_fold(
        ledger_rx,
        cfg.progress.take(),
        intake_done.clone(),
    ));
    let recorder = tokio::spawn(run_recorder(sim_truth_rx));
    let dispatcher = tokio::spawn(run_dispatcher(dispatcher_rx, remit_rx, ledger_tx.clone()));
    let resolved_faults = cfg
        .payers
        .keys()
        .map(|payer| {
            let profile = cfg.payer_faults.get(payer).copied().unwrap_or(cfg.faults);
            (*payer, profile)
        })
        .collect();
    let clearinghouse = Clearinghouse {
        payers: cfg.payers.clone(),
        rng: RngFactory::new(cfg.seed),
        faults: resolved_faults,
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
    let clock_for_reports = deps.clock.clone();
    let mut claim_tasks = ingest(&cfg, &ledger_tx, &deps).await?;
    drop(deps);
    // Set synchronously before any further await: virtual time cannot have
    // advanced past this instant, so the fold's snapshot check is race-free.
    let intake_finished_at = clock_for_reports.now();
    let _ = intake_done.set(intake_finished_at);

    while let Some(joined) = claim_tasks.join_next().await {
        joined.context("claim task panicked")?;
    }
    clearinghouse.await.context("clearinghouse panicked")?;
    dispatcher.await.context("dispatcher panicked")?;
    drop(ledger_tx);
    let FoldOutput {
        ledger,
        intake_snapshot,
    } = fold.await.context("ledger fold panicked")?;
    let sim_truth = recorder.await.context("sim-truth recorder panicked")?;

    tracing::info!(
        claims = ledger.claims.len(),
        injected_faults = sim_truth.injected.len(),
        "simulation complete"
    );
    Ok(RunOutput {
        ledger,
        intake_ledger: intake_snapshot,
        sim_truth,
        intake_finished_at,
        finished_at: clock_for_reports.now(),
    })
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
    let mut seen = std::collections::HashSet::new();
    for (line_no, line) in reader.lines().enumerate() {
        let line = line.context("reading input")?;
        let line_no = line_no + 1; // 1-based for humans
        tokio::time::sleep(interval).await;
        match validation::validate_line(&line) {
            // Fault 1.3: a claim_id we've already ingested. First document
            // wins; the dup becomes a history event, never a resubmission.
            Ok(claim) if seen.contains(&claim.claim_id) => {
                tracing::debug!(claim_id = %claim.claim_id, line_no, "duplicate claim_id in input");
                ledger
                    .emit(claim.claim_id, ClaimEvent::DuplicateIngest { line_no })
                    .await;
            }
            // Fault 1.4: schema-valid but nothing billable. Policy: resolved
            // trivially at ingest — correct books with zero submissions.
            Ok(claim) if claim.billable_lines().count() == 0 => {
                tracing::debug!(claim_id = %claim.claim_id, "no billable lines; trivially resolved");
                seen.insert(claim.claim_id.clone());
                ledger
                    .emit(claim.claim_id.clone(), ingested_event(&claim))
                    .await;
                ledger.emit(claim.claim_id, ClaimEvent::Resolved).await;
            }
            Ok(claim) => {
                tracing::debug!(claim_id = %claim.claim_id, payer = %claim.payer_id, "claim ingested");
                seen.insert(claim.claim_id.clone());
                ledger
                    .emit(claim.claim_id.clone(), ingested_event(&claim))
                    .await;
                tasks.spawn(run_claim(claim, deps.clone()));
            }
            Err((claim_id, reason)) => {
                let claim_id =
                    claim_id.unwrap_or_else(|| ClaimId(format!("<unparseable-line-{line_no}>")));
                tracing::debug!(%claim_id, ?reason, "claim rejected at ingest");
                seen.insert(claim_id.clone());
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

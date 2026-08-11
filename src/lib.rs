//! Healthcare billing lifecycle simulation (Brace Health take-home).
//! See DESIGN.md for the converged design; module dependency rules:
//! `sim` and `biller` never import each other — shared types live in `domain`.

pub mod biller;
pub mod claimgen;
pub mod domain;
pub mod ledger;
pub mod reports;
pub mod scenario;
pub mod sim;
pub mod simconfig;

use std::collections::HashMap;
use std::io::BufRead;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;

use crate::biller::claim_task::{ClaimTaskDeps, run_claim};
use crate::biller::dispatcher::run_quarantine;
use crate::biller::policy::RetryPolicy;
use crate::domain::{Claim, ClaimId, PayerId, VirtualTime, validation};
use crate::ledger::events::{ClaimEvent, LedgerTx};
use crate::ledger::fold::{FoldOutput, Progress, finalize, run_fold};
use crate::ledger::records::{ClaimIdentity, Ledger, LineRecord};
use crate::sim::clearinghouse::Clearinghouse;
use crate::sim::faults::FaultProfile;
use crate::sim::payer::{PayerConfig, default_payer_configs};
use crate::sim::rng::RngFactory;
use crate::sim::sim_truth::{SimTruth, run_recorder};

/// Where the claim documents come from. `Generated` is the product path:
/// claims are minted in memory and handed to ingest — and their claim tasks
/// — as they are produced, with no file anywhere. `Lines` and `File` are
/// library-only conveniences for tests, benchmarks, and replaying a real
/// dataset.
pub enum ClaimSource {
    Generated(claimgen::GenConfig),
    Lines(Vec<String>),
    File(PathBuf),
}

impl ClaimSource {
    /// The lazy line stream ingest consumes; `master_seed` resolves a
    /// generator seed that follows the run's seed.
    fn lines(
        &self,
        master_seed: u64,
    ) -> anyhow::Result<Box<dyn Iterator<Item = std::io::Result<String>> + Send + '_>> {
        match self {
            ClaimSource::Generated(generator) => {
                Ok(Box::new(claimgen::stream(generator, master_seed).map(Ok)))
            }
            ClaimSource::Lines(lines) => Ok(Box::new(lines.iter().cloned().map(Ok))),
            ClaimSource::File(path) => {
                let file = std::fs::File::open(path)
                    .with_context(|| format!("opening {}", path.display()))?;
                Ok(Box::new(std::io::BufReader::new(file).lines()))
            }
        }
    }

    /// Total documents when it is knowable up front (`File` streams blind).
    pub fn known_count(&self) -> Option<usize> {
        match self {
            ClaimSource::Generated(generator) => Some(generator.count),
            ClaimSource::Lines(lines) => Some(lines.len()),
            ClaimSource::File(_) => None,
        }
    }
}

impl From<claimgen::GenConfig> for ClaimSource {
    fn from(generator: claimgen::GenConfig) -> ClaimSource {
        ClaimSource::Generated(generator)
    }
}

impl From<Vec<String>> for ClaimSource {
    fn from(lines: Vec<String>) -> ClaimSource {
        ClaimSource::Lines(lines)
    }
}

impl From<PathBuf> for ClaimSource {
    fn from(path: PathBuf) -> ClaimSource {
        ClaimSource::File(path)
    }
}

pub struct RunConfig {
    pub source: ClaimSource,
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
    pub fn new(source: impl Into<ClaimSource>, seed: u64, rate_per_sec: f64) -> RunConfig {
        RunConfig {
            source: source.into(),
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
/// Time is computed, never slept (DESIGN.md 'Virtual time'), so this runs on any tokio
/// runtime — the multi-thread scheduler by default, where claim tasks
/// genuinely execute in parallel. Shutdown is structural: input exhausted +
/// claim-task JoinSet drained ⇒ channels close in dependency order ⇒ the
/// ledger fold returns ⇒ done. No shutdown signals anywhere.
pub async fn run(mut cfg: RunConfig) -> anyhow::Result<RunOutput> {
    // Deep buffer: with parallel producers the fold must almost never make a
    // sender park — backpressure only matters if the fold truly falls behind.
    let (ledger_tx, ledger_rx) = mpsc::channel(65_536);
    let (strays_tx, strays_rx) = mpsc::channel(256);
    let (sim_truth_tx, sim_truth_rx) = mpsc::unbounded_channel();
    let ledger_tx = LedgerTx::new(ledger_tx);

    let fold = tokio::spawn(run_fold(ledger_rx, cfg.progress.take()));
    let recorder = tokio::spawn(run_recorder(sim_truth_rx));
    let clerk = tokio::spawn(run_quarantine(strays_rx, ledger_tx.clone()));
    let resolved_faults = cfg
        .payers
        .keys()
        .map(|payer| {
            let profile = cfg.payer_faults.get(payer).copied().unwrap_or(cfg.faults);
            (*payer, profile)
        })
        .collect();
    let clearinghouse = Arc::new(Clearinghouse {
        payers: cfg.payers.clone(),
        rng: RngFactory::new(cfg.seed),
        faults: resolved_faults,
        sim_truth: sim_truth_tx,
    });

    let deps = ClaimTaskDeps {
        policy: cfg.policy,
        ledger: ledger_tx.clone(),
        clearinghouse,
        strays_tx,
    };
    // Phase timing for benchmark analysis (SIM_PHASES=1): wall time per
    // pipeline stage, to separate the parallel region from the serial ones.
    let phases = std::env::var_os("SIM_PHASES").is_some();
    let t0 = std::time::Instant::now();
    let (mut claim_tasks, intake_finished_at) = ingest(&cfg, &ledger_tx, &deps).await?;
    drop(deps); // releases our strays_tx + clearinghouse handles
    let t_ingest = t0.elapsed();

    while let Some(joined) = claim_tasks.join_next().await {
        joined.context("claim task panicked")?;
    }
    // Every claim task is done ⇒ every strays sender is dropped ⇒ the clerk
    // drains and exits; it holds a ledger sender, so it must finish before
    // the fold can.
    clerk.await.context("quarantine clerk panicked")?;
    drop(ledger_tx);
    let ledger = fold.await.context("ledger fold panicked")?;
    let sim_truth = recorder.await.context("sim-truth recorder panicked")?;
    let t_drain = t0.elapsed();

    let finished_at = ledger
        .event_log
        .iter()
        .map(|e| e.at)
        .max()
        .unwrap_or_default();
    let FoldOutput {
        ledger,
        intake_snapshot,
    } = finalize(ledger, intake_finished_at);
    if phases {
        eprintln!(
            "SIM_PHASES ingest={:.2}s drain={:.2}s finalize={:.2}s",
            t_ingest.as_secs_f64(),
            (t_drain - t_ingest).as_secs_f64(),
            (t0.elapsed() - t_drain).as_secs_f64(),
        );
    }

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
        finished_at,
    })
}

/// Pull lines from the claim source, validate each one, and spawn one claim
/// task per valid claim. Malformed lines become Rejected ledger rows — never
/// silent drops. The configured rate assigns each line its computed arrival
/// time — line i lands at i × (1/rate), exactly where the rate-limited sleep
/// loop used to put it — and returns the last arrival as the intake-end mark.
///
/// The source streams lazily, so with a `Generated` source claims are handed
/// to biller tasks batch by batch as they are minted — generation, validation
/// and claim-task spawning are one pipeline with no intermediate file.
///
/// Validation (JSON parsing — the CPU cost of ingest) runs in parallel:
/// lines are read in batches, fanned out to one validator task per worker,
/// and the verdicts are consumed strictly in input order, so dedup ("first
/// document wins") and every emitted event are byte-identical to the serial
/// loop. Only the pure parse is parallel; the bookkeeping stays sequential.
async fn ingest(
    cfg: &RunConfig,
    ledger: &LedgerTx,
    deps: &ClaimTaskDeps,
) -> anyhow::Result<(JoinSet<()>, VirtualTime)> {
    const BATCH: usize = 16 * 1024;
    let interval = Duration::from_secs_f64(1.0 / cfg.rate_per_sec);
    let workers = std::thread::available_parallelism().map_or(1, |n| n.get());

    let mut tasks = JoinSet::new();
    let mut seen = std::collections::HashSet::new();
    let mut arrival = VirtualTime::default();
    let mut line_no = 0usize;
    let mut lines = cfg.source.lines(cfg.seed)?;
    loop {
        let mut batch = Vec::with_capacity(BATCH);
        for line in lines.by_ref().take(BATCH) {
            batch.push(line.context("reading input")?);
        }
        if batch.is_empty() {
            break;
        }
        let chunk_size = batch.len().div_ceil(workers);
        let mut chunks: Vec<Vec<String>> = Vec::with_capacity(workers);
        let mut rest = batch;
        while rest.len() > chunk_size {
            let tail = rest.split_off(chunk_size);
            chunks.push(rest);
            rest = tail;
        }
        chunks.push(rest);
        let validators: Vec<_> = chunks
            .into_iter()
            .map(|chunk| {
                tokio::spawn(async move {
                    chunk
                        .iter()
                        .map(|line| validation::validate_line(line))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for validator in validators {
            let verdicts = validator.await.context("validator task panicked")?;
            for verdict in verdicts {
                line_no += 1;
                arrival = arrival + interval;
                ingest_one(
                    verdict, line_no, arrival, ledger, deps, &mut seen, &mut tasks,
                )
                .await;
            }
        }
    }
    Ok((tasks, arrival))
}

/// The sequential ingest state machine for one verdict, in input order.
#[allow(clippy::too_many_arguments)]
async fn ingest_one(
    verdict: Result<Claim, (Option<ClaimId>, domain::ValidationError)>,
    line_no: usize,
    arrival: VirtualTime,
    ledger: &LedgerTx,
    deps: &ClaimTaskDeps,
    seen: &mut std::collections::HashSet<ClaimId>,
    tasks: &mut JoinSet<()>,
) {
    match verdict {
        // Fault 1.3: a claim_id we've already ingested. First document
        // wins; the dup becomes a history event, never a resubmission.
        Ok(claim) if seen.contains(&claim.claim_id) => {
            tracing::debug!(claim_id = %claim.claim_id, line_no, "duplicate claim_id in input");
            ledger
                .emit(
                    arrival,
                    claim.claim_id,
                    ClaimEvent::DuplicateIngest { line_no },
                )
                .await;
        }
        // Fault 1.4: schema-valid but nothing billable. Policy: resolved
        // trivially at ingest — correct books with zero submissions.
        Ok(claim) if claim.billable_lines().count() == 0 => {
            tracing::debug!(claim_id = %claim.claim_id, "no billable lines; trivially resolved");
            seen.insert(claim.claim_id.clone());
            ledger
                .emit(arrival, claim.claim_id.clone(), ingested_event(&claim))
                .await;
            ledger
                .emit(arrival, claim.claim_id, ClaimEvent::Resolved)
                .await;
        }
        Ok(claim) => {
            tracing::debug!(claim_id = %claim.claim_id, payer = %claim.payer_id, "claim ingested");
            seen.insert(claim.claim_id.clone());
            ledger
                .emit(arrival, claim.claim_id.clone(), ingested_event(&claim))
                .await;
            tasks.spawn(run_claim(claim, arrival, deps.clone()));
        }
        Err((claim_id, reason)) => {
            let claim_id =
                claim_id.unwrap_or_else(|| ClaimId(format!("<unparseable-line-{line_no}>")));
            tracing::debug!(%claim_id, ?reason, "claim rejected at ingest");
            seen.insert(claim_id.clone());
            ledger
                .emit(arrival, claim_id, ClaimEvent::Rejected { reason })
                .await;
        }
    }
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

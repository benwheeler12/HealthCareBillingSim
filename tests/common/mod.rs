//! Shared helpers for integration tests: claim-document builders, input-file
//! writing, and paused-runtime execution. Each test crate uses the subset it
//! needs.
#![allow(dead_code)]

use std::io::Write;
use std::path::PathBuf;

use healthcare_billing_sim::ledger::records::Ledger;
use healthcare_billing_sim::{RunConfig, RunOutput, run};
use serde_json::json;

pub fn claim_line(claim_id: &str, payer: &str, lines: serde_json::Value) -> String {
    json!({
        "claim_id": claim_id,
        "place_of_service_code": 11,
        "insurance": {"payer_id": payer, "patient_member_id": format!("M-{claim_id}")},
        "patient": {"first_name": "Ada", "last_name": "Lovelace", "gender": "f", "dob": "1985-12-10"},
        "organization": {"name": "Analytical Engines LLC"},
        "rendering_provider": {"first_name": "Grace", "last_name": "Hopper", "npi": "1234567890"},
        "service_lines": lines,
    })
    .to_string()
}

pub fn service_line(id: &str, units: u32, dollars: f64, do_not_bill: bool) -> serde_json::Value {
    json!({
        "service_line_id": id,
        "procedure_code": "99213",
        "units": units,
        "details": "office visit",
        "unit_charge_currency": "USD",
        "unit_charge_amount": dollars,
        "do_not_bill": do_not_bill,
    })
}

/// A simple one-line claim, cycling payers by index.
pub fn simple_claim(index: usize) -> String {
    let payer = ["medicare", "united_health_group", "anthem"][index % 3];
    claim_line(
        &format!("c-{index:03}"),
        payer,
        json!([service_line(
            "L1",
            1 + (index as u32 % 3),
            100.0 + index as f64,
            false
        )]),
    )
}

pub fn write_input(name: &str, lines: &[String]) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let mut file = std::fs::File::create(&path).expect("create input");
    for line in lines {
        writeln!(file, "{line}").expect("write input");
    }
    path
}

/// Run a fully-specified config to completion on a fresh paused runtime.
pub fn run_sim_with(cfg: RunConfig) -> RunOutput {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("runtime");
    runtime.block_on(run(cfg)).expect("run")
}

/// Zero-fault run with default policy/payers — the happy-path baseline.
pub fn run_sim(path: PathBuf, seed: u64) -> Ledger {
    run_sim_with(RunConfig::new(path, seed, 1.0)).ledger
}

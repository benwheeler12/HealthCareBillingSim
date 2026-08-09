//! Scenario files for `--fault-profile`: a JSON document that switches on
//! faults and optionally reshapes payer personalities and the biller's retry
//! policy — everything a demo or an experiment needs to vary, without
//! recompiling. Missing fields keep their defaults; unknown fields are typos
//! and rejected loudly.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;

use crate::RunConfig;
use crate::domain::{Money, PayerId};
use crate::sim::faults::FaultProfile;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Scenario {
    pub faults: FaultPatch,
    pub payers: HashMap<PayerId, PayerPatch>,
    pub policy: PolicyPatch,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FaultPatch {
    pub forward_drop_rate: Option<f64>,
    pub return_drop_rate: Option<f64>,
    pub extra_delay_rate: Option<f64>,
    pub max_extra_delay_secs: Option<f64>,
    pub duplicate_rate: Option<f64>,
    pub dishonest_adjudication_rate: Option<f64>,
    pub line_drop_rate: Option<f64>,
    pub corrupt_claim_id_rate: Option<f64>,
    pub corrupt_remittance_rate: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PayerPatch {
    pub min_response_time_secs: Option<f64>,
    pub max_response_time_secs: Option<f64>,
    pub denial_rate: Option<f64>,
    pub max_not_allowed_bps: Option<u32>,
    pub max_deductible_bps: Option<u32>,
    pub coinsurance_bps: Option<u32>,
    pub copay_cents: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PolicyPatch {
    pub max_attempts: Option<u32>,
    pub timeout_secs: Option<f64>,
    pub backoff_base_secs: Option<f64>,
}

pub fn load(path: &Path) -> anyhow::Result<Scenario> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading scenario {}", path.display()))?;
    let scenario: Scenario = serde_json::from_str(&text)
        .with_context(|| format!("parsing scenario {}", path.display()))?;
    scenario.validate()?;
    Ok(scenario)
}

impl Scenario {
    fn validate(&self) -> anyhow::Result<()> {
        let f = &self.faults;
        for (name, rate) in [
            ("forward_drop_rate", f.forward_drop_rate),
            ("return_drop_rate", f.return_drop_rate),
            ("extra_delay_rate", f.extra_delay_rate),
            ("duplicate_rate", f.duplicate_rate),
            ("dishonest_adjudication_rate", f.dishonest_adjudication_rate),
            ("line_drop_rate", f.line_drop_rate),
            ("corrupt_claim_id_rate", f.corrupt_claim_id_rate),
            ("corrupt_remittance_rate", f.corrupt_remittance_rate),
        ] {
            if let Some(rate) = rate {
                anyhow::ensure!(
                    (0.0..=1.0).contains(&rate),
                    "{name} must be in [0,1], got {rate}"
                );
            }
        }
        for (payer, patch) in &self.payers {
            if let Some(rate) = patch.denial_rate {
                anyhow::ensure!(
                    (0.0..=1.0).contains(&rate),
                    "{payer}: denial_rate must be in [0,1], got {rate}"
                );
            }
        }
        Ok(())
    }

    pub fn apply(&self, cfg: &mut RunConfig) {
        apply_faults(&self.faults, &mut cfg.faults);
        for (payer, patch) in &self.payers {
            let target = cfg.payers.get_mut(payer).expect("all payers have defaults");
            set(
                &mut target.min_response_time_secs,
                patch.min_response_time_secs,
            );
            set(
                &mut target.max_response_time_secs,
                patch.max_response_time_secs,
            );
            set(&mut target.denial_rate, patch.denial_rate);
            set(&mut target.max_not_allowed_bps, patch.max_not_allowed_bps);
            set(&mut target.max_deductible_bps, patch.max_deductible_bps);
            set(&mut target.coinsurance_bps, patch.coinsurance_bps);
            if let Some(cents) = patch.copay_cents {
                target.copay = Money::from_cents(cents);
            }
        }
        if let Some(n) = self.policy.max_attempts {
            cfg.policy.max_attempts = n;
        }
        if let Some(secs) = self.policy.timeout_secs {
            cfg.policy.timeout = Duration::from_secs_f64(secs);
        }
        if let Some(secs) = self.policy.backoff_base_secs {
            cfg.policy.backoff_base = Duration::from_secs_f64(secs);
        }
    }
}

fn apply_faults(patch: &FaultPatch, faults: &mut FaultProfile) {
    set(&mut faults.forward_drop_rate, patch.forward_drop_rate);
    set(&mut faults.return_drop_rate, patch.return_drop_rate);
    set(&mut faults.extra_delay_rate, patch.extra_delay_rate);
    set(&mut faults.max_extra_delay_secs, patch.max_extra_delay_secs);
    set(&mut faults.duplicate_rate, patch.duplicate_rate);
    set(
        &mut faults.dishonest_adjudication_rate,
        patch.dishonest_adjudication_rate,
    );
    set(&mut faults.line_drop_rate, patch.line_drop_rate);
    set(
        &mut faults.corrupt_claim_id_rate,
        patch.corrupt_claim_id_rate,
    );
    set(
        &mut faults.corrupt_remittance_rate,
        patch.corrupt_remittance_rate,
    );
}

fn set<T: Copy>(target: &mut T, patch: Option<T>) {
    if let Some(value) = patch {
        *target = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_overrides_only_named_fields() {
        let scenario: Scenario = serde_json::from_str(
            r#"{
                "faults": {"forward_drop_rate": 0.3},
                "payers": {"anthem": {"denial_rate": 0.5, "min_response_time_secs": 100.0}},
                "policy": {"max_attempts": 2}
            }"#,
        )
        .expect("parse");
        let mut cfg = RunConfig::new("x.jsonl".into(), 1, 1.0);
        let default_anthem_max = cfg.payers[&PayerId::Anthem].max_response_time_secs;
        scenario.apply(&mut cfg);

        assert_eq!(cfg.faults.forward_drop_rate, 0.3);
        assert_eq!(cfg.faults.return_drop_rate, 0.0, "unnamed field untouched");
        let anthem = &cfg.payers[&PayerId::Anthem];
        assert_eq!(anthem.denial_rate, 0.5);
        assert_eq!(anthem.min_response_time_secs, 100.0);
        assert_eq!(anthem.max_response_time_secs, default_anthem_max);
        assert_eq!(cfg.policy.max_attempts, 2);
    }

    #[test]
    fn unknown_fields_and_bad_rates_are_rejected() {
        assert!(serde_json::from_str::<Scenario>(r#"{"fautls": {}}"#).is_err());
        let out_of_range: Scenario =
            serde_json::from_str(r#"{"faults": {"forward_drop_rate": 1.5}}"#).expect("parses");
        assert!(out_of_range.validate().is_err());
    }
}

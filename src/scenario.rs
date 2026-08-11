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
    /// Per-payer transport/semantic fault overrides, layered on top of the
    /// global profile: different clearinghouse routes fail differently.
    pub faults: FaultPatch,
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

impl FaultPatch {
    pub fn count_set(&self) -> usize {
        [
            self.forward_drop_rate.is_some(),
            self.return_drop_rate.is_some(),
            self.extra_delay_rate.is_some(),
            self.max_extra_delay_secs.is_some(),
            self.duplicate_rate.is_some(),
            self.dishonest_adjudication_rate.is_some(),
            self.line_drop_rate.is_some(),
            self.corrupt_claim_id_rate.is_some(),
            self.corrupt_remittance_rate.is_some(),
        ]
        .iter()
        .filter(|set| **set)
        .count()
    }
}

impl Scenario {
    /// Public validation entry for scenarios built in code (CLI flags).
    pub fn validated(&self) -> anyhow::Result<()> {
        self.validate()
    }

    /// How many individual overrides this scenario carries (payer patches
    /// count once each).
    pub fn count_set(&self) -> usize {
        let p = &self.policy;
        self.faults.count_set()
            + [
                p.max_attempts.is_some(),
                p.timeout_secs.is_some(),
                p.backoff_base_secs.is_some(),
            ]
            .iter()
            .filter(|set| **set)
            .count()
            + self.payers.len()
    }

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
            if patch.faults.count_set() > 0 {
                let mut profile = cfg.payer_faults.get(payer).copied().unwrap_or(cfg.faults);
                apply_faults(&patch.faults, &mut profile);
                cfg.payer_faults.insert(*payer, profile);
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

/// Built-in presets for one-flag demos. Precedence in the CLI: defaults →
/// preset → `--fault-profile` file → individual flags.
///
/// All presets share realistic, day-scale payer personalities and a
/// follow-up policy to match — the units a practice owner lives in. (The
/// *library* defaults stay second-scale; seeded tests keep their own clock.)
pub fn preset(name: &str) -> Option<Scenario> {
    const VDAY: f64 = 86_400.0;
    let mut scenario = Scenario::default();

    // Shared personality base: government-adjacent payers answer in days,
    // commercial payers in weeks, the strugglers in a month-plus; the biller
    // follows up after 45 days, twice, a week apart.
    let bands: [(PayerId, f64, f64); 10] = [
        (PayerId::KaiserPermanente, 2.0, 8.0),
        (PayerId::Medicare, 3.0, 10.0),
        (PayerId::Aetna, 5.0, 12.0),
        (PayerId::Cigna, 8.0, 20.0),
        (PayerId::BlueCrossBlueShield, 10.0, 22.0),
        (PayerId::UnitedHealthGroup, 10.0, 25.0),
        (PayerId::Humana, 12.0, 26.0),
        (PayerId::Centene, 14.0, 30.0),
        (PayerId::MolinaHealthcare, 18.0, 38.0),
        (PayerId::Anthem, 20.0, 40.0),
    ];
    for (payer, min_days, max_days) in bands {
        scenario.payers.insert(
            payer,
            PayerPatch {
                min_response_time_secs: Some(min_days * VDAY),
                max_response_time_secs: Some(max_days * VDAY),
                ..PayerPatch::default()
            },
        );
    }
    scenario.policy = PolicyPatch {
        max_attempts: Some(3),
        timeout_secs: Some(45.0 * VDAY),
        backoff_base_secs: Some(7.0 * VDAY),
    };

    match name {
        // Lossless transport — the true happy path, on realistic clocks.
        "honest" => {}
        // Everyday clearinghouse weather, with per-route personalities that
        // give each payer a distinct A/R aging profile: medicare's route is
        // clean but occasionally lies (young disputes); anthem's route loses
        // things constantly (old claims stuck in retry loops); uhg between.
        "messy" => {
            scenario.faults = FaultPatch {
                forward_drop_rate: Some(0.08),
                return_drop_rate: Some(0.05),
                duplicate_rate: Some(0.08),
                ..FaultPatch::default()
            };
            let medicare = scenario.payers.get_mut(&PayerId::Medicare).expect("base");
            medicare.faults = FaultPatch {
                forward_drop_rate: Some(0.03),
                return_drop_rate: Some(0.02),
                dishonest_adjudication_rate: Some(0.05),
                ..FaultPatch::default()
            };
            let anthem = scenario.payers.get_mut(&PayerId::Anthem).expect("base");
            anthem.faults = FaultPatch {
                forward_drop_rate: Some(0.35),
                return_drop_rate: Some(0.20),
                extra_delay_rate: Some(0.20),
                max_extra_delay_secs: Some(30.0 * VDAY),
                ..FaultPatch::default()
            };
            // The other route personalities, quieter but distinct: kaiser's
            // route is nearly perfect; molina's is anthem's little sibling;
            // centene loses claims outright; bcbs's clearinghouse route
            // stutters with duplicates; humana's paperwork sheds individual
            // service lines (partial remits); cigna's route garbles whole
            // remittances and claim_ids (garbage-as-silence, quarantine).
            let kaiser = scenario
                .payers
                .get_mut(&PayerId::KaiserPermanente)
                .expect("base");
            kaiser.faults = FaultPatch {
                forward_drop_rate: Some(0.01),
                return_drop_rate: Some(0.01),
                duplicate_rate: Some(0.01),
                ..FaultPatch::default()
            };
            let molina = scenario
                .payers
                .get_mut(&PayerId::MolinaHealthcare)
                .expect("base");
            molina.faults = FaultPatch {
                forward_drop_rate: Some(0.20),
                return_drop_rate: Some(0.12),
                extra_delay_rate: Some(0.15),
                max_extra_delay_secs: Some(20.0 * VDAY),
                ..FaultPatch::default()
            };
            let centene = scenario.payers.get_mut(&PayerId::Centene).expect("base");
            centene.faults = FaultPatch {
                forward_drop_rate: Some(0.15),
                ..FaultPatch::default()
            };
            let bcbs = scenario
                .payers
                .get_mut(&PayerId::BlueCrossBlueShield)
                .expect("base");
            bcbs.faults = FaultPatch {
                duplicate_rate: Some(0.15),
                ..FaultPatch::default()
            };
            let humana = scenario.payers.get_mut(&PayerId::Humana).expect("base");
            humana.faults = FaultPatch {
                line_drop_rate: Some(0.12),
                ..FaultPatch::default()
            };
            let cigna = scenario.payers.get_mut(&PayerId::Cigna).expect("base");
            cigna.faults = FaultPatch {
                corrupt_remittance_rate: Some(0.06),
                corrupt_claim_id_rate: Some(0.04),
                ..FaultPatch::default()
            };
        }
        // Everything at once: heavy transport chaos, semantic faults, a
        // denial-happy anthem, and a tight retry budget.
        "chaos" => {
            scenario.faults = FaultPatch {
                forward_drop_rate: Some(0.20),
                return_drop_rate: Some(0.10),
                duplicate_rate: Some(0.10),
                extra_delay_rate: Some(0.15),
                max_extra_delay_secs: Some(60.0 * VDAY),
                dishonest_adjudication_rate: Some(0.06),
                line_drop_rate: Some(0.05),
                corrupt_claim_id_rate: Some(0.05),
                corrupt_remittance_rate: Some(0.05),
            };
            let anthem = scenario.payers.get_mut(&PayerId::Anthem).expect("base");
            anthem.denial_rate = Some(0.35);
            let molina = scenario
                .payers
                .get_mut(&PayerId::MolinaHealthcare)
                .expect("base");
            molina.denial_rate = Some(0.30);
            scenario.policy.max_attempts = Some(2);
        }
        _ => return None,
    }
    Some(scenario)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_overrides_only_named_fields() {
        let scenario: Scenario = serde_json::from_str(
            r#"{
                "faults": {"forward_drop_rate": 0.3},
                "payers": {"anthem": {"denial_rate": 0.5, "min_response_time_secs": 100.0,
                                       "faults": {"forward_drop_rate": 0.9}}},
                "policy": {"max_attempts": 2}
            }"#,
        )
        .expect("parse");
        let mut cfg = RunConfig::new(crate::ClaimSource::Lines(Vec::new()), 1, 1.0);
        let default_anthem_max = cfg.payers[&PayerId::Anthem].max_response_time_secs;
        scenario.apply(&mut cfg);

        assert_eq!(cfg.faults.forward_drop_rate, 0.3);
        assert_eq!(cfg.faults.return_drop_rate, 0.0, "unnamed field untouched");
        let anthem = &cfg.payers[&PayerId::Anthem];
        assert_eq!(anthem.denial_rate, 0.5);
        assert_eq!(anthem.min_response_time_secs, 100.0);
        assert_eq!(anthem.max_response_time_secs, default_anthem_max);
        assert_eq!(cfg.policy.max_attempts, 2);
        // Per-payer faults layer over the (patched) global profile.
        let anthem_faults = cfg.payer_faults[&PayerId::Anthem];
        assert_eq!(anthem_faults.forward_drop_rate, 0.9);
        assert_eq!(anthem_faults.return_drop_rate, 0.0);
        assert!(!cfg.payer_faults.contains_key(&PayerId::Medicare));
    }

    #[test]
    fn presets_resolve_and_apply() {
        assert!(preset("nonsense").is_none());

        // Honest: realistic clocks, zero fault rates anywhere.
        let mut cfg = RunConfig::new(crate::ClaimSource::Lines(Vec::new()), 1, 1.0);
        preset("honest").expect("honest").apply(&mut cfg);
        assert_eq!(cfg.faults.forward_drop_rate, 0.0);
        assert!(cfg.payer_faults.is_empty());
        assert!(cfg.payers[&PayerId::Medicare].min_response_time_secs >= 86_400.0);
        assert!(
            cfg.policy.timeout.as_secs_f64() > cfg.payers[&PayerId::Anthem].max_response_time_secs,
            "honest runs must never hit emergent timeouts"
        );

        // Messy: per-payer route personalities.
        let mut cfg = RunConfig::new(crate::ClaimSource::Lines(Vec::new()), 1, 1.0);
        preset("messy").expect("messy").apply(&mut cfg);
        let anthem = cfg.payer_faults[&PayerId::Anthem];
        let medicare = cfg.payer_faults[&PayerId::Medicare];
        assert!(anthem.forward_drop_rate > cfg.faults.forward_drop_rate);
        assert!(medicare.forward_drop_rate < cfg.faults.forward_drop_rate);
        assert!(medicare.dishonest_adjudication_rate > 0.0);
        assert_eq!(anthem.dishonest_adjudication_rate, 0.0);
        // Humana sheds service lines; cigna garbles remittances — the
        // semantic fault classes stay visible in a flag-free run.
        assert!(cfg.payer_faults[&PayerId::Humana].line_drop_rate > 0.0);
        let cigna = cfg.payer_faults[&PayerId::Cigna];
        assert!(cigna.corrupt_remittance_rate > 0.0);
        assert!(cigna.corrupt_claim_id_rate > 0.0);

        // Chaos: everything on, tight budget.
        let mut cfg = RunConfig::new(crate::ClaimSource::Lines(Vec::new()), 1, 1.0);
        preset("chaos").expect("chaos").apply(&mut cfg);
        assert!(cfg.faults.corrupt_remittance_rate > 0.0);
        assert_eq!(cfg.policy.max_attempts, 2);
        assert!(cfg.payers[&PayerId::Anthem].denial_rate > 0.3);
    }

    #[test]
    fn unknown_fields_and_bad_rates_are_rejected() {
        assert!(serde_json::from_str::<Scenario>(r#"{"fautls": {}}"#).is_err());
        let out_of_range: Scenario =
            serde_json::from_str(r#"{"faults": {"forward_drop_rate": 1.5}}"#).expect("parses");
        assert!(out_of_range.validate().is_err());
    }
}

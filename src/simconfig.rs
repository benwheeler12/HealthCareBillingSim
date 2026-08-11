//! The one configuration the user edits: generation, simulation, faults,
//! retry policy, and payer personalities in a single struct that lives for
//! the whole interactive session. Each run mints a fresh `RunConfig` from it
//! (`to_run_config`), so what the configuration screen shows is exactly what
//! runs — there is no patch stack to resolve mentally.
//!
//! Presets are an *action* here, not a layer: applying one resets the
//! tunables to defaults and rewrites them in place. Any manual edit
//! afterwards flips the preset label to "custom".

use std::collections::HashMap;

use crate::biller::policy::RetryPolicy;
use crate::claimgen::GenConfig;
use crate::domain::{PayerId, human_virtual};
use crate::scenario;
use crate::sim::faults::FaultProfile;
use crate::sim::payer::{PayerConfig, default_payer_configs};
use crate::{ClaimSource, RunConfig};

/// Built-in fault presets, plus the state every manual edit lands in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    Honest,
    Messy,
    Chaos,
    Custom,
}

impl Preset {
    pub const CYCLE: [Preset; 3] = [Preset::Honest, Preset::Messy, Preset::Chaos];

    pub fn name(self) -> &'static str {
        match self {
            Preset::Honest => "honest",
            Preset::Messy => "messy",
            Preset::Chaos => "chaos",
            Preset::Custom => "custom",
        }
    }

    pub fn parse(name: &str) -> Option<Preset> {
        Preset::CYCLE.into_iter().find(|p| p.name() == name)
    }

    /// The next preset in the cycle; from `Custom`, either direction lands on
    /// a real preset.
    pub fn cycled(self, dir: i8) -> Preset {
        let index = Preset::CYCLE.iter().position(|p| *p == self);
        match (index, dir) {
            (Some(i), d) => {
                let n = Preset::CYCLE.len() as isize;
                Preset::CYCLE[((i as isize + d as isize).rem_euclid(n)) as usize]
            }
            (None, d) if d < 0 => Preset::Chaos,
            (None, _) => Preset::Honest,
        }
    }
}

/// Everything a run needs, editable in place. This is the source of truth
/// behind both the configuration screen and the plain CLI banner.
#[derive(Clone)]
pub struct SimConfig {
    pub generator: GenConfig,
    /// Master seed: drives the simulation, and the generator too unless
    /// `generator.seed` pins its own.
    pub seed: u64,
    /// Claims ingested per virtual second.
    pub rate_per_sec: f64,
    /// Worker threads for the sim runtime (0 = one per core).
    pub threads: usize,
    pub preset: Preset,
    pub faults: FaultProfile,
    pub payer_faults: HashMap<PayerId, FaultProfile>,
    pub payers: HashMap<PayerId, PayerConfig>,
    pub policy: RetryPolicy,
}

impl Default for SimConfig {
    /// The out-of-the-box configuration: 10k generated claims with the
    /// `messy` preset — the same demo the README opens on.
    fn default() -> SimConfig {
        let mut cfg = SimConfig {
            generator: GenConfig::default(),
            seed: 42,
            rate_per_sec: 0.0004,
            threads: 0,
            preset: Preset::Messy,
            faults: FaultProfile::default(),
            payer_faults: HashMap::new(),
            payers: default_payer_configs(),
            policy: RetryPolicy::default(),
        };
        cfg.apply_preset(Preset::Messy);
        cfg
    }
}

impl SimConfig {
    /// Reset the tunables to library defaults, then rewrite them with the
    /// preset — the screen always shows the values that will actually run.
    pub fn apply_preset(&mut self, preset: Preset) {
        if preset == Preset::Custom {
            self.preset = Preset::Custom;
            return;
        }
        self.faults = FaultProfile::default();
        self.payer_faults = HashMap::new();
        self.payers = default_payer_configs();
        self.policy = RetryPolicy::default();
        let mut rc = self.to_run_config();
        scenario::preset(preset.name())
            .expect("cycle presets all exist")
            .apply(&mut rc);
        self.absorb(rc);
        self.preset = preset;
    }

    /// Layer a scenario (a `--fault-profile` file, or CLI flag overrides) on
    /// top of the current values.
    pub fn apply_scenario(&mut self, scenario: &scenario::Scenario) {
        let mut rc = self.to_run_config();
        scenario.apply(&mut rc);
        self.absorb(rc);
    }

    fn absorb(&mut self, rc: RunConfig) {
        self.faults = rc.faults;
        self.payer_faults = rc.payer_faults;
        self.payers = rc.payers;
        self.policy = rc.policy;
    }

    /// A fresh RunConfig for one run; the claim source is always the
    /// in-memory generator.
    pub fn to_run_config(&self) -> RunConfig {
        RunConfig {
            source: ClaimSource::Generated(self.generator.clone()),
            seed: self.seed,
            rate_per_sec: self.rate_per_sec,
            policy: self.policy,
            payers: self.payers.clone(),
            faults: self.faults,
            payer_faults: self.payer_faults.clone(),
            progress: None,
        }
    }

    /// The run banner as (label, value) rows — one source of truth for the
    /// plain stdout print, the TUI's running screen, and tests.
    pub fn banner_rows(&self) -> Vec<(String, String)> {
        let interval_hint = if self.rate_per_sec < 0.1 {
            format!(" (one every {})", human_virtual(1.0 / self.rate_per_sec))
        } else {
            String::new()
        };
        // Displayed per virtual minute — nobody thinks in 0.0004 claims/second.
        let per_min = self.rate_per_sec * 60.0;
        let rate_display = match per_min {
            r if r >= 10.0 => format!("{r:.0}"),
            r if r >= 1.0 => format!("{r:.1}"),
            r => format!("{r:.2}"),
        };
        let mut rows = vec![
            ("claims".to_string(), self.generator.summary(self.seed)),
            ("seed".to_string(), self.seed.to_string()),
            (
                "ingest rate".to_string(),
                format!("{rate_display} claims per virtual minute{interval_hint}"),
            ),
            (
                "retry policy".to_string(),
                format!(
                    "{} attempts · {} timeout · {} backoff base",
                    self.policy.max_attempts,
                    human_virtual(self.policy.timeout.as_secs_f64()),
                    human_virtual(self.policy.backoff_base.as_secs_f64()),
                ),
            ),
            (
                "faults".to_string(),
                format!("[{}] {}", self.preset.name(), self.faults.summary()),
            ),
        ];
        for payer in PayerId::ALL {
            let p = &self.payers[&payer];
            let route = match self.payer_faults.get(&payer) {
                Some(profile) => format!("  route: {}", profile.summary()),
                None => String::new(),
            };
            // Aligned columns — ten of these rows in a stack must scan as a table.
            rows.push((
                payer.as_str().to_string(),
                format!(
                    "{:<8} denies {:>2.0}%  copay {:>6}{route}",
                    format!(
                        "{}–{}",
                        human_short(p.min_response_time_secs),
                        human_short(p.max_response_time_secs)
                    ),
                    p.denial_rate * 100.0,
                    p.copay.to_string(),
                ),
            ));
        }
        rows
    }
}

/// Compact virtual-duration for dense table/banner lines ("3d", "4.5h").
pub fn human_short(secs: f64) -> String {
    match secs {
        s if s >= 86_400.0 => format!("{:.0}d", s / 86_400.0),
        s if s >= 3_600.0 => format!("{:.1}h", s / 3_600.0),
        s if s >= 60.0 => format!("{:.1}m", s / 60.0),
        s => format!("{s:.0}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_the_messy_demo() {
        let cfg = SimConfig::default();
        assert_eq!(cfg.preset, Preset::Messy);
        assert!(cfg.faults.forward_drop_rate > 0.0);
        assert!(cfg.payer_faults.contains_key(&PayerId::Anthem));
        assert_eq!(cfg.generator.count, 10_000);
    }

    #[test]
    fn presets_rewrite_rather_than_layer() {
        let mut cfg = SimConfig::default();
        cfg.faults.forward_drop_rate = 0.99;
        cfg.apply_preset(Preset::Honest);
        assert_eq!(cfg.faults.forward_drop_rate, 0.0, "manual edit discarded");
        assert!(cfg.payer_faults.is_empty());
        assert_eq!(cfg.preset, Preset::Honest);
    }

    #[test]
    fn preset_cycle_covers_custom() {
        assert_eq!(Preset::Honest.cycled(1), Preset::Messy);
        assert_eq!(Preset::Honest.cycled(-1), Preset::Chaos);
        assert_eq!(Preset::Custom.cycled(1), Preset::Honest);
        assert_eq!(Preset::Custom.cycled(-1), Preset::Chaos);
    }

    #[test]
    fn to_run_config_uses_the_generator_source() {
        let cfg = SimConfig::default();
        let rc = cfg.to_run_config();
        assert!(matches!(rc.source, ClaimSource::Generated(_)));
        assert_eq!(rc.seed, cfg.seed);
        assert_eq!(rc.policy.max_attempts, cfg.policy.max_attempts);
    }
}

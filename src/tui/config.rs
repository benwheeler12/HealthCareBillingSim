//! The configuration form: every knob of the next run on one scrollable
//! screen — generation, simulation, clearinghouse faults, retry policy, and
//! the ten payer personalities. ↑/↓ move the cursor, ←/→ step the selected
//! value (clamped, per-field step sizes), Enter on a payer row expands its
//! personality and route faults, Enter anywhere else starts the simulation.
//!
//! The same widget serves twice: the standalone screen the program opens on,
//! and dashboard pane 5 after a run — same struct, same keys, so what you
//! learned at startup is what you use to kick off the next run.

use std::collections::HashSet;

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use healthcare_billing_sim::domain::{Money, PayerId, human_virtual};
use healthcare_billing_sim::sim::faults::FaultProfile;
use healthcare_billing_sim::simconfig::{Preset, SimConfig, human_short};

use super::theme::{self, ACCENT, bold, dim};

/// One virtual day, the natural unit for the duration knobs.
const VDAY: f64 = 86_400.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Row {
    Section(&'static str),
    /// The explicit "start simulation" action row.
    Start,
    Field(FieldId),
    /// Collapsed payer summary; Enter expands it into its fields.
    PayerHeader(PayerId),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FieldId {
    Count,
    MalformedRate,
    Drift,
    GenSeed,
    Seed,
    Rate,
    Threads,
    Preset,
    /// `None` payer edits the global fault profile; `Some` edits that
    /// payer's clearinghouse route override.
    Fault(Option<PayerId>, FaultField),
    Policy(PolicyField),
    Payer(PayerId, PayerField),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FaultField {
    ForwardDrop,
    ReturnDrop,
    Duplicate,
    DelayRate,
    MaxDelay,
    Dishonest,
    LineDrop,
    CorruptId,
    Garbage,
}

const FAULT_FIELDS: [FaultField; 9] = [
    FaultField::ForwardDrop,
    FaultField::ReturnDrop,
    FaultField::Duplicate,
    FaultField::DelayRate,
    FaultField::MaxDelay,
    FaultField::Dishonest,
    FaultField::LineDrop,
    FaultField::CorruptId,
    FaultField::Garbage,
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PolicyField {
    MaxAttempts,
    Timeout,
    Backoff,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PayerField {
    MinResponse,
    MaxResponse,
    DenialRate,
    NotAllowedBps,
    DeductibleBps,
    CoinsuranceBps,
    Copay,
}

const PAYER_FIELDS: [PayerField; 7] = [
    PayerField::MinResponse,
    PayerField::MaxResponse,
    PayerField::DenialRate,
    PayerField::NotAllowedBps,
    PayerField::DeductibleBps,
    PayerField::CoinsuranceBps,
    PayerField::Copay,
];

/// Cursor + expansion state; the row list itself is derived fresh from the
/// config every time, so there is nothing to keep in sync. The default
/// cursor, 0, is the start row.
#[derive(Default)]
pub struct Form {
    pub cursor: usize,
    expanded: HashSet<PayerId>,
    scroll: u16,
}

/// What Enter on the current row means.
pub enum EnterAction {
    StartRun,
    ToggledPayer,
}

impl Form {
    pub fn rows(&self, _cfg: &SimConfig) -> Vec<Row> {
        let mut rows = vec![
            Row::Start,
            Row::Section("generation"),
            Row::Field(FieldId::Count),
            Row::Field(FieldId::MalformedRate),
            Row::Field(FieldId::Drift),
            Row::Field(FieldId::GenSeed),
            Row::Section("simulation"),
            Row::Field(FieldId::Seed),
            Row::Field(FieldId::Rate),
            Row::Field(FieldId::Threads),
            Row::Section("clearinghouse faults"),
            Row::Field(FieldId::Preset),
        ];
        rows.extend(FAULT_FIELDS.map(|f| Row::Field(FieldId::Fault(None, f))));
        rows.extend([
            Row::Section("retry policy"),
            Row::Field(FieldId::Policy(PolicyField::MaxAttempts)),
            Row::Field(FieldId::Policy(PolicyField::Timeout)),
            Row::Field(FieldId::Policy(PolicyField::Backoff)),
            Row::Section("payers — enter expands personality + route faults"),
        ]);
        for payer in PayerId::ALL {
            rows.push(Row::PayerHeader(payer));
            if self.expanded.contains(&payer) {
                rows.extend(PAYER_FIELDS.map(|f| Row::Field(FieldId::Payer(payer, f))));
                rows.extend(FAULT_FIELDS.map(|f| Row::Field(FieldId::Fault(Some(payer), f))));
            }
        }
        rows
    }

    /// Move the cursor by `delta`, skipping section headers.
    pub fn move_cursor(&mut self, cfg: &SimConfig, delta: isize) {
        let rows = self.rows(cfg);
        let mut cursor = self.cursor as isize;
        let step = delta.signum();
        let mut remaining = delta.abs();
        while remaining > 0 {
            let mut next = cursor + step;
            while (0..rows.len() as isize).contains(&next)
                && matches!(rows[next as usize], Row::Section(_))
            {
                next += step;
            }
            if !(0..rows.len() as isize).contains(&next) {
                break;
            }
            cursor = next;
            remaining -= 1;
        }
        self.cursor = cursor.clamp(0, rows.len() as isize - 1) as usize;
    }

    /// ←/→ on the selected row.
    pub fn adjust(&mut self, cfg: &mut SimConfig, dir: i8) {
        let rows = self.rows(cfg);
        match rows.get(self.cursor) {
            Some(Row::Field(id)) => adjust_field(cfg, *id, dir),
            // ←/→ on a payer row toggles too — arrow keys are how the rest
            // of the form changes values, so honor them here.
            Some(Row::PayerHeader(payer)) => {
                if !self.expanded.remove(payer) {
                    self.expanded.insert(*payer);
                }
            }
            _ => {}
        }
    }

    /// Enter: expand/collapse a payer row, start the run anywhere else.
    pub fn enter(&mut self, cfg: &SimConfig) -> EnterAction {
        let rows = self.rows(cfg);
        match rows.get(self.cursor) {
            Some(Row::PayerHeader(payer)) => {
                if !self.expanded.remove(payer) {
                    self.expanded.insert(*payer);
                }
                EnterAction::ToggledPayer
            }
            _ => EnterAction::StartRun,
        }
    }

    /// Collapse the payer section the cursor sits in; true if one collapsed.
    pub fn escape(&mut self, cfg: &SimConfig) -> bool {
        let rows = self.rows(cfg);
        let payer = match rows.get(self.cursor) {
            Some(Row::PayerHeader(p)) => Some(*p),
            Some(Row::Field(FieldId::Payer(p, _)) | Row::Field(FieldId::Fault(Some(p), _))) => {
                Some(*p)
            }
            _ => None,
        };
        match payer {
            Some(p) if self.expanded.remove(&p) => {
                // Land the cursor back on the collapsed header.
                self.cursor = self
                    .rows(cfg)
                    .iter()
                    .position(|r| *r == Row::PayerHeader(p))
                    .unwrap_or(0);
                true
            }
            _ => false,
        }
    }
}

/// The resolved fault profile a row edits: global, or the payer's route
/// (created from the global profile on first touch).
fn fault_profile_mut(cfg: &mut SimConfig, payer: Option<PayerId>) -> &mut FaultProfile {
    match payer {
        None => &mut cfg.faults,
        Some(p) => {
            let base = cfg.faults;
            cfg.payer_faults.entry(p).or_insert(base)
        }
    }
}

fn adjust_field(cfg: &mut SimConfig, id: FieldId, dir: i8) {
    let d = dir as f64;
    match id {
        FieldId::Count => {
            let count = &mut cfg.generator.count;
            *count = if dir > 0 {
                (*count * 2).min(1_000_000)
            } else {
                (*count / 2).max(100)
            };
        }
        FieldId::MalformedRate => step_rate(&mut cfg.generator.malformed_rate, d),
        FieldId::Drift => cfg.generator.drift = !cfg.generator.drift,
        FieldId::GenSeed => {
            // Follows the master seed until stepped away; stepping back onto
            // the master value re-links it.
            let resolved = cfg.generator.resolved_seed(cfg.seed);
            let next = if dir > 0 {
                resolved.saturating_add(1)
            } else {
                resolved.saturating_sub(1)
            };
            cfg.generator.seed = (next != cfg.seed).then_some(next);
        }
        FieldId::Seed => {
            cfg.seed = if dir > 0 {
                cfg.seed.saturating_add(1)
            } else {
                cfg.seed.saturating_sub(1)
            };
        }
        FieldId::Rate => cfg.rate_per_sec = step_scale(cfg.rate_per_sec, dir, 1e-6, 100.0),
        FieldId::Threads => {
            cfg.threads = (cfg.threads as isize + dir as isize).clamp(0, 64) as usize;
        }
        FieldId::Preset => cfg.apply_preset(cfg.preset.cycled(dir)),
        FieldId::Fault(payer, field) => {
            let profile = fault_profile_mut(cfg, payer);
            match field {
                FaultField::ForwardDrop => step_rate(&mut profile.forward_drop_rate, d),
                FaultField::ReturnDrop => step_rate(&mut profile.return_drop_rate, d),
                FaultField::Duplicate => step_rate(&mut profile.duplicate_rate, d),
                FaultField::DelayRate => step_rate(&mut profile.extra_delay_rate, d),
                FaultField::MaxDelay => {
                    profile.max_extra_delay_secs = step_scale(
                        profile.max_extra_delay_secs.max(VDAY / 2.0),
                        dir,
                        VDAY / 2.0,
                        365.0 * VDAY,
                    );
                }
                FaultField::Dishonest => step_rate(&mut profile.dishonest_adjudication_rate, d),
                FaultField::LineDrop => step_rate(&mut profile.line_drop_rate, d),
                FaultField::CorruptId => step_rate(&mut profile.corrupt_claim_id_rate, d),
                FaultField::Garbage => step_rate(&mut profile.corrupt_remittance_rate, d),
            }
            cfg.preset = Preset::Custom;
        }
        FieldId::Policy(field) => {
            match field {
                PolicyField::MaxAttempts => {
                    cfg.policy.max_attempts =
                        (cfg.policy.max_attempts as i64 + dir as i64).clamp(1, 10) as u32;
                }
                PolicyField::Timeout => {
                    cfg.policy.timeout = std::time::Duration::from_secs_f64(step_scale(
                        cfg.policy.timeout.as_secs_f64(),
                        dir,
                        60.0,
                        365.0 * VDAY,
                    ));
                }
                PolicyField::Backoff => {
                    cfg.policy.backoff_base = std::time::Duration::from_secs_f64(step_scale(
                        cfg.policy.backoff_base.as_secs_f64(),
                        dir,
                        60.0,
                        365.0 * VDAY,
                    ));
                }
            }
            cfg.preset = Preset::Custom;
        }
        FieldId::Payer(payer, field) => {
            let p = cfg.payers.get_mut(&payer).expect("all payers configured");
            match field {
                PayerField::MinResponse => {
                    p.min_response_time_secs = step_scale(
                        p.min_response_time_secs,
                        dir,
                        60.0,
                        p.max_response_time_secs,
                    );
                }
                PayerField::MaxResponse => {
                    p.max_response_time_secs = step_scale(
                        p.max_response_time_secs,
                        dir,
                        p.min_response_time_secs,
                        365.0 * VDAY,
                    );
                }
                PayerField::DenialRate => step_rate(&mut p.denial_rate, d),
                PayerField::NotAllowedBps => step_bps(&mut p.max_not_allowed_bps, dir),
                PayerField::DeductibleBps => step_bps(&mut p.max_deductible_bps, dir),
                PayerField::CoinsuranceBps => step_bps(&mut p.coinsurance_bps, dir),
                PayerField::Copay => {
                    let cents = (p.copay.cents() + dir as i64 * 500).clamp(0, 50_000);
                    p.copay = Money::from_cents(cents);
                }
            }
            cfg.preset = Preset::Custom;
        }
    }
}

/// Probabilities step a percentage point at a time, exactly.
fn step_rate(rate: &mut f64, d: f64) {
    *rate = (((*rate * 100.0).round() + d) / 100.0).clamp(0.0, 1.0);
}

/// Scale-free knobs (rates per second, durations) step ×2 / ÷2 — one key
/// spans seconds to months in a dozen presses.
fn step_scale(value: f64, dir: i8, min: f64, max: f64) -> f64 {
    let next = if dir > 0 { value * 2.0 } else { value / 2.0 };
    next.clamp(min, max)
}

fn step_bps(bps: &mut u32, dir: i8) {
    *bps = (*bps as i64 + dir as i64 * 250).clamp(0, 10_000) as u32;
}

fn label(id: FieldId) -> String {
    match id {
        FieldId::Count => "claims to generate".into(),
        FieldId::MalformedRate => "malformed rate".into(),
        FieldId::Drift => "payer-mix drift".into(),
        FieldId::GenSeed => "generator seed".into(),
        FieldId::Seed => "seed".into(),
        FieldId::Rate => "ingest rate".into(),
        FieldId::Threads => "worker threads".into(),
        FieldId::Preset => "preset".into(),
        FieldId::Fault(_, field) => match field {
            FaultField::ForwardDrop => "forward drop".into(),
            FaultField::ReturnDrop => "return drop".into(),
            FaultField::Duplicate => "duplicates".into(),
            FaultField::DelayRate => "delays".into(),
            FaultField::MaxDelay => "max delay".into(),
            FaultField::Dishonest => "dishonest adjudication".into(),
            FaultField::LineDrop => "line drops".into(),
            FaultField::CorruptId => "corrupt claim ids".into(),
            FaultField::Garbage => "garbage remittances".into(),
        },
        FieldId::Policy(field) => match field {
            PolicyField::MaxAttempts => "max attempts".into(),
            PolicyField::Timeout => "timeout".into(),
            PolicyField::Backoff => "backoff base".into(),
        },
        FieldId::Payer(_, field) => match field {
            PayerField::MinResponse => "min response".into(),
            PayerField::MaxResponse => "max response".into(),
            PayerField::DenialRate => "denial rate".into(),
            PayerField::NotAllowedBps => "max not-allowed".into(),
            PayerField::DeductibleBps => "max deductible".into(),
            PayerField::CoinsuranceBps => "coinsurance".into(),
            PayerField::Copay => "copay".into(),
        },
    }
}

fn pct(rate: f64) -> String {
    format!("{:.0}%", rate * 100.0)
}

fn value(cfg: &SimConfig, id: FieldId) -> String {
    match id {
        FieldId::Count => theme::thousands(cfg.generator.count as u64),
        FieldId::MalformedRate => pct(cfg.generator.malformed_rate),
        FieldId::Drift => {
            if cfg.generator.drift {
                "on — slow payers early, fast payers late".into()
            } else {
                "off — uniform payer mix".into()
            }
        }
        FieldId::GenSeed => match cfg.generator.seed {
            Some(seed) => format!("{seed} (pinned)"),
            None => format!("follows seed ({})", cfg.seed),
        },
        FieldId::Seed => cfg.seed.to_string(),
        FieldId::Rate => {
            let per_min = cfg.rate_per_sec * 60.0;
            let rate = match per_min {
                r if r >= 10.0 => format!("{r:.0}"),
                r if r >= 1.0 => format!("{r:.1}"),
                r => format!("{r:.2}"),
            };
            format!("{rate} claims per virtual minute")
        }
        FieldId::Threads => match cfg.threads {
            0 => "auto (one per core)".into(),
            n => n.to_string(),
        },
        FieldId::Preset => cfg.preset.name().into(),
        FieldId::Fault(payer, field) => {
            let profile = match payer {
                None => &cfg.faults,
                Some(p) => cfg.payer_faults.get(&p).unwrap_or(&cfg.faults),
            };
            match field {
                FaultField::ForwardDrop => pct(profile.forward_drop_rate),
                FaultField::ReturnDrop => pct(profile.return_drop_rate),
                FaultField::Duplicate => pct(profile.duplicate_rate),
                FaultField::DelayRate => pct(profile.extra_delay_rate),
                FaultField::MaxDelay => human_virtual(profile.max_extra_delay_secs),
                FaultField::Dishonest => pct(profile.dishonest_adjudication_rate),
                FaultField::LineDrop => pct(profile.line_drop_rate),
                FaultField::CorruptId => pct(profile.corrupt_claim_id_rate),
                FaultField::Garbage => pct(profile.corrupt_remittance_rate),
            }
        }
        FieldId::Policy(field) => match field {
            PolicyField::MaxAttempts => cfg.policy.max_attempts.to_string(),
            PolicyField::Timeout => human_virtual(cfg.policy.timeout.as_secs_f64()),
            PolicyField::Backoff => human_virtual(cfg.policy.backoff_base.as_secs_f64()),
        },
        FieldId::Payer(payer, field) => {
            let p = &cfg.payers[&payer];
            match field {
                PayerField::MinResponse => human_virtual(p.min_response_time_secs),
                PayerField::MaxResponse => human_virtual(p.max_response_time_secs),
                PayerField::DenialRate => pct(p.denial_rate),
                PayerField::NotAllowedBps => format!("{}% of billed", p.max_not_allowed_bps / 100),
                PayerField::DeductibleBps => format!("{}% of allowed", p.max_deductible_bps / 100),
                PayerField::CoinsuranceBps => format!("{}%", p.coinsurance_bps / 100),
                PayerField::Copay => p.copay.to_string(),
            }
        }
    }
}

/// Render the form. `active` is true when the form owns the arrow keys —
/// always on the standalone configure screen, only while grabbed (Enter) on
/// the dashboard pane.
pub fn draw(
    frame: &mut ratatui::Frame,
    area: Rect,
    cfg: &SimConfig,
    form: &mut Form,
    active: bool,
) {
    let rows = form.rows(cfg);
    form.cursor = form.cursor.min(rows.len().saturating_sub(1));
    let mut lines: Vec<Line> = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let selected = active && i == form.cursor;
        let marker = if selected { "▸ " } else { "  " };
        let line = match row {
            Row::Section(title) => {
                Line::from(Span::styled(format!("  {title}"), theme::accent_bold()))
            }
            Row::Start => {
                let style = if selected {
                    theme::bold().fg(ratatui::style::Color::Black).bg(ACCENT)
                } else {
                    theme::accent_bold()
                };
                Line::from(vec![
                    Span::raw(marker),
                    Span::styled(" ▶ start simulation ", style),
                    Span::styled("  enter runs this configuration", dim()),
                ])
            }
            Row::Field(id) => {
                let indent = match id {
                    FieldId::Payer(..) | FieldId::Fault(Some(_), _) => "    ",
                    _ => "",
                };
                let value_style = if selected {
                    theme::bold().fg(ratatui::style::Color::Black).bg(ACCENT)
                } else {
                    Style::default()
                };
                Line::from(vec![
                    Span::raw(marker),
                    Span::styled(format!("{indent}{:<24} ", label(*id)), bold()),
                    Span::styled(format!(" {} ", value(cfg, *id)), value_style),
                ])
            }
            Row::PayerHeader(payer) => {
                let p = &cfg.payers[payer];
                let route = match cfg.payer_faults.get(payer) {
                    Some(profile) => format!("  route: {}", profile.summary()),
                    None => String::new(),
                };
                let arrow = if form.expanded.contains(payer) {
                    "▾"
                } else {
                    "▸"
                };
                let name_style = if selected {
                    theme::bold().fg(ratatui::style::Color::Black).bg(ACCENT)
                } else {
                    bold()
                };
                Line::from(vec![
                    Span::raw(marker),
                    Span::styled(format!(" {arrow} {:<22} ", payer.as_str()), name_style),
                    Span::raw(format!(
                        "{:<8} denies {:>2.0}%  copay {:>6}{route}",
                        format!(
                            "{}–{}",
                            human_short(p.min_response_time_secs),
                            human_short(p.max_response_time_secs)
                        ),
                        p.denial_rate * 100.0,
                        p.copay.to_string(),
                    )),
                ])
            }
        };
        lines.push(line);
    }

    // Keep the cursor visible: one derived line per row, so row index ==
    // line index and the scroll window is simple arithmetic.
    let height = area.height.saturating_sub(2) as usize; // panel borders
    let cursor_line = form.cursor;
    if cursor_line < form.scroll as usize {
        form.scroll = cursor_line as u16;
    } else if height > 0 && cursor_line >= form.scroll as usize + height {
        form.scroll = (cursor_line + 1 - height) as u16;
    }

    let hint = if active {
        "configuration — ↑/↓ field · ←/→ adjust · enter start (payer rows: expand) · esc back"
    } else {
        "configuration — enter to edit"
    };
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((form.scroll, 0))
            .block(theme::panel(hint)),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field_cursor(form: &mut Form, cfg: &SimConfig, id: FieldId) {
        form.cursor = form
            .rows(cfg)
            .iter()
            .position(|r| *r == Row::Field(id))
            .expect("field row exists");
    }

    #[test]
    fn rates_step_by_percentage_points_and_clamp() {
        let mut cfg = SimConfig::default();
        let mut form = Form::default();
        field_cursor(&mut form, &cfg, FieldId::MalformedRate);
        cfg.generator.malformed_rate = 0.0;
        form.adjust(&mut cfg, -1);
        assert_eq!(cfg.generator.malformed_rate, 0.0, "clamped at zero");
        form.adjust(&mut cfg, 1);
        form.adjust(&mut cfg, 1);
        assert!((cfg.generator.malformed_rate - 0.02).abs() < 1e-12);
    }

    #[test]
    fn editing_a_fault_flips_the_preset_to_custom() {
        let mut cfg = SimConfig::default();
        let mut form = Form::default();
        assert_eq!(cfg.preset, Preset::Messy);
        field_cursor(
            &mut form,
            &cfg,
            FieldId::Fault(None, FaultField::ForwardDrop),
        );
        form.adjust(&mut cfg, 1);
        assert_eq!(cfg.preset, Preset::Custom);
        // Cycling the preset from custom rewrites the tunables wholesale.
        field_cursor(&mut form, &cfg, FieldId::Preset);
        form.adjust(&mut cfg, 1);
        assert_eq!(cfg.preset, Preset::Honest);
        assert_eq!(cfg.faults.forward_drop_rate, 0.0);
    }

    #[test]
    fn generator_seed_follows_master_until_stepped_away() {
        let mut cfg = SimConfig::default();
        let mut form = Form::default();
        assert_eq!(cfg.generator.seed, None);
        field_cursor(&mut form, &cfg, FieldId::GenSeed);
        form.adjust(&mut cfg, 1);
        assert_eq!(cfg.generator.seed, Some(cfg.seed + 1));
        form.adjust(&mut cfg, -1);
        assert_eq!(cfg.generator.seed, None, "stepping back re-links it");
    }

    #[test]
    fn enter_expands_payers_and_starts_everywhere_else() {
        let cfg = SimConfig::default();
        let mut form = Form::default();
        assert!(matches!(form.enter(&cfg), EnterAction::StartRun));

        let header = form
            .rows(&cfg)
            .iter()
            .position(|r| matches!(r, Row::PayerHeader(_)))
            .expect("payer rows");
        form.cursor = header;
        let before = form.rows(&cfg).len();
        assert!(matches!(form.enter(&cfg), EnterAction::ToggledPayer));
        let after = form.rows(&cfg).len();
        assert_eq!(after - before, PAYER_FIELDS.len() + FAULT_FIELDS.len());

        // Esc from inside the expanded section collapses it and lands the
        // cursor back on the header.
        form.cursor = header + 3;
        assert!(form.escape(&cfg));
        assert_eq!(form.rows(&cfg).len(), before);
        assert_eq!(form.cursor, header);
    }

    #[test]
    fn cursor_skips_section_headers() {
        let cfg = SimConfig::default();
        let mut form = Form::default();
        let rows = form.rows(&cfg);
        for _ in 0..rows.len() {
            form.move_cursor(&cfg, 1);
            assert!(
                !matches!(form.rows(&cfg)[form.cursor], Row::Section(_)),
                "cursor must never rest on a section header"
            );
        }
    }

    #[test]
    fn editing_a_payer_route_creates_an_override_from_the_global_profile() {
        let mut cfg = SimConfig::default();
        cfg.apply_preset(Preset::Honest);
        assert!(cfg.payer_faults.is_empty());
        let mut form = Form::default();
        // Expand medicare, then bump its route's forward drop.
        let header = form
            .rows(&cfg)
            .iter()
            .position(|r| *r == Row::PayerHeader(PayerId::Medicare))
            .expect("medicare row");
        form.cursor = header;
        form.enter(&cfg);
        field_cursor(
            &mut form,
            &cfg,
            FieldId::Fault(Some(PayerId::Medicare), FaultField::ForwardDrop),
        );
        form.adjust(&mut cfg, 1);
        assert!((cfg.payer_faults[&PayerId::Medicare].forward_drop_rate - 0.01).abs() < 1e-12);
        assert_eq!(
            cfg.faults.forward_drop_rate, 0.0,
            "global profile untouched"
        );
    }
}

//! The Payer Scorecard pane: the old Scorecard and Denials panes merged into
//! one master–detail view. A graded league table of payers on top (derived
//! from remittance data alone); below it, the selected payer's configured
//! personality side by side with what the scorecard rediscovered, plus that
//! payer's denial breakdown by reason.

use std::collections::{BTreeMap, HashMap};

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Color;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};

use healthcare_billing_sim::RunOutput;
use healthcare_billing_sim::domain::{DenialReason, Money, PayerId};
use healthcare_billing_sim::reports::scorecard::PayerScore;
use healthcare_billing_sim::reports::{denial_breakdown, payer_scorecard};
use healthcare_billing_sim::sim::faults::FaultProfile;
use healthcare_billing_sim::sim::payer::PayerConfig;

use super::theme::{self, ACCENT, ALERT, BAD, GOOD, WARN, bold, dim, money, pct};

pub struct PayersView {
    rows: Vec<PayerRow>,
    pub table: TableState,
    denials: BTreeMap<(PayerId, DenialReason), (usize, Money)>,
    configs: HashMap<PayerId, PayerConfig>,
    routes: HashMap<PayerId, FaultProfile>,
    max_response: f64,
    max_denial: f64,
}

struct PayerRow {
    payer: PayerId,
    score: PayerScore,
    grade: Grade,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Grade {
    pub letter: char,
    color: Color,
}

pub fn build(
    output: &RunOutput,
    configs: HashMap<PayerId, PayerConfig>,
    routes: HashMap<PayerId, FaultProfile>,
) -> PayersView {
    let scores: Vec<(PayerId, PayerScore)> = payer_scorecard(&output.ledger).into_iter().collect();
    let mut graded: Vec<(f64, PayerRow)> = scores
        .iter()
        .zip(grade_all(&scores))
        .map(|(&(payer, score), (grade, composite))| {
            (
                composite,
                PayerRow {
                    payer,
                    score,
                    grade,
                },
            )
        })
        .collect();
    // League-table order: best composite first, name as the tiebreak.
    graded.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.payer.cmp(&b.1.payer))
    });
    let rows: Vec<PayerRow> = graded.into_iter().map(|(_, row)| row).collect();

    let max_response = rows
        .iter()
        .map(|r| r.score.avg_response_secs)
        .fold(0.0, f64::max);
    let max_denial = rows.iter().map(|r| r.score.denial_rate).fold(0.0, f64::max);
    let mut table = TableState::default();
    if !rows.is_empty() {
        table.select(Some(0));
    }
    PayersView {
        rows,
        table,
        denials: denial_breakdown(&output.ledger),
        configs,
        routes,
        max_response,
        max_denial,
    }
}

impl PayersView {
    pub fn move_selection(&mut self, delta: isize) {
        super::step(&mut self.table, self.rows.len(), delta);
    }

    fn selected(&self) -> Option<&PayerRow> {
        self.table.selected().and_then(|i| self.rows.get(i))
    }
}

/// Grade on the curve: min-max normalize denial rate, response time, and
/// paid/billed across the cohort; the composite is their mean (lower =
/// better). Returns (grade, composite) per payer, in input order.
fn grade_all(scores: &[(PayerId, PayerScore)]) -> Vec<(Grade, f64)> {
    let min_max = |f: fn(&PayerScore) -> f64| {
        let lo = scores
            .iter()
            .map(|(_, s)| f(s))
            .fold(f64::INFINITY, f64::min);
        let hi = scores
            .iter()
            .map(|(_, s)| f(s))
            .fold(f64::NEG_INFINITY, f64::max);
        (lo, hi)
    };
    let norm = |v: f64, (lo, hi): (f64, f64)| {
        if hi > lo { (v - lo) / (hi - lo) } else { 0.5 }
    };
    let denial_range = min_max(|s| s.denial_rate);
    let response_range = min_max(|s| s.avg_response_secs);
    let paid_range = min_max(|s| s.paid_to_billed);

    scores
        .iter()
        .map(|(_, s)| {
            let composite = (norm(s.denial_rate, denial_range)
                + norm(s.avg_response_secs, response_range)
                + (1.0 - norm(s.paid_to_billed, paid_range)))
                / 3.0;
            (grade_of(composite), composite)
        })
        .collect()
}

fn grade_of(composite: f64) -> Grade {
    match composite {
        c if c < 0.2 => Grade {
            letter: 'A',
            color: GOOD,
        },
        c if c < 0.4 => Grade {
            letter: 'B',
            color: Color::LightGreen,
        },
        c if c < 0.6 => Grade {
            letter: 'C',
            color: WARN,
        },
        c if c < 0.8 => Grade {
            letter: 'D',
            color: ALERT,
        },
        _ => Grade {
            letter: 'F',
            color: BAD,
        },
    }
}

pub fn draw(frame: &mut ratatui::Frame, area: Rect, view: &mut PayersView) {
    let [hint_area, table_area, detail_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(view.rows.len() as u16 + 3),
        Constraint::Min(5),
    ])
    .areas(area);

    frame.render_widget(
        theme::keys_hint(&[
            ("↑/↓", "pick a payer — the detail below follows"),
            (
                "grades",
                "on the curve: denial rate + response time + paid share",
            ),
        ]),
        hint_area,
    );

    let header = Row::new([
        "",
        "payer",
        "claims",
        "avg response",
        "denial rate",
        "paid / billed",
    ])
    .style(bold());
    let bar_w = 10usize;
    let rows: Vec<Row> = view
        .rows
        .iter()
        .map(|r| {
            let denial_frac = if view.max_denial > 0.0 {
                r.score.denial_rate / view.max_denial
            } else {
                0.0
            };
            let response_frac = if view.max_response > 0.0 {
                r.score.avg_response_secs / view.max_response
            } else {
                0.0
            };
            let mut denial_cell = theme::meter(denial_frac, bar_w, denial_color(denial_frac));
            denial_cell.push(Span::styled(
                format!(" {:>5}", pct(r.score.denial_rate)),
                bold(),
            ));
            let mut response_cell = theme::meter(response_frac, bar_w, ACCENT);
            response_cell.push(Span::raw(format!(
                " {:>6}",
                theme::human_compact(r.score.avg_response_secs)
            )));
            let mut paid_cell = theme::meter(r.score.paid_to_billed, bar_w, GOOD);
            paid_cell.push(Span::styled(
                format!(" {:>5}", pct(r.score.paid_to_billed)),
                bold(),
            ));
            Row::new(vec![
                Cell::from(Line::from(Span::styled(
                    format!(" {} ", r.grade.letter),
                    theme::bold().fg(r.grade.color),
                ))),
                Cell::from(r.payer.as_str().to_string()),
                Cell::from(theme::thousands(r.score.claims as u64)),
                Cell::from(Line::from(response_cell)),
                Cell::from(Line::from(denial_cell)),
                Cell::from(Line::from(paid_cell)),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(3),
            Constraint::Length(22),
            Constraint::Length(7),
            Constraint::Length(bar_w as u16 + 8),
            Constraint::Length(bar_w as u16 + 7),
            Constraint::Length(bar_w as u16 + 7),
        ],
    )
    .header(header)
    .block(
        theme::panel("Payer Scorecard — from remittance data alone, best to worst")
            .border_style(ratatui::style::Style::default().fg(ACCENT)),
    )
    .row_highlight_style(theme::bold().bg(Color::DarkGray))
    .highlight_symbol("▶ ");
    frame.render_stateful_widget(table, table_area, &mut view.table);

    let Some(selected) = view.selected() else {
        frame.render_widget(
            Paragraph::new("no remittances booked — nothing to grade")
                .block(theme::panel("Payer detail")),
            detail_area,
        );
        return;
    };
    let lines = detail_lines(selected, view);
    frame.render_widget(
        Paragraph::new(lines).block(theme::panel(format!(
            "{} — configured personality vs what the remittances revealed",
            selected.payer.as_str()
        ))),
        detail_area,
    );
}

fn denial_color(frac: f64) -> Color {
    match frac {
        f if f >= 0.75 => BAD,
        f if f >= 0.4 => WARN,
        _ => GOOD,
    }
}

/// The closed-loop demo, spelled out: the simulation configured this payer's
/// personality; the scorecard rediscovered it from remittance data alone.
fn detail_lines(row: &PayerRow, view: &PayersView) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(cfg) = view.configs.get(&row.payer) {
        lines.push(Line::from(vec![
            Span::styled(" configured ", theme::accent_bold()),
            Span::raw(format!(
                "responds in {}–{} · denies {:.0}% of lines · copay {}",
                theme::human_compact(cfg.min_response_time_secs),
                theme::human_compact(cfg.max_response_time_secs),
                cfg.denial_rate * 100.0,
                money(cfg.copay),
            )),
        ]));
        if let Some(route) = view.routes.get(&row.payer) {
            lines.push(Line::from(vec![
                Span::styled(" route      ", theme::accent_bold()),
                Span::styled(route.summary(), dim()),
            ]));
        }
    }
    lines.push(Line::from(vec![
        Span::styled(" observed   ", theme::accent_bold()),
        Span::raw(format!(
            "first answer in {} avg · denies {} of adjudicated lines · pays {} of billed",
            theme::human_compact(row.score.avg_response_secs),
            pct(row.score.denial_rate),
            pct(row.score.paid_to_billed),
        )),
    ]));
    lines.push(Line::default());

    let reasons: Vec<(&DenialReason, &(usize, Money))> = view
        .denials
        .iter()
        .filter(|((payer, _), _)| *payer == row.payer)
        .map(|((_, reason), stats)| (reason, stats))
        .collect();
    if reasons.is_empty() {
        lines.push(Line::from(Span::styled(
            " no denial lines booked for this payer",
            dim(),
        )));
        return lines;
    }
    let denied_total: Money = reasons.iter().map(|(_, (_, m))| *m).sum();
    lines.push(Line::from(vec![
        Span::styled(" denied lines by reason — ", bold()),
        Span::styled(money(denied_total), theme::bold().fg(BAD)),
        Span::styled(" billed on denied lines", bold()),
    ]));
    for (reason, (count, amount)) in reasons {
        let share = if denied_total.cents() > 0 {
            amount.cents() as f64 / denied_total.cents() as f64
        } else {
            0.0
        };
        let mut spans = vec![Span::raw(format!(
            "   {:<22} {:>6} lines {:>14}  ",
            reason.to_string(),
            theme::thousands(*count as u64),
            money(*amount),
        ))];
        spans.extend(theme::meter(share, 16, BAD));
        spans.push(Span::styled(format!(" {}", pct(share)), dim()));
        lines.push(Line::from(spans));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(denial: f64, response: f64, paid: f64) -> PayerScore {
        PayerScore {
            claims: 10,
            avg_response_secs: response,
            denial_rate: denial,
            paid_to_billed: paid,
        }
    }

    #[test]
    fn best_payer_outgrades_worst() {
        let scores = vec![
            (PayerId::Medicare, score(0.05, 3.0, 0.8)),
            (PayerId::Anthem, score(0.15, 20.0, 0.5)),
            (PayerId::Aetna, score(0.10, 8.0, 0.65)),
        ];
        let graded = grade_all(&scores);
        assert_eq!(graded[0].0.letter, 'A'); // best on every axis
        assert_eq!(graded[1].0.letter, 'F'); // worst on every axis
        assert!(graded[0].1 < graded[2].1 && graded[2].1 < graded[1].1);
    }

    #[test]
    fn identical_cohort_grades_mid_curve() {
        let scores = vec![
            (PayerId::Medicare, score(0.1, 5.0, 0.6)),
            (PayerId::Aetna, score(0.1, 5.0, 0.6)),
        ];
        for (grade, composite) in grade_all(&scores) {
            assert_eq!(grade.letter, 'C');
            assert!((composite - 0.5).abs() < 1e-9);
        }
    }
}

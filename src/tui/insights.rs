//! The Provider Insights pane: a master–detail view. Providers on the left
//! (biggest book of open money first) drive the claims table on the right;
//! Tab or Enter hops focus between the two, c/a/r re-sorts the claims, and
//! Enter on a claim opens its full audit trail.

use std::collections::HashMap;

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};

use healthcare_billing_sim::domain::Money;
use healthcare_billing_sim::reports::chase::ChaseItem;

use super::theme::{self, ACCENT, ALERT, WARN, bold, dim, money};

/// Master–detail state: a provider list on the left drives a claims table on
/// the right; Tab (or Enter on a provider) moves focus right, Tab toggles back.
pub struct Insights {
    /// Aggregated per organization, sorted by total outstanding desc.
    providers: Vec<ProviderRow>,
    pub provider_table: TableState,
    /// The selected provider's open claims: indices into the chase list,
    /// ordered by the active sort.
    pub claims: Vec<usize>,
    pub claims_table: TableState,
    pub focus: Focus,
    sort: SortKey,
    sort_desc: bool,
}

struct ProviderRow {
    name: String,
    open: usize,
    outstanding: Money,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Providers,
    Claims,
}

/// Sort order for the selected provider's claims table.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Cost,
    Age,
    Risk,
}

impl SortKey {
    fn label(self) -> &'static str {
        match self {
            SortKey::Cost => "cost",
            SortKey::Age => "age",
            SortKey::Risk => "risk",
        }
    }
}

impl Insights {
    pub fn build(chase: &[ChaseItem]) -> Insights {
        // Aggregate per organization: open-claim count and total outstanding.
        let mut totals: HashMap<&str, (usize, Money)> = HashMap::new();
        for item in chase {
            let entry = totals
                .entry(item.provider.as_str())
                .or_insert((0, Money::ZERO));
            entry.0 += 1;
            entry.1 += item.outstanding;
        }
        let mut providers: Vec<ProviderRow> = totals
            .into_iter()
            .map(|(name, (open, outstanding))| ProviderRow {
                name: name.to_string(),
                open,
                outstanding,
            })
            .collect();
        providers.sort_by(|a, b| {
            b.outstanding
                .cmp(&a.outstanding)
                .then_with(|| a.name.cmp(&b.name))
        });

        let mut provider_table = TableState::default();
        if !providers.is_empty() {
            provider_table.select(Some(0));
        }
        let mut insights = Insights {
            providers,
            provider_table,
            claims: Vec::new(),
            claims_table: TableState::default(),
            focus: Focus::Providers,
            sort: SortKey::Risk,
            sort_desc: true,
        };
        insights.rebuild_claims(chase);
        insights
    }

    /// Move the selection in whichever list has focus; moving the provider
    /// selection re-derives that provider's claims.
    pub fn move_selection(&mut self, delta: isize, chase: &[ChaseItem]) {
        match self.focus {
            Focus::Providers => {
                if super::step(&mut self.provider_table, self.providers.len(), delta) {
                    self.rebuild_claims(chase);
                }
            }
            Focus::Claims => {
                super::step(&mut self.claims_table, self.claims.len(), delta);
            }
        }
    }

    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Providers => Focus::Claims,
            Focus::Claims => Focus::Providers,
        };
    }

    /// Toggle direction when the active key is pressed again; switch keys
    /// descending-first otherwise.
    pub fn set_sort(&mut self, key: SortKey, chase: &[ChaseItem]) {
        if self.sort == key {
            self.sort_desc = !self.sort_desc;
        } else {
            self.sort = key;
            self.sort_desc = true;
        }
        self.rebuild_claims(chase);
    }

    fn selected_provider(&self) -> Option<&ProviderRow> {
        self.provider_table
            .selected()
            .and_then(|i| self.providers.get(i))
    }

    /// Re-derive the claims list for the selected provider under the active
    /// sort, and reset the claim selection to the top.
    fn rebuild_claims(&mut self, chase: &[ChaseItem]) {
        let Some(provider) = self.selected_provider().map(|p| p.name.clone()) else {
            self.claims.clear();
            self.claims_table.select(None);
            return;
        };
        self.claims = chase
            .iter()
            .enumerate()
            .filter(|(_, item)| item.provider == provider)
            .map(|(idx, _)| idx)
            .collect();
        let key = self.sort;
        self.claims.sort_by(|&a, &b| {
            let (a, b) = (&chase[a], &chase[b]);
            let ordering = match key {
                SortKey::Cost => a.outstanding.cmp(&b.outstanding),
                SortKey::Age => a.age.cmp(&b.age),
                SortKey::Risk => a
                    .risk()
                    .partial_cmp(&b.risk())
                    .unwrap_or(std::cmp::Ordering::Equal),
            };
            // Deterministic tiebreak keeps equal rows stable across resorts.
            ordering.then_with(|| a.claim_id.cmp(&b.claim_id))
        });
        if self.sort_desc {
            self.claims.reverse();
        }
        self.claims_table
            .select((!self.claims.is_empty()).then_some(0));
    }
}

pub fn draw(frame: &mut ratatui::Frame, area: Rect, ins: &mut Insights, chase: &[ChaseItem]) {
    let [hint_area, body] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(3)]).areas(area);
    frame.render_widget(
        theme::keys_hint(&[
            ("↑/↓", "select"),
            ("tab / enter", "hop to the other table"),
            ("c a r", "sort claims by cost/age/risk (again flips)"),
            ("enter", "on a claim: full audit trail"),
        ]),
        hint_area,
    );

    if ins.providers.is_empty() {
        frame.render_widget(
            Paragraph::new("nothing outstanding — all receivables booked")
                .block(theme::panel("Provider Insights")),
            body,
        );
        return;
    }
    let [left, right] =
        Layout::horizontal([Constraint::Length(46), Constraint::Min(40)]).areas(body);

    let focus_style = Style::default().fg(ACCENT);
    let blur_style = theme::dim();
    let highlight = theme::bold().bg(Color::DarkGray);

    // Left: the providers, biggest book of open money first.
    let provider_rows: Vec<Row> = ins
        .providers
        .iter()
        .map(|p| {
            Row::new(vec![
                Cell::from(p.name.clone()),
                Cell::from(p.open.to_string()),
                Cell::from(Line::from(Span::styled(money(p.outstanding), bold()))),
            ])
        })
        .collect();
    let provider_focused = ins.focus == Focus::Providers;
    let position = |state: &TableState, len: usize| {
        Line::from(Span::styled(
            format!(" {}/{len} ", state.selected().map_or(0, |i| i + 1)),
            dim(),
        ))
        .right_aligned()
    };
    let providers = Table::new(
        provider_rows,
        [
            Constraint::Min(24),
            Constraint::Length(4),
            Constraint::Length(12),
        ],
    )
    .header(Row::new(["provider", "open", "outstanding"]).style(bold()))
    .block(
        theme::panel(format!("Providers — {} with open A/R", ins.providers.len()))
            .title_bottom(position(&ins.provider_table, ins.providers.len()))
            .border_style(if provider_focused {
                focus_style
            } else {
                blur_style
            }),
    )
    .row_highlight_style(highlight)
    .highlight_symbol(if provider_focused { "▶ " } else { "  " });
    frame.render_stateful_widget(providers, left, &mut ins.provider_table);

    // Right: the selected provider's open claims under the active sort.
    let arrow = if ins.sort_desc { "↓" } else { "↑" };
    let sort_header = |name: &str, key: SortKey| {
        if ins.sort == key {
            Span::styled(format!("{name} {arrow}"), theme::accent_bold())
        } else {
            Span::styled(name.to_string(), bold())
        }
    };
    let header = Row::new(vec![
        Cell::from(Line::from(Span::styled("claim", bold()))),
        Cell::from(Line::from(Span::styled("payer", bold()))),
        Cell::from(Line::from(sort_header("outstanding", SortKey::Cost))),
        Cell::from(Line::from(sort_header("age", SortKey::Age))),
        Cell::from(Line::from(sort_header("risk", SortKey::Risk))),
        Cell::from(Line::from(Span::styled("att", bold()))),
        Cell::from(Line::from(Span::styled("status", bold()))),
    ]);
    let max_risk = ins
        .claims
        .iter()
        .filter_map(|&idx| chase.get(idx))
        .map(|item| item.risk())
        .fold(0.0, f64::max);
    let claim_rows: Vec<Row> = ins
        .claims
        .iter()
        .filter_map(|&idx| chase.get(idx))
        .map(|item| {
            Row::new(vec![
                Cell::from(item.claim_id.0.clone()),
                Cell::from(item.payer_id.as_str().to_string()),
                Cell::from(money(item.outstanding)),
                Cell::from(format!("{:.1}d", item.age.as_secs_f64() / 86_400.0)),
                Cell::from(Line::from(Span::styled(
                    theme::thousands(item.risk() as u64),
                    Style::default().fg(risk_color(item.risk(), max_risk)),
                ))),
                Cell::from(item.attempts.to_string()),
                Cell::from(Line::from(Span::styled(
                    item.status.clone(),
                    status_style(&item.status),
                ))),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(12),
        Constraint::Length(22),
        Constraint::Length(12),
        Constraint::Length(8),
        Constraint::Length(11),
        Constraint::Length(4),
        Constraint::Min(14),
    ];
    let title = match ins.selected_provider() {
        Some(p) => format!(
            "{} — {} open · {} outstanding · sorted by {} {} (risk = $ × days)",
            p.name,
            p.open,
            money(p.outstanding),
            ins.sort.label(),
            arrow,
        ),
        None => "claims".to_string(),
    };
    let claims_focused = ins.focus == Focus::Claims;
    let claims = Table::new(claim_rows, widths)
        .header(header)
        .block(
            theme::panel(title)
                .title_bottom(position(&ins.claims_table, ins.claims.len()))
                .border_style(if claims_focused {
                    focus_style
                } else {
                    blur_style
                }),
        )
        .row_highlight_style(highlight)
        .highlight_symbol(if claims_focused { "▶ " } else { "  " });
    frame.render_stateful_widget(claims, right, &mut ins.claims_table);
}

/// The riskiest third of the visible claims reads hot, the middle warm.
fn risk_color(risk: f64, max: f64) -> Color {
    match risk {
        r if max > 0.0 && r >= max * 0.66 => ALERT,
        r if max > 0.0 && r >= max * 0.33 => WARN,
        _ => Color::Reset,
    }
}

/// Color keyed to the chase list's plain-English labels: flags read hot,
/// anything mid-chase (retrying, partial remit) reads warm, a first quiet
/// wait stays neutral.
fn status_style(status: &str) -> Style {
    if status.starts_with("flagged") {
        Style::default().fg(ALERT)
    } else if status.starts_with("retrying") || status.starts_with("partial remit") {
        Style::default().fg(WARN)
    } else {
        Style::default()
    }
}

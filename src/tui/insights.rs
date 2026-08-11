//! The Provider Insights pane: a master–detail view over the **drained
//! final books** — what is still open when the run completes is exactly the
//! pile a human has to work. Providers on the left (biggest book of open
//! money first) drive the right panel: a plain-English A/R analysis of the
//! selected provider — where the money is stuck, what the open claims are
//! doing, payer signals, the top chase items by risk, and recommended
//! actions. Enter steps into the document to scroll it; Esc steps back to
//! the provider list. The analysis is the terminal layer.

use std::collections::HashMap;

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};

use healthcare_billing_sim::RunOutput;
use healthcare_billing_sim::domain::Money;
use healthcare_billing_sim::reports::chase::ChaseItem;
use healthcare_billing_sim::reports::provider_analysis;

use super::theme::{self, ACCENT, WARN, bold, dim, money};

/// Master–detail state: the provider list on the left drives the analysis
/// document on the right; Enter moves focus into the document to scroll it,
/// Esc steps back up to the providers.
pub struct Insights {
    /// Aggregated per organization, sorted by total outstanding desc.
    providers: Vec<ProviderRow>,
    pub provider_table: TableState,
    pub focus: Focus,
    /// The selected provider's analysis document, styled line by line.
    analysis: Vec<Line<'static>>,
    pub scroll: u16,
}

struct ProviderRow {
    name: String,
    open: usize,
    outstanding: Money,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Providers,
    Analysis,
}

/// Document lines opening with these prefixes render as section headers.
const SECTION_HEADERS: [&str; 6] = [
    "HEADLINE",
    "WHERE THE MONEY IS STUCK",
    "WHAT THE OPEN CLAIMS ARE DOING",
    "PAYER SIGNALS",
    "TOP CHASE ITEMS",
    "RECOMMENDED ACTIONS",
];

impl Insights {
    pub fn build(output: &RunOutput, chase: &[ChaseItem]) -> Insights {
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
            focus: Focus::Providers,
            analysis: Vec::new(),
            scroll: 0,
        };
        insights.rebuild_analysis(output);
        insights
    }

    /// Move the provider selection, or scroll the analysis, whichever has
    /// focus; moving the selection re-derives the document.
    pub fn move_selection(&mut self, delta: isize, output: &RunOutput) {
        match self.focus {
            Focus::Providers => {
                if super::step(&mut self.provider_table, self.providers.len(), delta) {
                    self.rebuild_analysis(output);
                }
            }
            Focus::Analysis => {
                // The draw clamps to the document length, like every
                // scrolled panel.
                self.scroll = if delta < 0 {
                    self.scroll.saturating_sub(delta.unsigned_abs() as u16)
                } else {
                    self.scroll.saturating_add(delta as u16)
                };
            }
        }
    }

    /// Esc: one layer back up. Returns false when already at the base layer
    /// (provider list), where ←/→ already control the pane bar.
    pub fn escape(&mut self) -> bool {
        match self.focus {
            Focus::Analysis => {
                self.focus = Focus::Providers;
                true
            }
            Focus::Providers => false,
        }
    }

    fn selected_provider(&self) -> Option<&ProviderRow> {
        self.provider_table
            .selected()
            .and_then(|i| self.providers.get(i))
    }

    /// Re-derive the analysis document for the selected provider and reset
    /// the scroll to the top.
    fn rebuild_analysis(&mut self, output: &RunOutput) {
        self.scroll = 0;
        let Some(provider) = self.selected_provider() else {
            self.analysis.clear();
            return;
        };
        let doc = provider_analysis(&output.ledger, output.finished_at, &provider.name);
        self.analysis = doc.lines().map(style_line).collect();
    }
}

/// Style one document line by shape: the title reads accented, section
/// headers bold, payer warnings warm; prose stays plain.
fn style_line(raw: &str) -> Line<'static> {
    let owned = format!(" {raw}");
    if raw.contains("— A/R analysis") {
        return Line::from(Span::styled(owned, theme::accent_bold()));
    }
    if SECTION_HEADERS.iter().any(|h| raw.starts_with(h)) {
        return Line::from(Span::styled(owned, bold()));
    }
    if raw.trim_start().starts_with('▲') {
        return Line::from(Span::styled(owned, Style::default().fg(WARN)));
    }
    Line::from(owned)
}

pub fn draw(frame: &mut ratatui::Frame, area: Rect, ins: &mut Insights) {
    let [hint_area, body] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(3)]).areas(area);
    frame.render_widget(
        theme::keys_hint(&[
            ("↑/↓", "select a provider"),
            ("enter", "into the analysis to scroll it"),
            ("esc", "back to the provider list"),
        ]),
        hint_area,
    );

    if ins.providers.is_empty() {
        frame.render_widget(
            Paragraph::new("nothing outstanding at run completion — every receivable booked")
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
    let position = Line::from(Span::styled(
        format!(
            " {}/{} ",
            ins.provider_table.selected().map_or(0, |i| i + 1),
            ins.providers.len()
        ),
        dim(),
    ))
    .right_aligned();
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
        theme::panel(format!(
            "Providers — {} with A/R open at run end",
            ins.providers.len()
        ))
        .title_bottom(position)
        .border_style(if provider_focused {
            focus_style
        } else {
            blur_style
        }),
    )
    .row_highlight_style(highlight)
    .highlight_symbol(if provider_focused { "▶ " } else { "  " });
    frame.render_stateful_widget(providers, left, &mut ins.provider_table);

    // Right: the selected provider's A/R analysis, scrollable when focused.
    let analysis_focused = ins.focus == Focus::Analysis;
    let block = theme::panel("provider A/R analysis").border_style(if analysis_focused {
        focus_style
    } else {
        blur_style
    });
    theme::scrolled_paragraph(frame, right, block, &ins.analysis, &mut ins.scroll);
}

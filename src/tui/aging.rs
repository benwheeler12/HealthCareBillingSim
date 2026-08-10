//! The A/R Aging pane: a master–detail view. A provider list on the left
//! (whole portfolio first, then each billing organization by open payer A/R)
//! drives the aging tables on the right — the same colored buckets and mix
//! bars, recomputed for whichever book is selected. Tab hops focus so ↑/↓
//! can either change the selection or scroll the tables.

use std::collections::BTreeMap;
use std::collections::HashMap;

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Row, Table, TableState};

use healthcare_billing_sim::domain::{Money, PayerId, VirtualTime};
use healthcare_billing_sim::ledger::records::Ledger;
use healthcare_billing_sim::reports::aging::AgingBuckets;
use healthcare_billing_sim::reports::{ar_aging_for, days_in_ar_for};

use super::theme::{self, ACCENT, ALERT, BAD, GOOD, WARN, bold, dim, money};

/// Young → old, the same hues the legend and mix bars use.
const BUCKET_COLORS: [Color; 4] = [GOOD, WARN, ALERT, BAD];
/// Sized so a full table row (label + five money columns + bar) fits the
/// detail panel of a 160-column terminal without clipping.
const MIX_WIDTH: usize = 12;

pub struct AgingView {
    /// Row 0 is the whole portfolio; the rest are organizations, biggest
    /// open payer A/R first.
    providers: Vec<ProviderRow>,
    pub table: TableState,
    focus: Focus,
    /// The aging document for the current selection, rebuilt on move.
    doc: Vec<Line<'static>>,
    pub scroll: u16,
}

struct ProviderRow {
    /// None = the whole portfolio.
    name: Option<String>,
    open: usize,
    payer_ar: Money,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Providers,
    Tables,
}

pub fn build(books: &Ledger, as_of: VirtualTime) -> AgingView {
    // Aggregate per organization: open payer-A/R claims and dollars. An
    // organization appears if it has anything on either book.
    let mut totals: HashMap<&str, (usize, Money)> = HashMap::new();
    let mut all = (0usize, Money::ZERO);
    for record in books.claims.values() {
        let Some(identity) = &record.identity else {
            continue;
        };
        let outstanding = record.payer_outstanding();
        let patient: Money = record
            .lines
            .iter()
            .filter(|l| l.is_booked())
            .filter_map(|l| l.adjudication.as_ref())
            .map(|adj| adj.patient_responsibility())
            .sum();
        if outstanding == Money::ZERO && patient == Money::ZERO {
            continue;
        }
        let entry = totals
            .entry(identity.organization_name.as_str())
            .or_insert((0, Money::ZERO));
        let open = usize::from(outstanding > Money::ZERO);
        entry.0 += open;
        entry.1 += outstanding;
        all.0 += open;
        all.1 += outstanding;
    }
    let mut providers: Vec<ProviderRow> = totals
        .into_iter()
        .map(|(name, (open, payer_ar))| ProviderRow {
            name: Some(name.to_string()),
            open,
            payer_ar,
        })
        .collect();
    providers.sort_by(|a, b| {
        b.payer_ar
            .cmp(&a.payer_ar)
            .then_with(|| a.name.cmp(&b.name))
    });
    providers.insert(
        0,
        ProviderRow {
            name: None,
            open: all.0,
            payer_ar: all.1,
        },
    );

    let mut table = TableState::default();
    table.select(Some(0));
    let doc = build_doc(books, as_of, None);
    AgingView {
        providers,
        table,
        focus: Focus::Providers,
        doc,
        scroll: 0,
    }
}

impl AgingView {
    /// ↑/↓: move whichever side has focus — the provider selection (which
    /// rebuilds the tables) or the table scroll.
    pub fn key_move(&mut self, delta: isize, books: &Ledger, as_of: VirtualTime) {
        match self.focus {
            Focus::Providers => {
                if super::step(&mut self.table, self.providers.len(), delta) {
                    let provider = self.selected_name().map(str::to_string);
                    self.doc = build_doc(books, as_of, provider.as_deref());
                    self.scroll = 0;
                }
            }
            Focus::Tables => {
                self.scroll = self.scroll.saturating_add_signed(delta as i16);
            }
        }
    }

    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Providers => Focus::Tables,
            Focus::Tables => Focus::Providers,
        };
    }

    pub(crate) fn selected_name(&self) -> Option<&str> {
        self.table
            .selected()
            .and_then(|i| self.providers.get(i))
            .and_then(|p| p.name.as_deref())
    }
}

/// The aging tables for one selection: headline, legend, payer A/R and
/// patient responsibility sections with colored buckets and mix bars.
fn build_doc(books: &Ledger, as_of: VirtualTime, provider: Option<&str>) -> Vec<Line<'static>> {
    let aging = ar_aging_for(books, as_of, provider);
    let dar = days_in_ar_for(books, as_of, provider);

    let scope = provider.unwrap_or("all providers");
    let mut lines = vec![
        Line::from(vec![
            Span::styled(format!(" {scope} — days in A/R: "), bold()),
            Span::styled(format!("{dar:.1}"), theme::accent_bold()),
            Span::styled(format!("   snapshot as of {as_of} (end of intake)"), dim()),
        ]),
        legend(),
        Line::default(),
    ];
    section(
        &mut lines,
        "Payer A/R — ages from first submission (retries never reset the clock)",
        &aging.payer,
    );
    lines.push(Line::default());
    section(
        &mut lines,
        "Patient responsibility — ages from adjudication, when it became patient debt",
        &aging.patient,
    );
    lines
}

fn legend() -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (color, label) in BUCKET_COLORS
        .iter()
        .zip(["0–30d", "31–60d", "61–90d", "90+d"])
    {
        spans.push(Span::styled("■ ", Style::default().fg(*color)));
        spans.push(Span::styled(format!("{label}  "), dim()));
    }
    spans.push(Span::styled(
        "· each row's mix bar shows how that book skews",
        dim(),
    ));
    Line::from(spans)
}

fn section(lines: &mut Vec<Line<'static>>, title: &str, rows: &BTreeMap<PayerId, AgingBuckets>) {
    lines.push(Line::from(Span::styled(
        format!(" ▌ {title}"),
        theme::accent_bold(),
    )));
    if rows.is_empty() {
        lines.push(Line::from(Span::styled("   nothing outstanding", dim())));
        return;
    }
    lines.push(Line::from(Span::styled(
        format!(
            "   {:<22} {:>13} {:>13} {:>13} {:>13} {:>14}  {:<MIX_WIDTH$}",
            "payer", "0–30d", "31–60d", "61–90d", "90+d", "total", "mix"
        ),
        bold(),
    )));
    let mut grand = AgingBuckets::default();
    for (payer, b) in rows {
        lines.push(row(payer.as_str(), b, false));
        grand.d0_30 += b.d0_30;
        grand.d31_60 += b.d31_60;
        grand.d61_90 += b.d61_90;
        grand.d90_plus += b.d90_plus;
    }
    if rows.len() > 1 {
        lines.push(row("TOTAL", &grand, true));
    }
}

fn row(label: &str, b: &AgingBuckets, emphasize: bool) -> Line<'static> {
    let cell = |amount: Money, color: Color| {
        let style = if amount == Money::ZERO {
            dim()
        } else {
            Style::default().fg(color)
        };
        Span::styled(format!(" {:>13}", money(amount)), style)
    };
    let mut spans = vec![Span::styled(
        format!("   {label:<22}"),
        if emphasize { bold() } else { Style::default() },
    )];
    for (amount, color) in [b.d0_30, b.d31_60, b.d61_90, b.d90_plus]
        .into_iter()
        .zip(BUCKET_COLORS)
    {
        spans.push(cell(amount, color));
    }
    spans.push(Span::styled(format!(" {:>14}", money(b.total())), bold()));
    spans.push(Span::raw("  "));
    spans.extend(theme::seg_bar(
        MIX_WIDTH,
        &[
            (b.d0_30.cents() as f64, BUCKET_COLORS[0]),
            (b.d31_60.cents() as f64, BUCKET_COLORS[1]),
            (b.d61_90.cents() as f64, BUCKET_COLORS[2]),
            (b.d90_plus.cents() as f64, BUCKET_COLORS[3]),
        ],
    ));
    Line::from(spans)
}

pub fn draw(frame: &mut ratatui::Frame, area: Rect, view: &mut AgingView) {
    let [hint_area, body] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(4)]).areas(area);
    frame.render_widget(
        theme::keys_hint(&[
            ("↑/↓", "pick a provider (all providers first)"),
            ("tab", "hop over to scroll the tables"),
            ("green → red", "receivables aging past 90 days"),
        ]),
        hint_area,
    );

    let [left, right] =
        Layout::horizontal([Constraint::Length(48), Constraint::Min(40)]).areas(body);

    let focus_style = Style::default().fg(ACCENT);
    let blur_style = theme::dim();
    let list_focused = view.focus == Focus::Providers;

    let provider_rows: Vec<Row> = view
        .providers
        .iter()
        .map(|p| {
            let (name, style) = match &p.name {
                Some(name) => (name.clone(), Style::default()),
                None => ("ALL PROVIDERS".to_string(), bold()),
            };
            Row::new(vec![
                Cell::from(Line::from(Span::styled(name, style))),
                Cell::from(p.open.to_string()),
                Cell::from(Line::from(Span::styled(money(p.payer_ar), bold()))),
            ])
        })
        .collect();
    let position = Line::from(Span::styled(
        format!(
            " {}/{} ",
            view.table.selected().map_or(0, |i| i + 1),
            view.providers.len()
        ),
        dim(),
    ))
    .right_aligned();
    let providers = Table::new(
        provider_rows,
        [
            Constraint::Min(24),
            Constraint::Length(4),
            Constraint::Length(13),
        ],
    )
    .header(Row::new(["provider", "open", "payer A/R"]).style(bold()))
    .block(
        theme::panel(format!("Books — {} rows", view.providers.len()))
            .title_bottom(position)
            .border_style(if list_focused {
                focus_style
            } else {
                blur_style
            }),
    )
    .row_highlight_style(theme::bold().bg(Color::DarkGray))
    .highlight_symbol(if list_focused { "▶ " } else { "  " });
    frame.render_stateful_widget(providers, left, &mut view.table);

    let scope = view
        .selected_name()
        .map(str::to_string)
        .unwrap_or_else(|| "all providers".to_string());
    let doc = view.doc.clone();
    theme::scrolled_paragraph(
        frame,
        right,
        theme::panel(format!("A/R Aging — {scope}")).border_style(if list_focused {
            blur_style
        } else {
            focus_style
        }),
        &doc,
        &mut view.scroll,
    );
}

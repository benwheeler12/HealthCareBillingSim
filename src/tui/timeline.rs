//! The Timeline pane: a master–detail view. A provider list on the left
//! (whole portfolio first, then each billing organization by claim volume)
//! drives the charts on the right — virtual time bucketed into fixed
//! windows, each lifecycle series normalized to events per virtual day,
//! plus the in-flight backlog curve (cumulative ingested minus settled),
//! re-bucketed from the retained event log for whichever book is selected.
//! Every timeline shares the portfolio's x-axis, so shapes stay comparable.

use std::collections::HashMap;

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Cell, Chart, Dataset, GraphType, Row, Table, TableState};

use healthcare_billing_sim::RunOutput;
use healthcare_billing_sim::domain::{ClaimId, VirtualTime, human_virtual};
use healthcare_billing_sim::ledger::events::{ClaimEvent, StampedEvent};

use super::theme::{self, ACCENT, bold, dim};

#[derive(Default)]
pub struct Timeline {
    end_day: f64,
    intake_end_day: f64,
    bucket_secs: f64,
    ingested: Vec<(f64, f64)>,
    submitted: Vec<(f64, f64)>,
    remitted: Vec<(f64, f64)>,
    settled: Vec<(f64, f64)>,
    in_flight: Vec<(f64, f64)>,
    /// Event totals per series, in the same order as the fields above.
    totals: [usize; 4],
    max_rate: f64,
    max_in_flight: f64,
}

pub const TIMELINE_BUCKETS: usize = 120;
pub const VDAY: f64 = 86_400.0;

pub fn timeline(
    log: &[StampedEvent],
    intake_end: VirtualTime,
    end: VirtualTime,
    buckets: usize,
) -> Timeline {
    timeline_where(log, intake_end, end, buckets, |_| true)
}

/// The same bucketing over the subset of the log whose claim passes `keep` —
/// per-provider timelines re-bucket without cloning events.
pub fn timeline_where(
    log: &[StampedEvent],
    intake_end: VirtualTime,
    end: VirtualTime,
    buckets: usize,
    keep: impl Fn(&ClaimId) -> bool,
) -> Timeline {
    let end_secs = end.as_duration().as_secs_f64().max(f64::EPSILON);
    let bucket_secs = end_secs / buckets as f64;
    let mut counts = vec![[0usize; 4]; buckets];
    for stamped in log.iter().filter(|s| keep(&s.claim_id)) {
        // Rejected/duplicate rows never take flight, and quarantine/garbage/
        // late notes are epistemics, not flow — the chart tracks claim traffic.
        let series = match &stamped.event {
            ClaimEvent::Ingested { .. } => 0,
            ClaimEvent::Submitted { .. } => 1,
            ClaimEvent::RemittanceApplied { .. } => 2,
            ClaimEvent::Resolved | ClaimEvent::Flagged { .. } => 3,
            _ => continue,
        };
        let at = stamped.at.as_duration().as_secs_f64();
        let idx = ((at / bucket_secs) as usize).min(buckets - 1);
        counts[idx][series] += 1;
    }

    let mut tl = Timeline {
        end_day: end_secs / VDAY,
        intake_end_day: intake_end.as_duration().as_secs_f64() / VDAY,
        bucket_secs,
        ..Timeline::default()
    };
    let per_day = VDAY / bucket_secs;
    let mut open = 0.0;
    for (i, bucket) in counts.iter().enumerate() {
        let x = (i as f64 + 0.5) * bucket_secs / VDAY;
        tl.ingested.push((x, bucket[0] as f64 * per_day));
        tl.submitted.push((x, bucket[1] as f64 * per_day));
        tl.remitted.push((x, bucket[2] as f64 * per_day));
        tl.settled.push((x, bucket[3] as f64 * per_day));
        open += bucket[0] as f64 - bucket[3] as f64;
        tl.in_flight.push((x, open));
        tl.max_in_flight = tl.max_in_flight.max(open);
        for (series, &n) in bucket.iter().enumerate() {
            tl.totals[series] += n;
            tl.max_rate = tl.max_rate.max(n as f64 * per_day);
        }
    }
    tl
}

/// Master–detail state: the provider list on the left drives the charts.
pub struct TimelineView {
    /// Row 0 is the whole portfolio; the rest by claim volume desc.
    providers: Vec<ProviderRow>,
    pub table: TableState,
    /// Which organization owns each claim — rejected-at-parse rows carry no
    /// identity and so appear only in the portfolio view.
    owners: HashMap<ClaimId, String>,
    /// The bucketed series for the current selection, rebuilt on move.
    current: Timeline,
}

struct ProviderRow {
    /// None = the whole portfolio.
    name: Option<String>,
    claims: usize,
}

pub fn build(output: &RunOutput) -> TimelineView {
    let mut owners = HashMap::new();
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for record in output.ledger.claims.values() {
        let Some(identity) = &record.identity else {
            continue;
        };
        owners.insert(record.claim_id.clone(), identity.organization_name.clone());
        *counts
            .entry(identity.organization_name.as_str())
            .or_insert(0) += 1;
    }
    let mut providers: Vec<ProviderRow> = counts
        .into_iter()
        .map(|(name, claims)| ProviderRow {
            name: Some(name.to_string()),
            claims,
        })
        .collect();
    providers.sort_by(|a, b| b.claims.cmp(&a.claims).then_with(|| a.name.cmp(&b.name)));
    providers.insert(
        0,
        ProviderRow {
            name: None,
            claims: output.ledger.claims.len(),
        },
    );

    let mut table = TableState::default();
    table.select(Some(0));
    let mut view = TimelineView {
        providers,
        table,
        owners,
        current: Timeline::default(),
    };
    view.rebuild(output);
    view
}

impl TimelineView {
    pub fn move_selection(&mut self, delta: isize, output: &RunOutput) {
        if super::step(&mut self.table, self.providers.len(), delta) {
            self.rebuild(output);
        }
    }

    pub(crate) fn selected_name(&self) -> Option<&str> {
        self.table
            .selected()
            .and_then(|i| self.providers.get(i))
            .and_then(|p| p.name.as_deref())
    }

    fn rebuild(&mut self, output: &RunOutput) {
        let log = &output.ledger.event_log;
        let (intake_end, end) = (output.intake_finished_at, output.finished_at);
        self.current = match self.selected_name() {
            None => timeline(log, intake_end, end, TIMELINE_BUCKETS),
            Some(provider) => timeline_where(log, intake_end, end, TIMELINE_BUCKETS, |id| {
                self.owners.get(id).is_some_and(|owner| owner == provider)
            }),
        };
    }
}

pub fn draw(frame: &mut ratatui::Frame, area: Rect, view: &mut TimelineView) {
    let [hint_area, body] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(6)]).areas(area);
    frame.render_widget(
        theme::keys_hint(&[(
            "↑/↓",
            "pick a provider — the charts re-bucket to their claims",
        )]),
        hint_area,
    );

    let [left, right] =
        Layout::horizontal([Constraint::Length(40), Constraint::Min(60)]).areas(body);

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
                Cell::from(theme::thousands(p.claims as u64)),
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
    let providers = Table::new(provider_rows, [Constraint::Min(24), Constraint::Length(7)])
        .header(Row::new(["provider", "claims"]).style(bold()))
        .block(
            theme::panel("Books — by claim volume")
                .title_bottom(position)
                .border_style(Style::default().fg(ACCENT)),
        )
        .row_highlight_style(theme::bold().bg(Color::DarkGray))
        .highlight_symbol("▶ ");
    frame.render_stateful_widget(providers, left, &mut view.table);

    let scope = view
        .selected_name()
        .map(str::to_string)
        .unwrap_or_else(|| "all providers".to_string());
    draw_charts(frame, right, &view.current, &scope);
}

fn draw_charts(frame: &mut ratatui::Frame, area: Rect, tl: &Timeline, scope: &str) {
    let half = [Constraint::Percentage(50), Constraint::Percentage(50)];
    let [rates_area, flight_area] = Layout::vertical(half).areas(area);
    let [row1, row2] = Layout::vertical(half).areas(rates_area);
    let [q_ingested, q_submitted] = Layout::horizontal(half).areas(row1);
    let [q_remitted, q_settled] = Layout::horizontal(half).areas(row2);

    let dim = theme::dim();

    // Small multiples on one shared y scale, so rates compare across charts
    // at a glance — submitted riding above ingested is the retry traffic.
    let rate_top = (tl.max_rate * 1.15).max(1.0);
    let quads = [
        (
            q_ingested,
            format!(
                "{scope} — ingested {} · each point ≈ {}",
                theme::thousands(tl.totals[0] as u64),
                human_virtual(tl.bucket_secs)
            ),
            Color::Cyan,
            &tl.ingested,
        ),
        (
            q_submitted,
            format!(
                "submitted {} (incl. retries)",
                theme::thousands(tl.totals[1] as u64)
            ),
            Color::Yellow,
            &tl.submitted,
        ),
        (
            q_remitted,
            format!("remitted {}", theme::thousands(tl.totals[2] as u64)),
            Color::Green,
            &tl.remitted,
        ),
        (
            q_settled,
            format!(
                "settled {} (resolved + flagged)",
                theme::thousands(tl.totals[3] as u64)
            ),
            Color::Magenta,
            &tl.settled,
        ),
    ];
    for (quad_area, title, color, data) in quads {
        let datasets = vec![
            Dataset::default()
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(color))
                .data(data),
        ];
        let chart = Chart::new(datasets)
            .block(theme::panel(title))
            .x_axis(
                Axis::default()
                    .style(dim)
                    .bounds([0.0, tl.end_day])
                    .labels(vec!["0".to_string(), format!("{:.0}d", tl.end_day)]),
            )
            .y_axis(
                Axis::default()
                    .title("per day")
                    .style(dim)
                    .bounds([0.0, rate_top])
                    .labels(vec!["0".to_string(), fmt_axis(rate_top)]),
            );
        frame.render_widget(chart, quad_area);
    }

    // The default legend hides itself above 1/4 of the chart area; the
    // legend is the intake-end marker's only key, so give it headroom.
    let legend_room = (Constraint::Ratio(1, 2), Constraint::Ratio(1, 2));
    let flight_top = (tl.max_in_flight * 1.15).max(1.0);
    let intake_marker = [(tl.intake_end_day, 0.0), (tl.intake_end_day, flight_top)];
    let flight_sets = vec![
        Dataset::default()
            .name("in flight")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::LightRed))
            .data(&tl.in_flight),
        Dataset::default()
            .name(format!("intake ends {:.0}d", tl.intake_end_day))
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(dim)
            .data(&intake_marker),
    ];
    let flight = Chart::new(flight_sets)
        .hidden_legend_constraints(legend_room)
        .block(theme::panel(
            "Backlog — claims in flight (ingested − settled) · drains to zero",
        ))
        .x_axis(
            Axis::default()
                .title("virtual days")
                .style(dim)
                .bounds([0.0, tl.end_day])
                .labels(vec![
                    "0".to_string(),
                    format!("{:.0}", tl.end_day / 2.0),
                    format!("{:.0}d", tl.end_day),
                ]),
        )
        .y_axis(
            Axis::default()
                .title("open")
                .style(dim)
                .bounds([0.0, flight_top])
                .labels(vec![
                    "0".to_string(),
                    fmt_axis(flight_top / 2.0),
                    fmt_axis(flight_top),
                ]),
        );
    frame.render_widget(flight, flight_area);
}

fn fmt_axis(v: f64) -> String {
    if v >= 10.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use healthcare_billing_sim::domain::{ClaimId, PayerId};
    use healthcare_billing_sim::ledger::records::{ClaimIdentity, FlagReason};

    use super::*;

    fn at(days: f64) -> VirtualTime {
        VirtualTime::default() + Duration::from_secs_f64(days * VDAY)
    }

    fn ev(days: f64, event: ClaimEvent) -> StampedEvent {
        StampedEvent {
            at: at(days),
            claim_id: ClaimId("c1".into()),
            event,
        }
    }

    fn ingested() -> ClaimEvent {
        ClaimEvent::Ingested {
            identity: ClaimIdentity {
                payer_id: PayerId::Medicare,
                provider_npi: String::new(),
                organization_name: String::new(),
                patient_member_id: String::new(),
            },
            lines: Vec::new(),
        }
    }

    #[test]
    fn timeline_buckets_events_and_normalizes_to_per_day_rates() {
        // 10 virtual days into 10 buckets -> 1-day buckets, per_day factor 1.
        let log = vec![
            ev(0.2, ingested()),
            ev(0.7, ingested()),
            ev(3.5, ClaimEvent::Resolved),
            ev(9.99, ClaimEvent::Resolved),
        ];
        let tl = timeline(&log, at(5.0), at(10.0), 10);
        assert_eq!(tl.totals, [2, 0, 0, 2]);
        assert_eq!(tl.ingested[0].1, 2.0);
        assert_eq!(tl.settled[3].1, 1.0);
        assert_eq!(tl.settled[9].1, 1.0);
        assert!((tl.end_day - 10.0).abs() < 1e-9);
        assert!((tl.intake_end_day - 5.0).abs() < 1e-9);
    }

    #[test]
    fn in_flight_tracks_ingested_minus_settled_and_drains_to_zero() {
        let log = vec![
            ev(1.0, ingested()),
            ev(2.0, ingested()),
            ev(
                2.1,
                ClaimEvent::Submitted {
                    attempt: 1,
                    timeout_at: at(4.0),
                },
            ),
            ev(6.0, ClaimEvent::Resolved),
            ev(
                8.0,
                ClaimEvent::Flagged {
                    reason: FlagReason::RetriesExhausted,
                },
            ),
        ];
        let tl = timeline(&log, at(2.5), at(10.0), 10);
        assert_eq!(tl.in_flight[1].1, 1.0);
        assert_eq!(tl.in_flight[2].1, 2.0);
        assert_eq!(tl.in_flight[6].1, 1.0);
        assert_eq!(tl.in_flight[9].1, 0.0);
        // Submissions are traffic, never backlog: retries must not inflate it.
        assert_eq!(tl.totals[1], 1);
        assert!(tl.in_flight.iter().all(|&(_, y)| y >= 0.0));
    }
}

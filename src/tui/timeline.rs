//! The Timeline pane: virtual time bucketed into fixed windows, each
//! lifecycle series normalized to events per virtual day, plus the in-flight
//! backlog curve (cumulative ingested minus settled). Built once from the
//! retained event log — the audit trail replayed as a picture.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::symbols::Marker;
use ratatui::widgets::{Axis, Chart, Dataset, GraphType};

use healthcare_billing_sim::domain::{VirtualTime, human_virtual};
use healthcare_billing_sim::ledger::events::{ClaimEvent, StampedEvent};

use super::theme;

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
    let end_secs = end.as_duration().as_secs_f64().max(f64::EPSILON);
    let bucket_secs = end_secs / buckets as f64;
    let mut counts = vec![[0usize; 4]; buckets];
    for stamped in log {
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

pub fn draw(frame: &mut ratatui::Frame, area: Rect, tl: &Timeline) {
    let [hint_area, charts] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(6)]).areas(area);
    frame.render_widget(
        theme::hint(
            "the run replayed from the audit trail — rates share one y-scale, so \
             submitted riding above ingested is retry traffic · the backlog below must drain to zero",
        ),
        hint_area,
    );

    let half = [Constraint::Percentage(50), Constraint::Percentage(50)];
    let [rates_area, flight_area] = Layout::vertical(half).areas(charts);
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
                " ingested {} · each point ≈ {} ",
                theme::thousands(tl.totals[0] as u64),
                human_virtual(tl.bucket_secs)
            ),
            Color::Cyan,
            &tl.ingested,
        ),
        (
            q_submitted,
            format!(
                " submitted {} (incl. retries) ",
                theme::thousands(tl.totals[1] as u64)
            ),
            Color::Yellow,
            &tl.submitted,
        ),
        (
            q_remitted,
            format!(" remitted {} ", theme::thousands(tl.totals[2] as u64)),
            Color::Green,
            &tl.remitted,
        ),
        (
            q_settled,
            format!(
                " settled {} (resolved + flagged) ",
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

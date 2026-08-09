//! Interactive terminal UI: a static screen with the reports arrayed as
//! panes (←/→ to move between them), a live status box pinned to the bottom,
//! and Enter on the chase list drilling into a claim's full audit trail.
//!
//! Threading: the TUI event loop owns the main thread on the *wall* clock;
//! the simulation runs on a worker thread inside its own paused-time runtime
//! (Decisions #12 — auto-advance needs a dedicated current-thread runtime).
//! They meet at two channels: the fold's live-progress watch tap, and a
//! oneshot carrying the finished `RunOutput`.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::symbols::Marker;
use ratatui::text::Line;
use ratatui::widgets::{
    Axis, Block, Chart, Clear, Dataset, GraphType, Paragraph, Row, Table, TableState, Tabs,
};
use tokio::sync::watch;

use healthcare_billing_sim::domain::{Money, VirtualTime, human_virtual};
use healthcare_billing_sim::ledger::events::{ClaimEvent, StampedEvent};
use healthcare_billing_sim::ledger::fold::Progress;
use healthcare_billing_sim::ledger::records::ClaimState;
use healthcare_billing_sim::reports::chase::ChaseItem;
use healthcare_billing_sim::reports::{
    Denials, Scorecard, ar_aging, chase_list, days_in_ar, denial_breakdown, diagnostic,
    payer_scorecard, summarize,
};
use healthcare_billing_sim::{RunConfig, RunOutput};

pub struct TuiOptions {
    /// Pre-rendered run-configuration banner (uncolored).
    pub banner: String,
    pub seed: u64,
}

/// Run the simulation under the interactive UI and hand the finished output
/// back so the caller can leave a plain-text record on stdout.
pub fn run(mut cfg: RunConfig, opts: TuiOptions) -> anyhow::Result<RunOutput> {
    let (progress_tx, progress_rx) = watch::channel(Progress::default());
    cfg.progress = Some(progress_tx);
    let (done_tx, done_rx) = mpsc::channel();
    let sim = std::thread::spawn(move || {
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .map_err(anyhow::Error::from)
            .and_then(|rt| rt.block_on(healthcare_billing_sim::run(cfg)));
        let _ = done_tx.send(result);
    });

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, progress_rx, &done_rx, &opts);
    ratatui::restore();
    let _ = sim.join();
    result
}

struct App {
    banner: String,
    seed: u64,
    wall_start: Instant,
    wall_done: Option<Duration>,
    progress: Progress,
    /// None while the simulation is still running.
    done: Option<Done>,
    pane: usize,
    scroll: u16,
    frame: usize,
}

struct Done {
    output: RunOutput,
    panes: Vec<Pane>,
    chase: Vec<ChaseItem>,
    table: TableState,
    timeline: Timeline,
    /// Open claim-detail overlay, if any.
    detail: Option<String>,
}

struct Pane {
    title: &'static str,
    content: String,
}

const TIMELINE_PANE: usize = 1;
const CHASE_PANE: usize = 6;

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    mut progress_rx: watch::Receiver<Progress>,
    done_rx: &mpsc::Receiver<anyhow::Result<RunOutput>>,
    opts: &TuiOptions,
) -> anyhow::Result<RunOutput> {
    let mut app = App {
        banner: opts.banner.clone(),
        seed: opts.seed,
        wall_start: Instant::now(),
        wall_done: None,
        progress: Progress::default(),
        done: None,
        pane: 0,
        scroll: 0,
        frame: 0,
    };

    loop {
        if app.done.is_none() {
            app.progress = *progress_rx.borrow_and_update();
            if let Ok(result) = done_rx.try_recv() {
                let output = result?;
                app.wall_done = Some(app.wall_start.elapsed());
                app.progress = final_progress(&output);
                app.done = Some(Done::build(output));
            }
        }
        app.frame = app.frame.wrapping_add(1);
        terminal.draw(|frame| draw(frame, &mut app))?;

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if handle_key(&mut app, key.code, key.modifiers) {
            match app.done {
                Some(done) => return Ok(done.output),
                None => anyhow::bail!("interrupted before the simulation finished"),
            }
        }
    }
}

/// Apply one keypress; returns true when the user asked to leave the UI.
fn handle_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> bool {
    let ctrl_c = code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL);
    let Some(done) = &mut app.done else {
        return ctrl_c || code == KeyCode::Char('q') || code == KeyCode::Esc;
    };

    // The overlay swallows everything: any of these close it, nothing quits.
    if done.detail.is_some() {
        if matches!(code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) || ctrl_c {
            done.detail = None;
        }
        return false;
    }

    let panes = done.panes.len();
    match code {
        _ if ctrl_c => return true,
        KeyCode::Char('q') | KeyCode::Esc => return true,
        KeyCode::Left => {
            app.pane = (app.pane + panes - 1) % panes;
            app.scroll = 0;
        }
        KeyCode::Right => {
            app.pane = (app.pane + 1) % panes;
            app.scroll = 0;
        }
        KeyCode::Up if app.pane == CHASE_PANE => move_selection(done, -1),
        KeyCode::Down if app.pane == CHASE_PANE => move_selection(done, 1),
        KeyCode::PageUp if app.pane == CHASE_PANE => move_selection(done, -10),
        KeyCode::PageDown if app.pane == CHASE_PANE => move_selection(done, 10),
        KeyCode::Up => app.scroll = app.scroll.saturating_sub(1),
        KeyCode::Down => app.scroll = app.scroll.saturating_add(1),
        KeyCode::PageUp => app.scroll = app.scroll.saturating_sub(10),
        KeyCode::PageDown => app.scroll = app.scroll.saturating_add(10),
        KeyCode::Enter if app.pane == CHASE_PANE => {
            if let Some(item) = done.table.selected().and_then(|i| done.chase.get(i)) {
                done.detail = Some(claim_detail(&done.output, &item.claim_id.0));
            }
        }
        _ => {}
    }
    false
}

fn move_selection(done: &mut Done, delta: isize) {
    if done.chase.is_empty() {
        return;
    }
    let current = done.table.selected().unwrap_or(0) as isize;
    let next = (current + delta).clamp(0, done.chase.len() as isize - 1);
    done.table.select(Some(next as usize));
}

fn final_progress(output: &RunOutput) -> Progress {
    let mut p = Progress {
        claims: output.ledger.claims.len(),
        now: output.finished_at,
        ..Progress::default()
    };
    for record in output.ledger.claims.values() {
        match record.state {
            ClaimState::Resolved => p.resolved += 1,
            ClaimState::Rejected { .. } => p.rejected += 1,
            ClaimState::Flagged { .. } => p.flagged += 1,
            _ => {}
        }
    }
    p
}

impl Done {
    fn build(output: RunOutput) -> Done {
        let (books, as_of) = (&output.intake_ledger, output.intake_finished_at);
        let aging = ar_aging(books, as_of).to_string();
        let (payer_aging, patient_aging) = match aging.find("=== Patient") {
            Some(idx) => (
                aging[..idx].trim_end().to_string(),
                aging[idx..].to_string(),
            ),
            None => (aging, String::new()),
        };

        let overview = format!(
            "{}\n{}\n\nA/R panes are a snapshot as of end of intake ({as_of}) — the books\nas a biller sees them mid-flight. The run then drained every claim to a\nterminal state by {} (the correctness guarantee).",
            output_banner(&output),
            summarize(&output.ledger),
            output.finished_at,
        );
        let payer_pane = format!(
            "{payer_aging}\n\ndays in A/R: {:.1}   (as of {as_of})",
            days_in_ar(books, as_of)
        );
        let chase = chase_list(books, as_of, 500);
        let mut table = TableState::default();
        if !chase.is_empty() {
            table.select(Some(0));
        }
        let timeline = timeline(
            &output.ledger.event_log,
            output.intake_finished_at,
            output.finished_at,
            TIMELINE_BUCKETS,
        );

        let panes = vec![
            Pane {
                title: "Overview",
                content: overview,
            },
            Pane {
                title: "Timeline",
                content: String::new(),
            },
            Pane {
                title: "AR Aging",
                content: payer_pane,
            },
            Pane {
                title: "Patient",
                content: patient_aging,
            },
            Pane {
                title: "Scorecard",
                content: Scorecard(payer_scorecard(&output.ledger)).to_string(),
            },
            Pane {
                title: "Denials",
                content: Denials(denial_breakdown(&output.ledger)).to_string(),
            },
            Pane {
                title: "Chase",
                content: String::new(),
            },
            Pane {
                title: "Diagnostic",
                content: diagnostic(&output.ledger, &output.sim_truth).to_string(),
            },
        ];
        Done {
            output,
            panes,
            chase,
            table,
            timeline,
            detail: None,
        }
    }
}

/// The banner belongs to the caller (it knows the CLI); this trailer names
/// what the panes were computed from.
fn output_banner(output: &RunOutput) -> String {
    format!(
        "{} claims on the books · intake ended {} · drained by {}",
        output.ledger.claims.len(),
        output.intake_finished_at,
        output.finished_at
    )
}

fn draw(frame: &mut ratatui::Frame, app: &mut App) {
    let [tabs_area, content, status_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(4),
    ])
    .areas(frame.area());

    match &mut app.done {
        None => {
            frame.render_widget(
                Line::from(" healthcare billing simulation — running ")
                    .bold()
                    .cyan(),
                tabs_area,
            );
            let body = Paragraph::new(app.banner.clone())
                .block(Block::bordered().title(" run configuration "));
            frame.render_widget(body, content);
        }
        Some(done) => {
            // The active tab is a solid block, not just a tinted word: padded
            // title, black-on-cyan, with the rest of the bar dimmed.
            let titles: Vec<String> = done
                .panes
                .iter()
                .map(|p| format!(" {} ", p.title))
                .collect();
            let tabs = Tabs::new(titles)
                .select(app.pane)
                .style(Style::default().fg(Color::DarkGray))
                .highlight_style(
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
                .divider("");
            frame.render_widget(tabs, tabs_area);

            if app.pane == CHASE_PANE {
                draw_chase(frame, content, done);
            } else if app.pane == TIMELINE_PANE {
                draw_timeline(frame, content, done);
            } else {
                let pane = &done.panes[app.pane];
                let body = Paragraph::new(pane.content.clone())
                    .block(Block::bordered().title(format!(" {} ", pane.title)))
                    .scroll((app.scroll, 0));
                frame.render_widget(body, content);
            }

            if let Some(detail) = &done.detail {
                draw_detail(frame, content, detail);
            }
        }
    }

    draw_status(frame, status_area, app);
}

/// The Timeline pane's data: virtual time bucketed into fixed windows, each
/// lifecycle series normalized to events per virtual day, plus the in-flight
/// backlog curve (cumulative ingested minus settled). Built once from the
/// retained event log — the audit trail replayed as a picture.
#[derive(Default)]
struct Timeline {
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

const TIMELINE_BUCKETS: usize = 120;
const VDAY: f64 = 86_400.0;

fn timeline(
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

fn draw_timeline(frame: &mut ratatui::Frame, area: Rect, done: &Done) {
    let tl = &done.timeline;
    let half = [Constraint::Percentage(50), Constraint::Percentage(50)];
    let [rates_area, flight_area] = Layout::vertical(half).areas(area);
    let [row1, row2] = Layout::vertical(half).areas(rates_area);
    let [q_ingested, q_submitted] = Layout::horizontal(half).areas(row1);
    let [q_remitted, q_settled] = Layout::horizontal(half).areas(row2);

    let dim = Style::default().fg(Color::DarkGray);

    // Small multiples on one shared y scale, so rates compare across charts
    // at a glance — submitted riding above ingested is the retry traffic.
    let rate_top = (tl.max_rate * 1.15).max(1.0);
    let quads = [
        (
            q_ingested,
            format!(
                " ingested {} · each point ≈ {} ",
                tl.totals[0],
                human_virtual(tl.bucket_secs)
            ),
            Color::Cyan,
            &tl.ingested,
        ),
        (
            q_submitted,
            format!(" submitted {} (incl. retries) ", tl.totals[1]),
            Color::Yellow,
            &tl.submitted,
        ),
        (
            q_remitted,
            format!(" remitted {} ", tl.totals[2]),
            Color::Green,
            &tl.remitted,
        ),
        (
            q_settled,
            format!(" settled {} (resolved + flagged) ", tl.totals[3]),
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
            .block(Block::bordered().title(title))
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
        .block(
            Block::bordered()
                .title(" Backlog — claims in flight (ingested − settled) · drains to zero "),
        )
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

fn draw_chase(frame: &mut ratatui::Frame, area: Rect, done: &mut Done) {
    let header = Row::new(["claim", "payer", "outstanding", "age", "att", "status"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = done
        .chase
        .iter()
        .map(|item| {
            Row::new([
                item.claim_id.0.clone(),
                item.payer_id.as_str().to_string(),
                item.outstanding.to_string(),
                format!("{:.1}d", item.age.as_secs_f64() / 86_400.0),
                item.attempts.to_string(),
                item.status.clone(),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(14),
        Constraint::Length(20),
        Constraint::Length(12),
        Constraint::Length(8),
        Constraint::Length(4),
        Constraint::Min(20),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::bordered().title(format!(
            " Chase — {} open receivables · enter for the claim's audit trail ",
            done.chase.len()
        )))
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    frame.render_stateful_widget(table, area, &mut done.table);
}

fn draw_detail(frame: &mut ratatui::Frame, area: Rect, detail: &str) {
    let width = area.width.saturating_sub(6).min(96);
    let height = area.height.saturating_sub(2);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let body = Paragraph::new(detail.to_string()).block(
        Block::bordered()
            .title(" claim audit trail — esc to close ")
            .border_style(Style::default().fg(Color::Cyan)),
    );
    frame.render_widget(body, popup);
}

fn draw_status(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    const FRAMES: [char; 4] = ['⠋', '⠙', '⠸', '⠴'];
    let p = &app.progress;
    let counts = format!(
        "{} claims · {} resolved · {} rejected · {} flagged · {}",
        p.claims,
        p.resolved,
        p.rejected,
        p.flagged,
        human_virtual(p.now.as_duration().as_secs_f64()),
    );
    let (headline, keys) = match app.wall_done {
        None => (
            format!(
                "{} running · {counts} · {:.1}s wall",
                FRAMES[app.frame % FRAMES.len()],
                app.wall_start.elapsed().as_secs_f64()
            ),
            "q quit".to_string(),
        ),
        Some(wall) => (
            format!(
                "✓ complete · {counts} · {:.2}s wall · seed {}",
                wall.as_secs_f64(),
                app.seed
            ),
            "←/→ panes · ↑/↓ scroll or select · enter claim detail · q quit (prints plain report)"
                .to_string(),
        ),
    };
    let status = Paragraph::new(format!("{headline}\n{keys}"))
        .block(Block::bordered().title(" simulation "));
    frame.render_widget(status, area);
}

/// Everything the ledger knows about one claim, straight from the audit
/// trail: identity, both states (at the intake snapshot and final), money
/// lines, and the full event history — the event-sourced ledger on display.
fn claim_detail(output: &RunOutput, claim_id: &str) -> String {
    let key = healthcare_billing_sim::domain::ClaimId(claim_id.to_string());
    let Some(record) = output.ledger.claims.get(&key) else {
        return format!("claim {claim_id} not found");
    };
    let snapshot_state = output
        .intake_ledger
        .claims
        .get(&key)
        .map(|r| state_label(&r.state))
        .unwrap_or_else(|| "not yet ingested".to_string());

    let mut out = String::new();
    out.push_str(&format!("claim {claim_id}\n"));
    if let Some(identity) = &record.identity {
        out.push_str(&format!(
            "payer {} · member {} · npi {}\n",
            identity.payer_id, identity.patient_member_id, identity.provider_npi
        ));
    }
    out.push_str(&format!(
        "at intake snapshot: {snapshot_state}\nfinal state:        {} · {} attempts\n",
        state_label(&record.state),
        record.attempts
    ));
    out.push_str(&format!(
        "outstanding {} of {} billed\n\nservice lines:\n",
        record.payer_outstanding(),
        record.lines.iter().map(|l| l.billed()).sum::<Money>()
    ));
    for line in &record.lines {
        let status = match &line.adjudication {
            _ if line.do_not_bill => "do_not_bill".to_string(),
            Some(adj) => format!(
                "paid {} · patient {} · not allowed {}{}",
                adj.payer_paid,
                adj.patient_responsibility(),
                adj.not_allowed,
                adj.denial_reason
                    .map(|r| format!(" · denied: {r}"))
                    .unwrap_or_default()
            ),
            None => "unanswered".to_string(),
        };
        out.push_str(&format!(
            "  {} {} ×{} @ {} — {status}\n",
            line.service_line_id, line.procedure_code, line.units, line.unit_charge
        ));
    }
    out.push_str("\nevent history:\n");
    for stamped in &record.history {
        out.push_str(&format!(
            "  {:<26} {}\n",
            stamped.at.to_string(),
            event_label(&stamped.event)
        ));
    }
    out
}

fn state_label(state: &ClaimState) -> String {
    match state {
        ClaimState::Pending => "pending".into(),
        ClaimState::AwaitingResponse { timeout_at } => {
            format!("awaiting response (deadline {timeout_at})")
        }
        ClaimState::Resolved => "resolved".into(),
        ClaimState::Rejected { .. } => "rejected at ingest".into(),
        ClaimState::Flagged { reason } => format!("flagged: {reason:?}"),
    }
}

fn event_label(event: &ClaimEvent) -> String {
    match event {
        ClaimEvent::Ingested { .. } => "ingested".into(),
        ClaimEvent::Rejected { reason } => format!("rejected: {reason:?}"),
        ClaimEvent::DuplicateIngest { line_no } => {
            format!("duplicate claim_id on input line {line_no} — first document wins")
        }
        ClaimEvent::Submitted {
            attempt,
            timeout_at,
        } => {
            format!("submitted, attempt {attempt} (deadline {timeout_at})")
        }
        ClaimEvent::RemittanceApplied { lines } => {
            format!("remittance applied: {} line(s) adjudicated", lines.len())
        }
        ClaimEvent::Resolved => "resolved — books balance exactly".into(),
        ClaimEvent::Flagged { reason } => format!("flagged for review: {reason:?}"),
        ClaimEvent::RemittanceQuarantined => "remittance quarantined (unknown claim)".into(),
        ClaimEvent::GarbageRemittance => "garbage remittance received — treated as silence".into(),
        ClaimEvent::LateRemittance { .. } => "late remittance after terminal state".into(),
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

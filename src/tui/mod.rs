//! The interactive application shell: configure → run → explore → run again,
//! all in one process. The program opens on a configuration form (every knob
//! of the simulation editable with the arrow keys); Enter generates claims in
//! memory and streams them straight into the simulation; the finished run
//! opens a five-pane dashboard whose last pane is that same configuration
//! form, so the next run is always one Enter away.
//!
//! Pane map — the reports arranged as one instrument, money views first,
//! every pane scrollable by provider:
//!   1 A/R Aging            outcome bars + aging books as of one moment,
//!                          scrubbable a day at a time across the run
//!   2 Provider Insights    per-provider A/R analysis over the drained final
//!                          books — what's still open, and what to chase first
//!   3 Timeline             the run replayed as rate + backlog charts, per book
//!   4 Payer Scorecard      graded payers + denial detail (scorecard ∪ denials)
//!   5 Configuration        the startup form again — edit values, Enter reruns
//!
//! Threading: the TUI event loop owns the main thread on the *wall* clock;
//! each simulation runs on its own multi-thread runtime behind a spawned
//! thread (DESIGN.md 'Virtual time' — time is computed, so claim tasks parallelize).
//! They meet at two channels: the fold's live-progress watch tap, and a
//! channel carrying the finished `RunOutput`.

mod aging;
mod config;
mod insights;
mod payers;
mod theme;
mod timeline;

use std::sync::mpsc;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Gauge, Paragraph, TableState, Tabs};
use tokio::sync::watch;

use healthcare_billing_sim::RunOutput;
use healthcare_billing_sim::domain::human_virtual;
use healthcare_billing_sim::ledger::fold::Progress;
use healthcare_billing_sim::ledger::records::ClaimState;
use healthcare_billing_sim::reports::chase_list;
use healthcare_billing_sim::simconfig::SimConfig;

use theme::{ACCENT, BAD, GOOD, WARN, bold, dim};

/// Run the interactive session: configure, simulate, explore, repeat. Returns
/// the most recently *completed* run (with the configuration that produced
/// it) so the caller can leave a plain-text record on stdout — None if the
/// user quit before any run finished.
pub fn run(initial: SimConfig) -> anyhow::Result<Option<(SimConfig, RunOutput)>> {
    let mut terminal = ratatui::init();
    let mut app = App::new(initial);
    let result = event_loop(&mut terminal, &mut app);
    ratatui::restore();
    result?;
    Ok(app.into_record())
}

const AGING: usize = 0;
const PROVIDERS: usize = 1;
const PAYERS: usize = 3;
const CONFIG: usize = 4;
// Pane 2, Timeline, is the draw dispatch's wildcard arm.
const PANE_TITLES: [&str; 5] = [
    "A/R Aging",
    "Provider Insights",
    "Timeline",
    "Payer Scorecard",
    "Configuration",
];

/// One keys line, everywhere, always — the status box never rewords itself.
/// Pane-specific instructions live in the hint row of the pane they belong
/// to, and the full map is one `?` away.
const KEYS_HINT: &str =
    "←/→ panes · ↑/↓ select · enter steps in · esc steps out · ? keys · ctrl-c quit";

struct App {
    /// The configuration, editable across the whole session; each run takes
    /// a snapshot of it.
    cfg: SimConfig,
    form: config::Form,
    phase: Phase,
    /// The last completed run whose dashboard has been torn down by a rerun —
    /// kept so Ctrl-C mid-rerun still leaves a record in the scrollback.
    previous: Option<(SimConfig, RunOutput)>,
    pane: usize,
    /// On the dashboard, the configuration pane's edit grab: Enter takes the
    /// arrow keys for the form, Esc gives them back to the pane bar.
    config_grabbed: bool,
    frame: usize,
    help: bool,
}

enum Phase {
    /// The form owns the screen; no simulation yet.
    Configure,
    Running(Box<RunHandle>),
    Dashboard(Box<DashState>),
}

/// A simulation in flight on its own thread + runtime.
struct RunHandle {
    /// The configuration this run was started with — the banner shows this,
    /// not the live (possibly re-edited) form values.
    snapshot: SimConfig,
    progress_rx: watch::Receiver<Progress>,
    done_rx: mpsc::Receiver<anyhow::Result<RunOutput>>,
    thread: std::thread::JoinHandle<()>,
    started: Instant,
    progress: Progress,
}

struct DashState {
    snapshot: SimConfig,
    done: Done,
    wall: Duration,
    progress: Progress,
}

struct Done {
    output: RunOutput,
    timeline: timeline::TimelineView,
    payers: payers::PayersView,
    aging: aging::AgingView,
    insights: insights::Insights,
}

impl App {
    fn new(cfg: SimConfig) -> App {
        App {
            cfg,
            form: config::Form::default(),
            phase: Phase::Configure,
            previous: None,
            pane: AGING,
            config_grabbed: false,
            frame: 0,
            help: false,
        }
    }

    /// Launch a simulation from the current configuration. A dashboard being
    /// torn down here parks its output in `previous` for the exit record.
    fn start_run(&mut self) {
        if let Phase::Dashboard(dash) = std::mem::replace(&mut self.phase, Phase::Configure) {
            self.previous = Some((dash.snapshot, dash.done.output));
        }
        let snapshot = self.cfg.clone();
        let (progress_tx, progress_rx) = watch::channel(Progress::default());
        let (done_tx, done_rx) = mpsc::channel();
        let mut run_cfg = snapshot.to_run_config();
        run_cfg.progress = Some(progress_tx);
        let threads = snapshot.threads;
        let thread = std::thread::spawn(move || {
            // Multi-thread runtime (DESIGN.md 'Virtual time'): nothing sleeps, so no
            // paused clock — claim tasks execute in true parallel under the UI.
            let mut builder = tokio::runtime::Builder::new_multi_thread();
            if threads > 0 {
                builder.worker_threads(threads);
            }
            let result = builder
                .enable_all()
                .build()
                .map_err(anyhow::Error::from)
                .and_then(|rt| rt.block_on(healthcare_billing_sim::run(run_cfg)));
            let _ = done_tx.send(result);
        });
        self.phase = Phase::Running(Box::new(RunHandle {
            snapshot,
            progress_rx,
            done_rx,
            thread,
            started: Instant::now(),
            progress: Progress::default(),
        }));
    }

    fn into_record(self) -> Option<(SimConfig, RunOutput)> {
        match self.phase {
            Phase::Dashboard(dash) => {
                let dash = *dash;
                Some((dash.snapshot, dash.done.output))
            }
            // Quit mid-run or on the form: fall back to the last completed
            // run, if any. A still-running sim thread is deliberately left
            // behind — the process is exiting, and there is no cancel path
            // by design (structural shutdown, no signals).
            _ => self.previous,
        }
    }
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> anyhow::Result<()> {
    loop {
        poll_run(app)?;
        app.frame = app.frame.wrapping_add(1);
        terminal.draw(|frame| draw(frame, app))?;

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        // Terminals speaking an enhanced keyboard protocol report auto-repeat
        // as Repeat instead of a stream of Presses — treat both as a press so
        // holding a key behaves the same everywhere. Releases are noise.
        if key.kind == KeyEventKind::Release {
            continue;
        }
        if handle_key(app, key.code, key.modifiers) {
            return Ok(());
        }
    }
}

/// Check on a running simulation; on completion, swap the phase to the
/// dashboard.
fn poll_run(app: &mut App) -> anyhow::Result<()> {
    let Phase::Running(running) = &mut app.phase else {
        return Ok(());
    };
    running.progress = *running.progress_rx.borrow_and_update();
    let Ok(result) = running.done_rx.try_recv() else {
        return Ok(());
    };
    let Phase::Running(running) = std::mem::replace(&mut app.phase, Phase::Configure) else {
        unreachable!("matched Running above");
    };
    let _ = running.thread.join();
    let output = result?;
    let progress = final_progress(&output);
    let snapshot = running.snapshot;
    let done = Done::build(output, &snapshot);
    app.phase = Phase::Dashboard(Box::new(DashState {
        snapshot,
        done,
        wall: running.started.elapsed(),
        progress,
    }));
    app.pane = AGING;
    app.config_grabbed = false;
    Ok(())
}

/// Apply one keypress; returns true when the user asked to leave the UI.
/// Only Ctrl-C quits — Enter steps down a layer (or starts a run), Esc steps
/// back up, and at the top layer Esc is a no-op.
fn handle_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> bool {
    // Ctrl-C always leaves, overlays or not — and nothing else does.
    if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
        return true;
    }
    // The help overlay swallows everything: any key closes it, nothing quits.
    if app.help {
        app.help = false;
        return false;
    }
    if code == KeyCode::Char('?') {
        app.help = true;
        return false;
    }
    match app.phase {
        Phase::Configure => configure_key(app, code),
        Phase::Running(_) => {}
        Phase::Dashboard(_) => dashboard_key(app, code),
    }
    false
}

/// The startup screen: the form owns every key.
fn configure_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Up => app.form.move_cursor(&app.cfg, -1),
        KeyCode::Down => app.form.move_cursor(&app.cfg, 1),
        KeyCode::PageUp => app.form.move_cursor(&app.cfg, -10),
        KeyCode::PageDown => app.form.move_cursor(&app.cfg, 10),
        KeyCode::Left => app.form.adjust(&mut app.cfg, -1),
        KeyCode::Right => app.form.adjust(&mut app.cfg, 1),
        KeyCode::Enter => {
            if matches!(app.form.enter(&app.cfg), config::EnterAction::StartRun) {
                app.start_run();
            }
        }
        KeyCode::Esc => {
            app.form.escape(&app.cfg);
        }
        _ => {}
    }
}

fn dashboard_key(app: &mut App, code: KeyCode) {
    let panes = PANE_TITLES.len();
    // The configuration pane needs the arrow keys for value editing, so it
    // follows the aging pane's grab grammar: ↑/↓ always move the field
    // cursor, Enter grabs ←/→ for adjusting (and then starts the run), Esc
    // hands them back to the pane bar.
    if app.pane == CONFIG {
        match code {
            KeyCode::Char(c @ '1'..='5') => app.pane = c as usize - '1' as usize,
            KeyCode::Up => app.form.move_cursor(&app.cfg, -1),
            KeyCode::Down => app.form.move_cursor(&app.cfg, 1),
            KeyCode::PageUp => app.form.move_cursor(&app.cfg, -10),
            KeyCode::PageDown => app.form.move_cursor(&app.cfg, 10),
            KeyCode::Left if app.config_grabbed => app.form.adjust(&mut app.cfg, -1),
            KeyCode::Right if app.config_grabbed => app.form.adjust(&mut app.cfg, 1),
            KeyCode::Left => app.pane = (app.pane + panes - 1) % panes,
            KeyCode::Right => app.pane = (app.pane + 1) % panes,
            KeyCode::Enter if app.config_grabbed => {
                if matches!(app.form.enter(&app.cfg), config::EnterAction::StartRun) {
                    app.start_run();
                }
            }
            KeyCode::Enter => app.config_grabbed = true,
            KeyCode::Esc => {
                if !app.form.escape(&app.cfg) {
                    app.config_grabbed = false;
                }
            }
            _ => {}
        }
        return;
    }

    let Phase::Dashboard(dash) = &mut app.phase else {
        return;
    };
    let done = &mut dash.done;
    match code {
        // While the A/R timeline is grabbed, ←/→ scrub the as-of day; the
        // pane bar gets them back when Esc lets go.
        KeyCode::Left if app.pane == AGING && done.aging.timeline_grabbed() => {
            done.aging.step_day(-1, &done.output)
        }
        KeyCode::Right if app.pane == AGING && done.aging.timeline_grabbed() => {
            done.aging.step_day(1, &done.output)
        }
        KeyCode::Left => app.pane = (app.pane + panes - 1) % panes,
        KeyCode::Right => app.pane = (app.pane + 1) % panes,
        KeyCode::Char(c @ '1'..='5') => app.pane = c as usize - '1' as usize,
        KeyCode::Up if app.pane == PROVIDERS => done.insights.move_selection(-1, &done.output),
        KeyCode::Down if app.pane == PROVIDERS => done.insights.move_selection(1, &done.output),
        KeyCode::PageUp if app.pane == PROVIDERS => done.insights.move_selection(-10, &done.output),
        KeyCode::PageDown if app.pane == PROVIDERS => {
            done.insights.move_selection(10, &done.output)
        }
        KeyCode::Up if app.pane == PAYERS => done.payers.move_selection(-1),
        KeyCode::Down if app.pane == PAYERS => done.payers.move_selection(1),
        KeyCode::PageUp if app.pane == PAYERS => done.payers.move_selection(-10),
        KeyCode::PageDown if app.pane == PAYERS => done.payers.move_selection(10),
        KeyCode::Up if app.pane == AGING => done.aging.key_move(-1, &done.output),
        KeyCode::Down if app.pane == AGING => done.aging.key_move(1, &done.output),
        KeyCode::PageUp if app.pane == AGING => done.aging.key_move(-10, &done.output),
        KeyCode::PageDown if app.pane == AGING => done.aging.key_move(10, &done.output),
        KeyCode::Up => done.timeline.move_selection(-1, &done.output),
        KeyCode::Down => done.timeline.move_selection(1, &done.output),
        KeyCode::PageUp => done.timeline.move_selection(-10, &done.output),
        KeyCode::PageDown => done.timeline.move_selection(10, &done.output),
        // Enter steps down a layer; Esc steps back up, stopping at the pane
        // bar (it never quits — that's Ctrl-C's job alone).
        KeyCode::Enter if app.pane == AGING => done.aging.enter_report(),
        KeyCode::Esc if app.pane == AGING => {
            done.aging.escape();
        }
        KeyCode::Esc if app.pane == PROVIDERS => {
            done.insights.escape();
        }
        // Enter steps down into the analysis document — the terminal layer;
        // ↑/↓ then scroll it, Esc steps back to the provider list.
        KeyCode::Enter if app.pane == PROVIDERS => {
            done.insights.focus = insights::Focus::Analysis;
        }
        _ => {}
    }
}

/// Advance a table selection by `delta`, clamped; returns true if it moved.
pub(crate) fn step(table: &mut TableState, len: usize, delta: isize) -> bool {
    if len == 0 {
        return false;
    }
    let current = table.selected().unwrap_or(0) as isize;
    let next = (current + delta).clamp(0, len as isize - 1) as usize;
    let moved = table.selected() != Some(next);
    table.select(Some(next));
    moved
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
    fn build(output: RunOutput, snapshot: &SimConfig) -> Done {
        // The chase list feeds Provider Insights, which reads the drained
        // final books: what's still open at run completion is exactly the
        // pile a human has to work. (The A/R Aging pane keeps its own as-of
        // snapshot — mid-flight by default, scrubbable across the run.)
        let chase = chase_list(&output.ledger, output.finished_at, usize::MAX);
        let timeline = timeline::build(&output);
        // The Payer Scorecard pane shows configured personalities next to
        // the observed statistics — from the snapshot that actually ran.
        let payers = payers::build(
            &output,
            snapshot.payers.clone(),
            snapshot.payer_faults.clone(),
        );
        let aging = aging::build(&output);
        let insights = insights::Insights::build(&output, &chase);
        Done {
            output,
            timeline,
            payers,
            aging,
            insights,
        }
    }
}

fn draw(frame: &mut ratatui::Frame, app: &mut App) {
    let [tabs_area, content, status_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(4),
    ])
    .areas(frame.area());

    match &mut app.phase {
        Phase::Configure => {
            frame.render_widget(
                Line::from(vec![
                    Span::styled(
                        " healthcare billing simulation — configure ",
                        theme::accent_bold(),
                    ),
                    Span::styled("· enter starts the run", dim()),
                ]),
                tabs_area,
            );
            config::draw(frame, content, &app.cfg, &mut app.form, true);
        }
        Phase::Running(running) => draw_running(frame, tabs_area, content, running),
        Phase::Dashboard(dash) => {
            // The active tab is a solid block, not just a tinted word: padded
            // numbered title, black-on-cyan, with the rest of the bar dimmed.
            let titles: Vec<Line> = PANE_TITLES
                .iter()
                .enumerate()
                .map(|(i, title)| Line::from(format!(" {} {title} ", i + 1)))
                .collect();
            let tabs = Tabs::new(titles)
                .select(app.pane)
                .style(dim())
                .highlight_style(theme::bold().fg(Color::Black).bg(ACCENT))
                .divider("");
            frame.render_widget(tabs, tabs_area);

            let done = &mut dash.done;
            match app.pane {
                AGING => aging::draw(frame, content, &mut done.aging, &done.output),
                PAYERS => payers::draw(frame, content, &mut done.payers),
                PROVIDERS => insights::draw(frame, content, &mut done.insights),
                CONFIG => config::draw(frame, content, &app.cfg, &mut app.form, app.config_grabbed),
                _ => timeline::draw(frame, content, &mut done.timeline),
            }
        }
    }

    draw_status(frame, status_area, app);
    if app.help {
        draw_help(frame, frame.area());
    }
}

/// The waiting room: the run's configuration under a live drain gauge, so
/// the screen is telling the story before the first pane exists.
fn draw_running(frame: &mut ratatui::Frame, tabs_area: Rect, content: Rect, running: &RunHandle) {
    frame.render_widget(
        Line::from(vec![
            Span::styled(
                " healthcare billing simulation — running ",
                theme::accent_bold(),
            ),
            Span::styled("· the dashboard opens when the books drain", dim()),
        ]),
        tabs_area,
    );
    let [banner_area, gauge_area] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).areas(content);
    let banner_lines: Vec<Line> = running
        .snapshot
        .banner_rows()
        .into_iter()
        .map(|(label, value)| {
            Line::from(vec![
                Span::styled(format!(" {label:<22} "), bold()),
                Span::raw(value),
            ])
        })
        .collect();
    frame.render_widget(
        Paragraph::new(banner_lines).block(theme::panel("run configuration")),
        banner_area,
    );

    let p = &running.progress;
    let settled = p.resolved + p.rejected + p.flagged;
    let ratio = if p.claims > 0 {
        (settled as f64 / p.claims as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let gauge = Gauge::default()
        .block(theme::panel("claims driven to a terminal state"))
        .gauge_style(Style::default().fg(ACCENT).bg(Color::DarkGray))
        .ratio(ratio)
        .label(format!(
            "{} / {} settled",
            theme::thousands(settled as u64),
            theme::thousands(p.claims as u64)
        ));
    frame.render_widget(gauge, gauge_area);
}

fn draw_status(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    const FRAMES: [char; 4] = ['⠋', '⠙', '⠸', '⠴'];
    let headline = match &app.phase {
        Phase::Configure => vec![
            Span::styled(" ✎ configure · ", theme::accent_bold()),
            Span::raw(format!(
                "{} claims ready to generate · seed {}",
                theme::thousands(app.cfg.generator.count as u64),
                app.cfg.seed,
            )),
            Span::styled(" · enter starts the simulation", dim()),
        ],
        Phase::Running(running) => {
            let mut spans = vec![Span::styled(
                format!(" {} running · ", FRAMES[app.frame % FRAMES.len()]),
                theme::accent_bold(),
            )];
            spans.extend(progress_spans(&running.progress));
            spans.push(Span::styled(
                format!(" · {:.1}s wall", running.started.elapsed().as_secs_f64()),
                dim(),
            ));
            spans
        }
        Phase::Dashboard(dash) => {
            let mut spans = vec![Span::styled(" ✓ complete · ", theme::bold().fg(GOOD))];
            spans.extend(progress_spans(&dash.progress));
            spans.push(Span::styled(
                format!(
                    " · {:.2}s wall · seed {}",
                    dash.wall.as_secs_f64(),
                    dash.snapshot.seed
                ),
                dim(),
            ));
            spans
        }
    };
    // The headline carries live state; the keys line is deliberately the
    // same on every pane and in every phase — pane-specific instructions
    // live in the hint row of the pane they belong to.
    let status = Paragraph::new(vec![
        Line::from(headline),
        Line::from(Span::styled(format!(" {KEYS_HINT}"), dim())),
    ])
    .block(theme::panel("simulation"));
    frame.render_widget(status, area);
}

fn progress_spans(p: &Progress) -> Vec<Span<'static>> {
    vec![
        Span::styled(theme::thousands(p.claims as u64), bold()),
        Span::raw(" claims · "),
        Span::styled(
            theme::thousands(p.resolved as u64),
            Style::default().fg(GOOD),
        ),
        Span::raw(" resolved · "),
        Span::styled(
            theme::thousands(p.rejected as u64),
            Style::default().fg(WARN),
        ),
        Span::raw(" rejected · "),
        Span::styled(theme::thousands(p.flagged as u64), Style::default().fg(BAD)),
        Span::raw(" flagged · "),
        Span::raw(human_virtual(p.now.as_duration().as_secs_f64())),
    ]
}

/// The `?` overlay: every key in the app on one card.
fn draw_help(frame: &mut ratatui::Frame, area: Rect) {
    let key = |k: &str, action: &str| {
        Line::from(vec![
            Span::styled(format!("   {k:<12}"), Style::default().fg(ACCENT)),
            Span::raw(action.to_string()),
        ])
    };
    let note = |text: &str| Line::from(Span::styled(format!("   {text}"), dim()));
    let section = |title: &str| Line::from(Span::styled(format!(" {title}"), theme::accent_bold()));
    let lines = vec![
        section("Everywhere"),
        key("←/→", "move between panes"),
        key("1-5", "jump straight to a pane"),
        key("↑/↓", "scroll, or move the selection"),
        key("pgup/pgdn", "the same, ten at a time"),
        key("enter", "step down a layer — select, grab, drill in"),
        key(
            "esc",
            "step back up a layer — the top layer is the pane bar",
        ),
        key("ctrl-c", "quit — the plain report prints to stdout"),
        Line::default(),
        section("Configuration (startup screen, and pane 5)"),
        key("↑/↓", "move between fields"),
        key(
            "←/→",
            "adjust the selected value (on pane 5: after enter grabs)",
        ),
        key(
            "enter",
            "payer rows expand; anywhere else starts the simulation",
        ),
        key("esc", "collapse a payer, then hand the arrows back"),
        note("presets rewrite the fault fields in place; edits flip the label to 'custom'"),
        Line::default(),
        section("1 A/R Aging"),
        key("↑/↓", "pick a book — all providers first, then each one"),
        key(
            "enter",
            "into the report: ↑/↓ scroll, ←/→ move the books a day (hold 1s: ×4)",
        ),
        key("esc", "back out to the provider list"),
        note("buckets shade green → red as receivables age past 90 days"),
        Line::default(),
        section("2 Provider Insights"),
        key("↑/↓", "pick a provider; the analysis follows"),
        key("enter", "into the analysis document; ↑/↓ scroll it"),
        key("esc", "back up to the provider list"),
        Line::default(),
        section("3 Timeline"),
        key("↑/↓", "pick a book — the charts re-bucket to its claims"),
        note("rate charts share one y-scale; the backlog must drain to zero"),
        Line::default(),
        section("4 Payer Scorecard"),
        key("↑/↓", "pick a payer; the detail below follows"),
        note("grades are on the curve: denial rate + response time + paid share"),
        Line::default(),
        Line::from(Span::styled("   any key closes this card", dim())),
    ];
    let popup = theme::popup(area, 78, lines.len() as u16 + 2);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(theme::panel("keyboard map").border_style(Style::default().fg(ACCENT))),
        popup,
    );
}

/// Render smoke tests: run a real (small, messy) simulation and draw every
/// phase, pane and overlay into a TestBackend — the closest an automated
/// check gets to pressing the keys. A panic anywhere in layout/formatting
/// fails here.
#[cfg(test)]
mod render_tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use serde_json::json;

    use healthcare_billing_sim::ClaimSource;
    use healthcare_billing_sim::simconfig::Preset;

    use super::*;

    fn claim_line(i: usize) -> String {
        let payers = ["medicare", "anthem", "aetna", "cigna", "humana"];
        json!({
            "claim_id": format!("c-{i:03}"),
            "place_of_service_code": 11,
            "insurance": {"payer_id": payers[i % payers.len()], "patient_member_id": format!("M-{i}")},
            "patient": {"first_name": "Ada", "last_name": "Lovelace", "gender": "f", "dob": "1985-12-10"},
            "organization": {"name": format!("Clinic {}", i % 4)},
            "rendering_provider": {"first_name": "Grace", "last_name": "Hopper", "npi": "1234567890"},
            "service_lines": [{
                "service_line_id": "L1",
                "procedure_code": "99213",
                "units": 1,
                "details": "office visit",
                "unit_charge_currency": "USD",
                "unit_charge_amount": 100.0 + i as f64,
                "do_not_bill": false,
            }],
        })
        .to_string()
    }

    /// A small messy configuration whose run finishes in well under a second.
    fn small_cfg() -> SimConfig {
        SimConfig {
            seed: 7,
            rate_per_sec: 10.0,
            generator: healthcare_billing_sim::claimgen::GenConfig {
                count: 40,
                malformed_rate: 0.0,
                ..Default::default()
            },
            ..SimConfig::default()
        }
    }

    /// A finished App over a real messy run: 40 in-memory claims across five
    /// payers, ingested fast enough that receivables are still open at
    /// intake end. No file anywhere.
    fn done_app() -> App {
        let cfg = small_cfg();
        let lines: Vec<String> = (0..40).map(claim_line).collect();
        let mut run_cfg = cfg.to_run_config();
        run_cfg.source = ClaimSource::Lines(lines);
        let output = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(healthcare_billing_sim::run(run_cfg))
            .expect("run");
        dashboard_app(cfg, output)
    }

    fn dashboard_app(cfg: SimConfig, output: RunOutput) -> App {
        let progress = final_progress(&output);
        let done = Done::build(output, &cfg);
        let mut app = App::new(cfg.clone());
        app.phase = Phase::Dashboard(Box::new(DashState {
            snapshot: cfg,
            done,
            wall: Duration::from_secs(1),
            progress,
        }));
        app
    }

    fn render(app: &mut App) -> String {
        let backend = TestBackend::new(140, 44);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, app)).expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer.cell((x, y)).map_or(" ", |c| c.symbol()));
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn every_pane_and_overlay_renders() {
        let mut app = done_app();
        let markers = [
            "A/R Aging — all providers",
            "Providers —",
            "Backlog",
            "Configured —",
            "start simulation",
        ];
        for (pane, marker) in markers.iter().enumerate() {
            app.pane = pane;
            let text = render(&mut app);
            assert!(
                text.contains(marker),
                "pane {pane} is missing {marker:?}:\n{text}"
            );
        }

        // The help overlay sits on top of whatever pane is active.
        handle_key(&mut app, KeyCode::Char('?'), KeyModifiers::NONE);
        assert!(render(&mut app).contains("keyboard map"));
        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);

        // The providers pane renders the selected provider's analysis
        // document — headline first, every figure computed from the ledger.
        app.pane = PROVIDERS;
        let text = render(&mut app);
        assert!(text.contains("A/R analysis"), "no analysis doc:\n{text}");
        assert!(text.contains("HEADLINE"), "{text}");
        assert!(text.contains("outstanding across"), "{text}");
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);

        // The aging pane re-derives its report — outcome bars included —
        // for a single provider.
        app.pane = AGING;
        handle_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        let selected = match &app.phase {
            Phase::Dashboard(dash) => dash
                .done
                .aging
                .selected_name()
                .expect("a provider row")
                .to_string(),
            _ => unreachable!(),
        };
        let text = render(&mut app);
        assert!(
            text.contains(&format!("A/R Aging — {selected}")),
            "per-provider aging title missing:\n{text}"
        );
        assert!(
            text.contains("claims — ") && text.contains("money — "),
            "outcome bars missing from the aging report:\n{text}"
        );

        // The timeline pane re-buckets its charts for a single provider.
        app.pane = 2;
        handle_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        let selected = match &app.phase {
            Phase::Dashboard(dash) => dash
                .done
                .timeline
                .selected_name()
                .expect("a provider row")
                .to_string(),
            _ => unreachable!(),
        };
        let text = render(&mut app);
        assert!(
            text.contains(&format!("{selected} — ingested")),
            "per-provider timeline title missing:\n{text}"
        );
    }

    #[test]
    fn configure_screen_renders_the_form() {
        let mut app = App::new(SimConfig::default());
        let text = render(&mut app);
        assert!(text.contains("start simulation"), "{text}");
        assert!(text.contains("claims to generate"), "{text}");
        assert!(text.contains("clearinghouse faults"), "{text}");
        assert!(text.contains("configure"), "{text}");
    }

    #[test]
    fn enter_on_the_form_runs_the_simulation_to_a_dashboard() {
        let mut app = App::new(small_cfg());
        assert!(matches!(app.phase, Phase::Configure));
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(
            matches!(app.phase, Phase::Running(_)),
            "enter on the start row must launch the run"
        );
        wait_for_dashboard(&mut app);
        let text = render(&mut app);
        assert!(text.contains("A/R Aging — all providers"), "{text}");

        // Rerun from pane 5: grab the form, press enter on the start row.
        app.pane = CONFIG;
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE); // grab
        assert!(app.config_grabbed);
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE); // start
        assert!(matches!(app.phase, Phase::Running(_)));
        assert!(
            app.previous.is_some(),
            "the torn-down dashboard must be kept for the exit record"
        );
        wait_for_dashboard(&mut app);
        assert!(matches!(app.phase, Phase::Dashboard(_)));
        assert!(app.into_record().is_some());
    }

    fn wait_for_dashboard(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !matches!(app.phase, Phase::Dashboard(_)) {
            assert!(Instant::now() < deadline, "run did not finish in 30s");
            poll_run(app).expect("run failed");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn running_screen_renders_banner_and_gauge() {
        let mut app = App::new(small_cfg());
        // A run handle whose simulation is real but tiny; render before it
        // finishes as well as after — both must lay out.
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let text = render(&mut app);
        assert!(text.contains("run configuration"), "{text}");
        assert!(text.contains("terminal state"), "{text}");
        wait_for_dashboard(&mut app);
    }

    #[test]
    fn keys_route_between_panes_and_quit() {
        let mut app = done_app();
        handle_key(&mut app, KeyCode::Char('4'), KeyModifiers::NONE);
        assert_eq!(app.pane, PAYERS);
        handle_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        match &app.phase {
            Phase::Dashboard(dash) => assert_eq!(dash.done.payers.table.selected(), Some(1)),
            _ => unreachable!(),
        }
        // Right from the last pane wraps around to the first.
        handle_key(&mut app, KeyCode::Char('5'), KeyModifiers::NONE);
        assert_eq!(app.pane, CONFIG);
        handle_key(&mut app, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(app.pane, AGING);
        handle_key(&mut app, KeyCode::Char('2'), KeyModifiers::NONE);
        assert_eq!(app.pane, PROVIDERS);
        handle_key(&mut app, KeyCode::Char('a'), KeyModifiers::NONE);
        // Neither q nor Esc terminates anymore — only Ctrl-C does.
        assert!(!handle_key(
            &mut app,
            KeyCode::Char('q'),
            KeyModifiers::NONE
        ));
        assert!(!handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE));
        assert!(handle_key(
            &mut app,
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        ));
    }

    #[test]
    fn config_pane_edits_only_while_grabbed() {
        let mut app = done_app();
        app.pane = CONFIG;
        let seed_before = app.cfg.seed;
        // Ungrabbed: ←/→ still switch panes.
        handle_key(&mut app, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(app.pane, AGING);
        app.pane = CONFIG;
        // Grab, walk to the seed field, adjust it.
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let rows = app.form.rows(&app.cfg);
        let seed_row = rows
            .iter()
            .position(|r| *r == config::Row::Field(config::FieldId::Seed))
            .expect("seed row");
        app.form.cursor = seed_row;
        handle_key(&mut app, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(app.cfg.seed, seed_before + 1);
        assert_eq!(app.pane, CONFIG, "grabbed arrows must not switch panes");
        // Esc releases the grab; the pane bar gets the arrows back.
        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!app.config_grabbed);
        handle_key(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(app.pane, PAYERS);
    }

    #[test]
    fn enter_grabs_the_aging_timeline_and_esc_lets_go() {
        let mut app = done_app();
        app.pane = AGING;
        let grabbed = |app: &App| match &app.phase {
            Phase::Dashboard(dash) => dash.done.aging.timeline_grabbed(),
            _ => unreachable!(),
        };

        // Enter grabs the timeline: ←/→ scrub the as-of day, not the panes.
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(grabbed(&app));
        handle_key(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(app.pane, AGING, "← must scrub the day, not switch panes");
        let text = render(&mut app);
        assert!(text.contains("timeline"), "no scrubber row:\n{text}");
        assert!(
            text.contains("books as of"),
            "report must carry its as-of moment:\n{text}"
        );

        // Esc lets go; the arrows control the pane bar again.
        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!grabbed(&app));
        handle_key(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(app.pane, PANE_TITLES.len() - 1);
    }

    #[test]
    fn insights_enter_steps_down_and_esc_steps_up() {
        let mut app = done_app();
        app.pane = PROVIDERS;
        let focus = |app: &App| match &app.phase {
            Phase::Dashboard(dash) => dash.done.insights.focus,
            _ => unreachable!(),
        };
        let scroll = |app: &App| match &app.phase {
            Phase::Dashboard(dash) => dash.done.insights.scroll,
            _ => unreachable!(),
        };

        assert!(matches!(focus(&app), insights::Focus::Providers));
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(focus(&app), insights::Focus::Analysis));

        // With the document focused, ↑/↓ scroll it instead of moving the
        // provider selection.
        handle_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(scroll(&app), 1);
        handle_key(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(scroll(&app), 0);

        // Esc steps back up; the analysis is the terminal layer, so one Esc
        // lands on the providers, where Esc becomes a no-op — never a quit.
        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(focus(&app), insights::Focus::Providers));
        assert!(!handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(focus(&app), insights::Focus::Providers));
    }

    #[test]
    fn a_messy_preset_default_config_runs_generated_claims() {
        // The product path end to end: SimConfig → generated source → run.
        let mut cfg = small_cfg();
        cfg.generator.count = 60;
        cfg.generator.malformed_rate = 0.05;
        cfg.apply_preset(Preset::Messy);
        let output = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(healthcare_billing_sim::run(cfg.to_run_config()))
            .expect("run");
        assert!(!output.ledger.claims.is_empty());
        let mut app = dashboard_app(cfg, output);
        for pane in 0..PANE_TITLES.len() {
            app.pane = pane;
            render(&mut app);
        }
    }
}

/// Dev preview, not a check: `cargo test -- --ignored dump_frames` renders
/// every phase, pane and overlay over the default 10k generated run into
/// target/frames/*.txt — the fastest way to eyeball a layout change without
/// a terminal session.
#[cfg(test)]
mod frame_dump {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    fn dump(app: &mut App, dir: &std::path::Path, name: &str) {
        let backend = TestBackend::new(160, 45);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, app)).expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer.cell((x, y)).map_or(" ", |c| c.symbol()));
            }
            text.push('\n');
        }
        std::fs::write(dir.join(format!("{name}.txt")), text).expect("write frame");
    }

    #[test]
    #[ignore]
    fn dump_frames() {
        let cfg = SimConfig::default();
        let output = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("rt")
            .block_on(healthcare_billing_sim::run(cfg.to_run_config()))
            .expect("run");
        let progress = final_progress(&output);
        let done = Done::build(output, &cfg);
        let mut app = App::new(cfg.clone());
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/frames");
        std::fs::create_dir_all(&dir).expect("mkdir");

        // The configure screen, collapsed and with a payer expanded.
        dump(&mut app, &dir, "configure");
        app.form.cursor = app
            .form
            .rows(&app.cfg)
            .iter()
            .position(|r| matches!(r, config::Row::PayerHeader(_)))
            .expect("payer row");
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        // Enter on a payer row expands it rather than starting a run, so the
        // app is still on the configure screen here.
        dump(&mut app, &dir, "configure-payer-expanded");
        app.form = config::Form::default();

        app.phase = Phase::Dashboard(Box::new(DashState {
            snapshot: cfg,
            done,
            wall: Duration::from_secs(2),
            progress,
        }));
        for pane in 0..PANE_TITLES.len() {
            app.pane = pane;
            dump(&mut app, &dir, &format!("pane{pane}"));
        }
        // Per-provider variants of the two master–detail report panes.
        app.pane = AGING;
        handle_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        dump(&mut app, &dir, "pane0-provider");
        // The aging books scrubbed 60 days back from the end of intake.
        handle_key(&mut app, KeyCode::Up, KeyModifiers::NONE);
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        for _ in 0..60 {
            handle_key(&mut app, KeyCode::Left, KeyModifiers::NONE);
        }
        dump(&mut app, &dir, "pane0-scrubbed");
        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        app.pane = 2;
        handle_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        dump(&mut app, &dir, "pane2-provider");
        app.help = true;
        dump(&mut app, &dir, "help");
        app.help = false;
        app.pane = PROVIDERS;
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        dump(&mut app, &dir, "insights-analysis");
    }
}

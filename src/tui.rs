//! A live view of a WHOOP strap: heart rate, intervals, events and an offload.
//!
//! ```text
//! cargo run --bin tui                 # scan, connect, stream
//! cargo run --bin tui -- --family 5   # only consider WHOOP 5/MG
//! cargo run --bin tui -- --simulate   # a simulated strap, no hardware
//! ```
//!
//! `--simulate` drives [`poop::sim`], which builds real frames and runs
//! them through the real decoders. The header says so, because a reading from a
//! simulated strap is not a measurement and the two should never look alike on
//! screen.
//!
//! Keys
//! ----
//!   Tab      open the strap picker
//!   s        scan again
//!   r        toggle the proprietary live heart-rate stream
//!   o        start a history offload to whoop-history.ndjson
//!   a        abort an offload in progress
//!   b        buzz the strap, to confirm which one this is
//!   c        clear the trace and the logs
//!   d        disconnect
//!   q / Esc  quit
//!
//! Picker: ↑/↓ to move, Enter to connect, s to rescan, Esc to close.
//!
//! The offload here obeys the same rule as the CLI's: each chunk is written and
//! `fsync`ed before its acknowledgement is sent, and a write failure withholds
//! the acknowledgement and stops the session. A TUI that acknowledged first
//! would look identical on screen and quietly lose records.

use std::collections::VecDeque;
use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{
        Axis, Block, Borders, Chart, Clear, Dataset, GraphType, List, ListItem, ListState,
        Paragraph,
    },
    Frame, Terminal,
};
use tokio::sync::{mpsc, oneshot};

use poop::history::{HistorySession, Step, IDLE_TIMEOUT};
use poop::protocol::DeviceFamily;
use poop::sim::{self, SimConfig};
use poop::store::NdjsonSink;
use poop::strap::Strap;
use poop::types::WhoopEvent;
use poop::whoop_client::{WhoopClient, WhoopClientConfig, WhoopDevice};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Beats kept for the trace. At roughly one a second this is about four
/// minutes, which shows a trend without compressing it into a band.
const TRACE_POINTS: usize = 240;
/// Lines kept per log pane.
const LOG_LINES: usize = 200;
/// Frame interval. A WHOOP produces about one reading a second, so 10 FPS is
/// already far more than the data needs; it is the spinner that wants it.
const TICK: Duration = Duration::from_millis(100);
const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

// ── State ─────────────────────────────────────────────────────────────────────

enum Mode {
    Scanning,
    Connecting(String),
    Connected {
        name: String,
        family: DeviceFamily,
        /// Whether this is a simulation. Shown, never hidden.
        simulated: bool,
    },
    NoStraps,
    Disconnected,
}

/// What an offload is doing, so the footer can say so.
enum Offload {
    Off,
    Running {
        chunks: u64,
        records: u64,
        unknown: u64,
        path: PathBuf,
    },
    Done {
        records: u64,
        reason: String,
    },
    Failed(String),
}

struct App {
    mode: Mode,
    last_error: Option<String>,

    bpm: Option<u8>,
    rr_ms: Option<u32>,
    battery: Option<f32>,
    identity: Vec<String>,
    worn: Option<bool>,

    trace: VecDeque<f64>,
    events: VecDeque<String>,
    records: VecDeque<String>,

    offload: Offload,
    realtime: bool,
    /// Frames that failed their envelope check, counted rather than listed: a
    /// desynced link produces them in bursts and a list of identical lines
    /// hides everything else.
    rejected: u64,

    show_picker: bool,
    picker_entries: Vec<String>,
    picker_index: usize,
    picker_scanning: bool,
}

impl App {
    fn new() -> Self {
        Self {
            mode: Mode::Scanning,
            last_error: None,
            bpm: None,
            rr_ms: None,
            battery: None,
            identity: Vec::new(),
            worn: None,
            trace: VecDeque::with_capacity(TRACE_POINTS),
            events: VecDeque::with_capacity(LOG_LINES),
            records: VecDeque::with_capacity(LOG_LINES),
            offload: Offload::Off,
            realtime: false,
            rejected: 0,
            show_picker: false,
            picker_entries: Vec::new(),
            picker_index: 0,
            picker_scanning: false,
        }
    }

    fn clear(&mut self) {
        self.trace.clear();
        self.events.clear();
        self.records.clear();
        self.rejected = 0;
    }

    fn push_beat(&mut self, bpm: u8) {
        self.bpm = Some(bpm);
        if self.trace.len() == TRACE_POINTS {
            self.trace.pop_front();
        }
        self.trace.push_back(f64::from(bpm));
    }

    fn log_event(&mut self, line: String) {
        if self.events.len() == LOG_LINES {
            self.events.pop_front();
        }
        self.events.push_back(line);
    }

    fn log_record(&mut self, line: String) {
        if self.records.len() == LOG_LINES {
            self.records.pop_front();
        }
        self.records.push_back(line);
    }

    /// The trace's y range, fitted to the data.
    ///
    /// A fixed 40–200 axis renders a resting heart rate as a flat line, and the
    /// variation is the reason to draw it at all.
    fn y_bounds(&self) -> [f64; 2] {
        let (mut low, mut high) = self
            .trace
            .iter()
            .fold((f64::MAX, f64::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)));
        if !low.is_finite() || !high.is_finite() {
            return [40.0, 120.0];
        }
        if high - low < 10.0 {
            let middle = (high + low) / 2.0;
            low = middle - 5.0;
            high = middle + 5.0;
        }
        let pad = (high - low) * 0.1;
        [(low - pad).max(0.0), high + pad]
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn spinner() -> &'static str {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    SPINNER[(ms / 100) as usize % SPINNER.len()]
}

// ── Tasks ─────────────────────────────────────────────────────────────────────

struct ScanResult {
    straps: Vec<WhoopDevice>,
    error: Option<String>,
}

fn start_scan(config: WhoopClientConfig) -> oneshot::Receiver<ScanResult> {
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let result = match WhoopClient::new(config).scan_all().await {
            Ok(straps) => ScanResult {
                straps,
                error: None,
            },
            Err(e) => ScanResult {
                straps: Vec::new(),
                error: Some(format!("{e:#}")),
            },
        };
        let _ = tx.send(result);
    });
    rx
}

type Connected = (mpsc::Receiver<WhoopEvent>, Strap);

fn start_connect(
    config: WhoopClientConfig,
    device: WhoopDevice,
) -> oneshot::Receiver<Result<Connected>> {
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let result = WhoopClient::new(config)
            .connect_to(device)
            .await
            .map(|(events, handle)| (events, Strap::live(handle)));
        let _ = tx.send(result);
    });
    rx
}

/// The event pump: drains the client's channel into the shared app state, and
/// drives an offload when one is running.
///
/// The offload lives here rather than in the UI loop because the durability
/// step is a blocking file write: doing it between two `draw` calls would stall
/// the frame, and more importantly it must complete before the acknowledgement
/// is sent, which is not something to interleave with keyboard handling.
fn spawn_pump(
    mut events: mpsc::Receiver<WhoopEvent>,
    app: Arc<Mutex<App>>,
    handle: Strap,
    mut offload_requests: mpsc::Receiver<OffloadRequest>,
) {
    tokio::spawn(async move {
        let mut session: Option<(HistorySession, NdjsonSink, PathBuf)> = None;
        // The idle watchdog's anchor, advanced only by offload progress — see
        // `HistorySession::is_progress`. Without it a strap that went quiet
        // mid-transfer would leave the footer saying "offload: running" for as
        // long as the page stayed open, because the standard Heart Rate Service
        // keeps notifying either way.
        let mut last_progress = Instant::now();

        loop {
            // Far enough out to be inert when no offload is running: a `select!`
            // branch cannot be omitted conditionally, so the deadline is moved
            // instead of the branch.
            let deadline = match &session {
                Some(_) => tokio::time::Instant::from_std(last_progress + IDLE_TIMEOUT),
                None => tokio::time::Instant::now() + Duration::from_secs(3600),
            };

            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {
                    if let Some((active, writer, _)) = session.take() {
                        let _ = writer.finish();
                        let _ = handle.abort_history().await;
                        app.lock().unwrap().offload = Offload::Failed(format!(
                            "strap went silent after {} record(s)",
                            active.records_seen()
                        ));
                    }
                }

                request = offload_requests.recv() => {
                    match request {
                        Some(OffloadRequest::Start(path)) => {
                            match NdjsonSink::create(&path) {
                                Ok(writer) => {
                                    session = Some((
                                        HistorySession::new(handle.family()),
                                        writer,
                                        path.clone(),
                                    ));
                                    // Scoped so the guard is provably gone
                                    // before the await below: a spawned task
                                    // must stay `Send`, and a `MutexGuard` held
                                    // across a suspension point is not.
                                    {
                                        app.lock().unwrap().offload = Offload::Running {
                                            chunks: 0,
                                            records: 0,
                                            unknown: 0,
                                            path,
                                        };
                                    }
                                    last_progress = Instant::now();
                                    if let Err(e) = handle.begin_history().await {
                                        session = None;
                                        app.lock().unwrap().offload =
                                            Offload::Failed(format!("{e:#}"));
                                    }
                                }
                                Err(e) => {
                                    app.lock().unwrap().offload =
                                        Offload::Failed(format!("{e:#}"));
                                }
                            }
                        }
                        Some(OffloadRequest::Abort) => {
                            let _ = handle.abort_history().await;
                            if let Some((session, writer, _)) = session.take() {
                                let _ = writer.finish();
                                app.lock().unwrap().offload = Offload::Done {
                                    records: session.records_seen(),
                                    reason: "aborted".into(),
                                };
                            }
                        }
                        None => return,
                    }
                }

                event = events.recv() => {
                    let Some(event) = event else { break };

                    if HistorySession::is_progress(&event) {
                        last_progress = Instant::now();
                    }

                    // Offload first: the session needs every history event in
                    // arrival order, and the UI only needs a summary.
                    if let Some((active, writer, path)) = session.as_mut() {
                        match active.feed(&event) {
                            Step::Chunk(chunk) => {
                                let count = chunk.len();
                                // Durable, then — and only then — the token.
                                match writer.write_chunk(&chunk) {
                                    Ok(()) => {
                                        for record in &chunk.records {
                                            app.lock().unwrap().log_record(record.summary());
                                        }
                                        {
                                            let mut state = app.lock().unwrap();
                                            if let Offload::Running {
                                                chunks, records, unknown, ..
                                            } = &mut state.offload {
                                                *chunks += 1;
                                                *records += count as u64;
                                                *unknown = active.unknown_records();
                                            }
                                        }
                                        if let Some(ack) = chunk.committed() {
                                            if let Err(e) = handle.ack_history(&ack).await {
                                                app.lock().unwrap().offload =
                                                    Offload::Failed(format!("ack failed: {e:#}"));
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        // No acknowledgement: the strap keeps
                                        // the chunk for the next attempt.
                                        app.lock().unwrap().offload = Offload::Failed(format!(
                                            "{e:#} — not acknowledged, strap kept the chunk"
                                        ));
                                        let _ = handle.abort_history().await;
                                        session = None;
                                    }
                                }
                            }
                            Step::Complete => {
                                let records = active.records_seen();
                                let unknown = active.unknown_records();
                                let where_to = path.display().to_string();
                                if let Some((_, writer, _)) = session.take() {
                                    let _ = writer.finish();
                                }
                                let note = if unknown > 0 {
                                    format!("{unknown} undecoded, written raw → {where_to}")
                                } else {
                                    format!("written to {where_to}")
                                };
                                app.lock().unwrap().offload =
                                    Offload::Done { records, reason: note };
                            }
                            _ => {}
                        }
                    }

                    let mut state = app.lock().unwrap();
                    match event {
                        WhoopEvent::Connected { name, family } => {
                            state.mode = Mode::Connected {
                                name,
                                family,
                                simulated: handle.is_simulated(),
                            };
                        }
                        WhoopEvent::HeartRate(hr) => {
                            state.push_beat(hr.bpm);
                            if let Some(&last) = hr.rr_ms.last() {
                                state.rr_ms = Some(last);
                            }
                        }
                        WhoopEvent::Battery(b) => state.battery = Some(b.percent),
                        WhoopEvent::Event(e) => {
                            match e.number {
                                9 => state.worn = Some(true),
                                10 => state.worn = Some(false),
                                _ => {}
                            }
                            state.log_event(format!("{:>10}  {}", e.unix_seconds, e.name));
                        }
                        WhoopEvent::ConsoleLog(text) => {
                            state.log_event(format!("{:>10}  {}", "log", text.trim_end()))
                        }
                        WhoopEvent::Hello(h) => {
                            state.identity.push(format!("fw {}", h.firmware))
                        }
                        WhoopEvent::HistoryMeta(meta) => {
                            state.log_event(format!("{:>10}  history {:?}", "", meta.kind))
                        }
                        WhoopEvent::Rejected(_) => state.rejected += 1,
                        WhoopEvent::Disconnected => {
                            state.mode = Mode::Disconnected;
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }

        if let Some((_, writer, _)) = session.take() {
            let _ = writer.finish();
        }
        let mut state = app.lock().unwrap();
        if !matches!(state.mode, Mode::Disconnected) {
            state.mode = Mode::Disconnected;
        }
    });
}

enum OffloadRequest {
    Start(PathBuf),
    Abort,
}

// ── Render ────────────────────────────────────────────────────────────────────

fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let root = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(5),
        Constraint::Min(6),
        Constraint::Length(10),
        Constraint::Length(3),
    ])
    .split(area);

    draw_header(frame, root[0], app);
    draw_tiles(frame, root[1], app);
    draw_trace(frame, root[2], app);
    draw_logs(frame, root[3], app);
    draw_footer(frame, root[4], app);

    if app.show_picker {
        draw_picker(frame, area, app);
    }
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let (label, color) = match &app.mode {
        Mode::Scanning => (format!("{} scanning…", spinner()), Color::Yellow),
        Mode::Connecting(name) => (
            format!("{} connecting to {name}…", spinner()),
            Color::Yellow,
        ),
        Mode::Connected {
            name,
            family,
            simulated,
        } => {
            if *simulated {
                // A different colour and an explicit word, so a screenshot of
                // invented data cannot be mistaken for a reading.
                (
                    format!("◆ {name}  {}  SIMULATED", family.label()),
                    Color::Cyan,
                )
            } else {
                (format!("● {name}  {}", family.label()), Color::Green)
            }
        }
        Mode::NoStraps => (
            format!("{} no strap found — press s to scan again", spinner()),
            Color::Yellow,
        ),
        Mode::Disconnected => {
            let reason = app
                .last_error
                .as_deref()
                .map(|e| format!(" — {e}"))
                .unwrap_or_default();
            (format!("○ disconnected{reason}"), Color::Red)
        }
    };

    let mut spans = vec![
        Span::styled("poop", Style::default().add_modifier(Modifier::BOLD)),
        separator(),
        Span::styled(label, Style::default().fg(color)),
    ];
    if !app.identity.is_empty() {
        spans.push(separator());
        spans.push(Span::styled(
            app.identity.join(" · "),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if app.rejected > 0 {
        spans.push(separator());
        spans.push(Span::styled(
            format!("{} bad frame(s)", app.rejected),
            Style::default().fg(Color::Yellow),
        ));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn separator<'a>() -> Span<'a> {
    Span::styled("  │  ", Style::default().fg(Color::DarkGray))
}

fn draw_tiles(frame: &mut Frame, area: Rect, app: &App) {
    let columns = Layout::horizontal([
        Constraint::Ratio(1, 4),
        Constraint::Ratio(1, 4),
        Constraint::Ratio(1, 4),
        Constraint::Ratio(1, 4),
    ])
    .split(area);

    let tile = |frame: &mut Frame, area: Rect, title: &str, value: String, color: Color| {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                value,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )))
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {title} ")),
            ),
            area,
        );
    };

    tile(
        frame,
        columns[0],
        "heart rate",
        app.bpm.map_or("—".into(), |b| format!("{b} bpm")),
        Color::Red,
    );
    tile(
        frame,
        columns[1],
        "last r-r",
        app.rr_ms.map_or("—".into(), |ms| format!("{ms} ms")),
        Color::Magenta,
    );
    tile(
        frame,
        columns[2],
        "battery",
        app.battery.map_or("—".into(), |p| format!("{p:.0}%")),
        Color::Green,
    );
    tile(
        frame,
        columns[3],
        "on wrist",
        match app.worn {
            Some(true) => "yes".into(),
            Some(false) => "no".into(),
            // Unknown until the strap says so. A default of "yes" would be a
            // guess printed as a reading.
            None => "—".into(),
        },
        Color::Cyan,
    );
}

fn draw_trace(frame: &mut Frame, area: Rect, app: &App) {
    let points: Vec<(f64, f64)> = app
        .trace
        .iter()
        .enumerate()
        .map(|(i, &v)| (i as f64, v))
        .collect();

    let [low, high] = app.y_bounds();
    let dataset = Dataset::default()
        .marker(symbols::Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::Red))
        .data(&points);

    let chart = Chart::new(vec![dataset])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" heart rate — last 4 min "),
        )
        .x_axis(
            Axis::default()
                .bounds([0.0, TRACE_POINTS as f64])
                .style(Style::default().fg(Color::DarkGray)),
        )
        .y_axis(
            Axis::default()
                .bounds([low, high])
                .labels(vec![format!("{low:.0}"), format!("{high:.0}")])
                .style(Style::default().fg(Color::DarkGray)),
        );

    frame.render_widget(chart, area);
}

fn draw_logs(frame: &mut Frame, area: Rect, app: &App) {
    let columns =
        Layout::horizontal([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)]).split(area);

    let render = |frame: &mut Frame, area: Rect, title: &str, lines: &VecDeque<String>| {
        let rows = area.height.saturating_sub(2) as usize;
        let items: Vec<ListItem> = lines
            .iter()
            .rev()
            .take(rows)
            .map(|line| ListItem::new(line.clone()))
            .collect();
        frame.render_widget(
            List::new(items).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {title} ")),
            ),
            area,
        );
    };

    render(frame, columns[0], "events", &app.events);
    render(frame, columns[1], "records", &app.records);
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let mut spans = vec![
        key("tab"),
        Span::raw(" straps  "),
        key("s"),
        Span::raw(" scan  "),
        key("r"),
        Span::raw(if app.realtime {
            " live hr off  "
        } else {
            " live hr on  "
        }),
        key("o"),
        Span::raw(" offload  "),
        key("a"),
        Span::raw(" abort  "),
        key("b"),
        Span::raw(" buzz  "),
        key("c"),
        Span::raw(" clear  "),
        key("q"),
        Span::raw(" quit"),
    ];

    // The offload state replaces nothing; it is appended, because the keys stay
    // useful while one runs.
    let status = match &app.offload {
        Offload::Off => None,
        Offload::Running {
            chunks,
            records,
            unknown,
            path,
        } => Some((
            format!(
                "{} offload: {records} record(s), {chunks} chunk(s){} → {}",
                spinner(),
                if *unknown > 0 {
                    format!(", {unknown} undecoded")
                } else {
                    String::new()
                },
                path.display()
            ),
            Color::Cyan,
        )),
        Offload::Done { records, reason } => Some((
            format!("offload done: {records} record(s), {reason}"),
            Color::Green,
        )),
        Offload::Failed(e) => Some((format!("offload failed: {e}"), Color::Red)),
    };
    if let Some((text, color)) = status {
        spans.push(separator());
        spans.push(Span::styled(text, Style::default().fg(color)));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn key(label: &str) -> Span<'_> {
    Span::styled(
        label,
        Style::default()
            .fg(Color::Black)
            .bg(Color::Gray)
            .add_modifier(Modifier::BOLD),
    )
}

fn draw_picker(frame: &mut Frame, area: Rect, app: &App) {
    let width = area.width.saturating_sub(8).min(72);
    let height = (app.picker_entries.len() as u16 + 4).clamp(6, area.height.saturating_sub(4));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let title = if app.picker_scanning {
        format!(" {} scanning… ", spinner())
    } else {
        format!(" {} strap(s) — enter to connect ", app.picker_entries.len())
    };

    if app.picker_entries.is_empty() {
        frame.render_widget(
            Paragraph::new("nothing in range — press s to scan again")
                .block(Block::default().borders(Borders::ALL).title(title)),
            popup,
        );
        return;
    }

    let items: Vec<ListItem> = app
        .picker_entries
        .iter()
        .map(|entry| ListItem::new(entry.clone()))
        .collect();
    let mut state = ListState::default();
    state.select(Some(app.picker_index));
    frame.render_stateful_widget(
        List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        popup,
        &mut state,
    );
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Called from `bin/tui.rs`, which is the binary's entry point; this is the
/// body.
#[tokio::main]
pub async fn run() -> Result<()> {
    use std::io::IsTerminal as _;
    if !io::stdout().is_terminal() {
        eprintln!("Error: the poop TUI needs a real terminal (TTY).");
        eprintln!("Run it in a terminal emulator, not piped or redirected.");
        std::process::exit(1);
    }

    // Logs go to a file so they never interfere with the display. The BLE layer
    // is chatty and a stray line would corrupt the frame:
    //   RUST_LOG=debug cargo run --bin tui
    if let Ok(file) = File::create("poop-tui.log") {
        let _ = env_logger::Builder::from_default_env()
            .target(env_logger::Target::Pipe(Box::new(file)))
            .try_init();
    }

    let mut config = WhoopClientConfig {
        scan_timeout_secs: 6,
        ..WhoopClientConfig::default()
    };
    // A tiny argument surface: the CLI is the place for options, and the one
    // choice that changes what the TUI can see is which generation to look for.
    let mut simulate = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--simulate" => simulate = true,
            "-f" | "--family" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--family needs 4 or 5"))?;
                config.family = Some(match value.as_str() {
                    "4" | "4.0" => DeviceFamily::Whoop4,
                    "5" | "5.0" | "mg" | "MG" => DeviceFamily::Whoop5,
                    other => return Err(anyhow!("unknown family {other}; use 4 or 5")),
                });
            }
            "-h" | "--help" => {
                println!("usage: tui [--family 4|5] [--simulate]");
                return Ok(());
            }
            other => return Err(anyhow!("unknown argument {other}")),
        }
    }

    let app = Arc::new(Mutex::new(App::new()));
    let mut straps: Vec<WhoopDevice> = Vec::new();
    let mut pending_connect: Option<oneshot::Receiver<Result<Connected>>> = None;
    let mut handle: Option<Strap> = None;
    let mut offload_tx: Option<mpsc::Sender<OffloadRequest>> = None;

    // A simulation needs no radio, so it skips the scan entirely rather than
    // waiting out a window that cannot find anything.
    let mut pending_scan = if simulate {
        let sim_config = SimConfig {
            family: config.family.unwrap_or(DeviceFamily::Whoop5),
            ..SimConfig::default()
        };
        let (events, sim_handle) = sim::connect(sim_config);
        let strap = Strap::sim(sim_handle);
        let (tx, rx) = mpsc::channel(4);
        offload_tx = Some(tx);
        spawn_pump(events, Arc::clone(&app), strap.clone(), rx);
        handle = Some(strap);
        None
    } else {
        app.lock().unwrap().picker_scanning = true;
        Some(start_scan(config.clone()))
    };

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let mut last_draw = Instant::now();

    'main: loop {
        // ── Finished scans ───────────────────────────────────────────────────
        if let Some(rx) = pending_scan.as_mut() {
            if let Ok(result) = rx.try_recv() {
                pending_scan = None;
                straps = result.straps;
                let mut state = app.lock().unwrap();
                state.picker_entries = straps.iter().map(WhoopDevice::label).collect();
                state.picker_scanning = false;
                state.picker_index = 0;
                if straps.is_empty() {
                    state.mode = Mode::NoStraps;
                    state.last_error = result.error;
                } else if handle.is_none() && !state.show_picker {
                    // One strap and nothing connected: go straight there, which
                    // is what someone running this with their own strap on
                    // wants. Several, and the picker opens instead of guessing.
                    let device = straps[0].clone();
                    state.mode = Mode::Connecting(device.name.clone());
                    drop(state);
                    pending_connect = Some(start_connect(config.clone(), device));
                    if straps.len() > 1 {
                        app.lock().unwrap().show_picker = true;
                    }
                }
            }
        }

        // ── Finished connects ────────────────────────────────────────────────
        if let Some(rx) = pending_connect.as_mut() {
            if let Ok(result) = rx.try_recv() {
                pending_connect = None;
                match result {
                    Ok((events, connected)) => {
                        let (tx, rx) = mpsc::channel(4);
                        offload_tx = Some(tx);
                        // `Strap` is an enum over two `Arc`s, so cloning it is
                        // a refcount bump — no second layer needed.
                        spawn_pump(events, Arc::clone(&app), connected.clone(), rx);
                        handle = Some(connected);
                        let mut state = app.lock().unwrap();
                        state.show_picker = false;
                        state.last_error = None;
                    }
                    Err(e) => {
                        let mut state = app.lock().unwrap();
                        state.mode = Mode::Disconnected;
                        state.last_error = Some(format!("{e:#}"));
                    }
                }
            }
        }

        // ── Draw ─────────────────────────────────────────────────────────────
        if last_draw.elapsed() >= TICK {
            let state = app.lock().unwrap();
            terminal.draw(|frame| draw(frame, &state))?;
            drop(state);
            last_draw = Instant::now();
        }

        // ── Keys ─────────────────────────────────────────────────────────────
        if !event::poll(TICK)? {
            continue;
        }
        let Event::Key(pressed) = event::read()? else {
            continue;
        };
        if !pressed.is_press() {
            continue;
        }

        let picker_open = app.lock().unwrap().show_picker;
        if picker_open {
            match pressed.code {
                KeyCode::Esc => app.lock().unwrap().show_picker = false,
                KeyCode::Up => {
                    let mut state = app.lock().unwrap();
                    state.picker_index = state.picker_index.saturating_sub(1);
                }
                KeyCode::Down => {
                    let mut state = app.lock().unwrap();
                    let last = state.picker_entries.len().saturating_sub(1);
                    state.picker_index = (state.picker_index + 1).min(last);
                }
                KeyCode::Enter => {
                    let index = app.lock().unwrap().picker_index;
                    if let Some(device) = straps.get(index).cloned() {
                        if let Some(old) = handle.take() {
                            let _ = old.disconnect().await;
                        }
                        offload_tx = None;
                        let mut state = app.lock().unwrap();
                        state.mode = Mode::Connecting(device.name.clone());
                        state.show_picker = false;
                        state.clear();
                        drop(state);
                        pending_connect = Some(start_connect(config.clone(), device));
                    }
                }
                KeyCode::Char('s') => {
                    app.lock().unwrap().picker_scanning = true;
                    pending_scan = Some(start_scan(config.clone()));
                }
                KeyCode::Char('q') => break 'main,
                _ => {}
            }
            continue;
        }

        match pressed.code {
            KeyCode::Char('q') | KeyCode::Esc => break 'main,
            KeyCode::Char('c') if pressed.modifiers.contains(KeyModifiers::CONTROL) => break 'main,
            KeyCode::Tab => app.lock().unwrap().show_picker = true,
            KeyCode::Char('s') => {
                app.lock().unwrap().picker_scanning = true;
                pending_scan = Some(start_scan(config.clone()));
            }
            KeyCode::Char('r') => {
                if let Some(h) = handle.clone() {
                    let mut state = app.lock().unwrap();
                    state.realtime = !state.realtime;
                    let on = state.realtime;
                    drop(state);
                    let app = Arc::clone(&app);
                    tokio::spawn(async move {
                        if let Err(e) = h.toggle_realtime_hr(on).await {
                            app.lock().unwrap().last_error = Some(format!("{e:#}"));
                        }
                    });
                }
            }
            KeyCode::Char('o') => {
                if let Some(tx) = offload_tx.clone() {
                    let running = matches!(app.lock().unwrap().offload, Offload::Running { .. });
                    if !running {
                        let _ = tx
                            .send(OffloadRequest::Start(PathBuf::from("whoop-history.ndjson")))
                            .await;
                    }
                }
            }
            KeyCode::Char('a') => {
                if let Some(tx) = offload_tx.clone() {
                    let _ = tx.send(OffloadRequest::Abort).await;
                }
            }
            KeyCode::Char('b') => {
                if let Some(h) = handle.clone() {
                    let app = Arc::clone(&app);
                    tokio::spawn(async move {
                        if let Err(e) = h.buzz(1).await {
                            app.lock().unwrap().last_error = Some(format!("{e:#}"));
                        }
                    });
                }
            }
            KeyCode::Char('c') => app.lock().unwrap().clear(),
            KeyCode::Char('d') => {
                if let Some(h) = handle.take() {
                    let _ = h.disconnect().await;
                }
                offload_tx = None;
                app.lock().unwrap().mode = Mode::Disconnected;
            }
            _ => {}
        }
    }

    // ── Teardown ─────────────────────────────────────────────────────────────
    if let Some(h) = handle {
        let _ = h.disconnect().await;
    }
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

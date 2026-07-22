use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::io::{self, Stdout};
use std::pin::Pin;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Gauge, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use tokio::time::MissedTickBehavior;

use crate::connectivity::{ConnectivityKind, ConnectivityStatus};
use crate::setup_ui::set_tui_terminal_active;
use crate::state::{AppEvent, FeedKind};
use crate::trading::TradingEvent;

pub const FRAME_INTERVAL: Duration = Duration::from_millis(80);
pub const MAX_LOG_ENTRIES: usize = 200;
const MAX_LOG_MESSAGE_CHARS: usize = 240;
const MIN_TERMINAL_WIDTH: u16 = 80;
const MIN_TERMINAL_HEIGHT: u16 = 24;
const MAX_CARD_WIDTH: u16 = 96;
const COPY_NOTICE_FRAMES: u16 = 40;
const STATUS_PULSE_FRAMES: u64 = 32;

// This palette deliberately matches the first-run TUI and browser dashboard.
const BACKGROUND: Color = Color::Rgb(0, 0, 0);
const SURFACE: Color = Color::Rgb(10, 10, 10);
const SURFACE_RAISED: Color = Color::Rgb(21, 21, 21);
const BORDER: Color = Color::Rgb(36, 36, 36);
const TEXT: Color = Color::Rgb(232, 232, 232);
const MUTED: Color = Color::Rgb(153, 153, 153);
const ACCENT: Color = Color::Rgb(255, 153, 0);
const SUCCESS: Color = Color::Rgb(0, 255, 136);
const WARNING: Color = Color::Rgb(255, 204, 0);
const DANGER: Color = Color::Rgb(255, 77, 95);

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Debug)]
pub struct RuntimeUiCancelled;

impl fmt::Display for RuntimeUiCancelled {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PolyTread launch cancelled")
    }
}

impl Error for RuntimeUiCancelled {}

pub fn is_cancelled(error: &anyhow::Error) -> bool {
    error.downcast_ref::<RuntimeUiCancelled>().is_some()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchStepState {
    Pending,
    Running,
    Complete,
    Warning,
    Skipped,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LaunchStep {
    label: String,
    detail: Option<String>,
    state: LaunchStepState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LaunchProgress {
    steps: Vec<LaunchStep>,
}

impl LaunchProgress {
    fn new(labels: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            steps: labels
                .into_iter()
                .map(|label| LaunchStep {
                    label: label.into(),
                    detail: None,
                    state: LaunchStepState::Pending,
                })
                .collect(),
        }
    }

    fn update(&mut self, index: usize, state: LaunchStepState, detail: String) {
        let step = self
            .steps
            .get_mut(index)
            .expect("runtime launch step index is defined statically");
        step.state = state;
        step.detail = Some(detail);
    }

    fn fail_active(&mut self, detail: String) {
        let index = self
            .steps
            .iter()
            .position(|step| step.state == LaunchStepState::Running)
            .or_else(|| {
                self.steps
                    .iter()
                    .position(|step| step.state == LaunchStepState::Pending)
            });
        if let Some(index) = index {
            self.update(index, LaunchStepState::Failed, detail);
        }
    }

    fn completed_count(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| {
                matches!(
                    step.state,
                    LaunchStepState::Complete | LaunchStepState::Warning | LaunchStepState::Skipped
                )
            })
            .count()
    }

    fn ratio(&self) -> f64 {
        if self.steps.is_empty() {
            0.0
        } else {
            self.completed_count() as f64 / self.steps.len() as f64
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeLogLevel {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeLog {
    timestamp: String,
    level: RuntimeLogLevel,
    message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FeedObservation {
    connected: bool,
    reconnects: u64,
    status: String,
    last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeHealth {
    Starting,
    Active,
    Degraded,
    Attention,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeAction {
    Continue,
    Detach,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DashboardNotice {
    message: String,
    level: RuntimeLogLevel,
    frames_remaining: u16,
}

pub struct RuntimeUi {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    tick: u64,
    progress: LaunchProgress,
    dashboard_url: Option<String>,
    dashboard_notice: Option<DashboardNotice>,
    logs: VecDeque<RuntimeLog>,
    connectivity: Option<ConnectivityStatus>,
    feeds: BTreeMap<FeedKind, FeedObservation>,
    last_discovery: Vec<String>,
    last_trading_status: Option<String>,
    last_portfolio_error: Option<String>,
}

impl RuntimeUi {
    pub fn enter(labels: impl IntoIterator<Item = impl Into<String>>) -> Result<Self> {
        enable_raw_mode().context("failed to enable terminal raw mode")?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, Hide) {
            restore_terminal();
            return Err(error).context("failed to open the PolyTread launch screen");
        }
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(error) => {
                restore_terminal();
                return Err(error).context("failed to initialize the PolyTread launch screen");
            }
        };
        if let Err(error) = terminal.clear() {
            restore_terminal();
            return Err(error).context("failed to clear the PolyTread launch screen");
        }
        set_tui_terminal_active(true);
        Ok(Self {
            terminal,
            tick: 0,
            progress: LaunchProgress::new(labels),
            dashboard_url: None,
            dashboard_notice: None,
            logs: VecDeque::with_capacity(MAX_LOG_ENTRIES),
            connectivity: None,
            feeds: BTreeMap::new(),
            last_discovery: Vec::new(),
            last_trading_status: None,
            last_portfolio_error: None,
        })
    }

    pub fn running(&mut self, index: usize, detail: impl Into<String>) {
        self.progress
            .update(index, LaunchStepState::Running, detail.into());
    }

    pub fn complete(&mut self, index: usize, detail: impl Into<String>) {
        let detail = detail.into();
        let label = self.progress.steps[index].label.clone();
        self.progress
            .update(index, LaunchStepState::Complete, detail.clone());
        self.push_log(RuntimeLogLevel::Success, format!("{label}: {detail}"));
    }

    pub fn warning(&mut self, index: usize, detail: impl Into<String>) {
        let detail = detail.into();
        let label = self.progress.steps[index].label.clone();
        self.progress
            .update(index, LaunchStepState::Warning, detail.clone());
        self.push_log(RuntimeLogLevel::Warning, format!("{label}: {detail}"));
    }

    pub fn skipped(&mut self, index: usize, detail: impl Into<String>) {
        let detail = detail.into();
        let label = self.progress.steps[index].label.clone();
        self.progress
            .update(index, LaunchStepState::Skipped, detail.clone());
        self.push_log(RuntimeLogLevel::Info, format!("{label}: {detail}"));
    }

    pub fn fail_active(&mut self, detail: impl Into<String>) {
        let detail = detail.into();
        self.progress.fail_active(detail.clone());
        self.push_log(RuntimeLogLevel::Error, detail);
    }

    pub fn seed_connectivity(&mut self, status: ConnectivityStatus) {
        self.connectivity = Some(status);
    }

    pub async fn animate_while<F>(&mut self, mut future: Pin<Box<F>>) -> Result<F::Output>
    where
        F: Future + ?Sized,
    {
        self.draw_launch()?;
        let mut interval = tokio::time::interval(FRAME_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                output = future.as_mut() => {
                    self.draw_launch()?;
                    return Ok(output);
                }
                _ = interval.tick() => {
                    self.draw_launch()?;
                    self.tick = self.tick.wrapping_add(1);
                    self.consume_launch_events()?;
                }
            }
        }
    }

    pub async fn animate_briefly(&mut self, duration: Duration) -> Result<()> {
        self.animate_while(Box::pin(tokio::time::sleep(duration)))
            .await
    }

    pub fn dashboard_ready(&mut self, dashboard_url: String) -> Result<()> {
        let listener_detail = dashboard_display_origin(&dashboard_url);
        if let Some(index) = self
            .progress
            .steps
            .iter()
            .position(|step| step.state == LaunchStepState::Running)
        {
            self.complete(index, format!("Listening on {listener_detail}"));
        }
        self.dashboard_url = Some(dashboard_url);
        self.push_log(
            RuntimeLogLevel::Success,
            "Local dashboard is ready; the access link rotates on restart",
        );
        self.draw_status()
    }

    pub fn push_log(&mut self, level: RuntimeLogLevel, message: impl Into<String>) {
        let message = truncate_log_message(&message.into());
        if self.logs.len() == MAX_LOG_ENTRIES {
            self.logs.pop_front();
        }
        self.logs.push_back(RuntimeLog {
            timestamp: chrono::Local::now().format("%H:%M:%S").to_string(),
            level,
            message,
        });
    }

    pub fn observe_event(&mut self, event: &AppEvent) {
        match event {
            AppEvent::Connectivity(status) => {
                let changed = self.connectivity.as_ref().is_none_or(|previous| {
                    previous.kind != status.kind || previous.headline != status.headline
                });
                self.connectivity = Some(status.clone());
                if changed {
                    let level = match status.kind {
                        ConnectivityKind::Available => RuntimeLogLevel::Success,
                        ConnectivityKind::Checking => RuntimeLogLevel::Info,
                        ConnectivityKind::Degraded => RuntimeLogLevel::Warning,
                        ConnectivityKind::DnsFiltering
                        | ConnectivityKind::EndpointRestricted
                        | ConnectivityKind::Unreachable => RuntimeLogLevel::Error,
                    };
                    self.push_log(level, status.headline.clone());
                }
            }
            AppEvent::FeedStatus(status) => {
                let observation = FeedObservation {
                    connected: status.connected,
                    reconnects: status.reconnects,
                    status: status.status.clone(),
                    last_error: status.last_error.clone(),
                };
                let changed = self
                    .feeds
                    .get(&status.feed)
                    .is_none_or(|previous| previous != &observation);
                self.feeds.insert(status.feed, observation);
                if changed {
                    let level = if status.connected {
                        RuntimeLogLevel::Success
                    } else if status.status == "waiting" && status.reconnects == 0 {
                        RuntimeLogLevel::Info
                    } else {
                        RuntimeLogLevel::Warning
                    };
                    let detail = status.last_error.as_deref().unwrap_or(&status.status);
                    self.push_log(level, format!("{} feed: {detail}", status.feed.label()));
                }
            }
            AppEvent::Discovery(update) => {
                let discovered = update
                    .sessions
                    .iter()
                    .map(|session| session.slug.clone())
                    .collect::<Vec<_>>();
                if discovered != self.last_discovery {
                    self.last_discovery = discovered;
                    self.push_log(
                        RuntimeLogLevel::Info,
                        format!(
                            "Market discovery found {} session(s)",
                            update.sessions.len()
                        ),
                    );
                }
            }
            AppEvent::Trading(TradingEvent::Status { message, .. }) => {
                if self.last_trading_status.as_deref() != Some(message) {
                    self.last_trading_status = Some(message.clone());
                    self.push_log(RuntimeLogLevel::Info, format!("Trading: {message}"));
                }
            }
            AppEvent::Trading(TradingEvent::Error { message, .. }) => {
                self.push_log(RuntimeLogLevel::Error, format!("Trading: {message}"));
            }
            AppEvent::Trading(TradingEvent::OrderPlaced { order_id, side, .. }) => {
                self.push_log(
                    RuntimeLogLevel::Success,
                    format!("Order {order_id} placed ({side})"),
                );
            }
            AppEvent::Trading(TradingEvent::OrderCancelled {
                order_id, reason, ..
            }) => {
                self.push_log(
                    RuntimeLogLevel::Warning,
                    format!("Order {order_id} cancelled: {reason}"),
                );
            }
            AppEvent::Portfolio(portfolio) => {
                if portfolio.last_error != self.last_portfolio_error {
                    self.last_portfolio_error = portfolio.last_error.clone();
                    if let Some(error) = portfolio.last_error.as_deref() {
                        self.push_log(RuntimeLogLevel::Warning, format!("Portfolio: {error}"));
                    }
                }
            }
            AppEvent::Price(_)
            | AppEvent::Market(_)
            | AppEvent::MarketBatch(_)
            | AppEvent::Trading(_) => {}
        }
    }

    pub fn tick_status(&mut self) -> Result<RuntimeAction> {
        self.draw_status()?;
        self.tick = self.tick.wrapping_add(1);
        while event::poll(Duration::ZERO).context("failed to poll runtime TUI input")? {
            if let Event::Key(key) = event::read().context("failed to read runtime TUI input")?
                && is_actionable_key(key)
            {
                if is_runtime_detach_key(key) {
                    self.push_log(
                        RuntimeLogLevel::Info,
                        "Closing this view; PolyTread will continue in the background",
                    );
                    self.draw_status()?;
                    return Ok(RuntimeAction::Detach);
                }
                if is_copy_key(key) {
                    self.copy_dashboard_url();
                    self.draw_status()?;
                }
            }
        }
        self.advance_dashboard_notice();
        Ok(RuntimeAction::Continue)
    }

    fn copy_dashboard_url(&mut self) {
        let result = self
            .dashboard_url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("dashboard link is not ready"))
            .and_then(copy_text_to_clipboard);
        let (message, level) = match result {
            Ok(()) => (
                "Complete private URL copied to the clipboard".to_string(),
                RuntimeLogLevel::Success,
            ),
            Err(_) => (
                "Clipboard unavailable — press Esc to print the complete URL".to_string(),
                RuntimeLogLevel::Warning,
            ),
        };
        self.dashboard_notice = Some(DashboardNotice {
            message,
            level,
            frames_remaining: COPY_NOTICE_FRAMES,
        });
    }

    fn advance_dashboard_notice(&mut self) {
        let Some(notice) = self.dashboard_notice.as_mut() else {
            return;
        };
        notice.frames_remaining = notice.frames_remaining.saturating_sub(1);
        if notice.frames_remaining == 0 {
            self.dashboard_notice = None;
        }
    }

    pub fn show_failure(&mut self, error: &str) -> Result<()> {
        loop {
            self.terminal
                .draw(|frame| render_failure(frame, error))
                .context("failed to draw the PolyTread failure screen")?;
            if event::poll(FRAME_INTERVAL).context("failed to poll runtime TUI input")?
                && let Event::Key(key) =
                    event::read().context("failed to read runtime TUI input")?
                && is_actionable_key(key)
                && (is_cancel_key(key) || key.code == KeyCode::Enter)
            {
                return Ok(());
            }
            self.tick = self.tick.wrapping_add(1);
        }
    }

    fn draw_launch(&mut self) -> Result<()> {
        let tick = self.tick;
        let progress = &self.progress;
        self.terminal
            .draw(|frame| render_launch(frame, tick, progress))
            .context("failed to draw the PolyTread launch checks")?;
        Ok(())
    }

    fn draw_status(&mut self) -> Result<()> {
        let tick = self.tick;
        let dashboard_url = self
            .dashboard_url
            .as_deref()
            .unwrap_or("Starting local dashboard...");
        let health = self.health();
        let health_detail = self.health_detail(health);
        let logs = &self.logs;
        let dashboard_notice = self.dashboard_notice.as_ref();
        self.terminal
            .draw(|frame| {
                render_runtime(
                    frame,
                    tick,
                    dashboard_url,
                    dashboard_notice,
                    health,
                    &health_detail,
                    logs,
                )
            })
            .context("failed to draw the PolyTread runtime screen")?;
        Ok(())
    }

    fn consume_launch_events(&mut self) -> Result<()> {
        while event::poll(Duration::ZERO).context("failed to poll runtime TUI input")? {
            if let Event::Key(key) = event::read().context("failed to read runtime TUI input")?
                && is_actionable_key(key)
                && is_cancel_key(key)
            {
                return Err(RuntimeUiCancelled.into());
            }
        }
        Ok(())
    }

    fn health(&self) -> RuntimeHealth {
        let Some(connectivity) = self.connectivity.as_ref() else {
            return RuntimeHealth::Starting;
        };
        let connected_feeds = self.feeds.values().filter(|feed| feed.connected).count();
        let feed_failure = self
            .feeds
            .values()
            .any(|feed| !feed.connected && (feed.reconnects > 0 || feed.status != "waiting"));
        match connectivity.kind {
            ConnectivityKind::Checking => RuntimeHealth::Starting,
            ConnectivityKind::Available if feed_failure => RuntimeHealth::Degraded,
            ConnectivityKind::Available if connected_feeds >= 2 => RuntimeHealth::Active,
            ConnectivityKind::Available => RuntimeHealth::Starting,
            ConnectivityKind::Degraded => RuntimeHealth::Degraded,
            ConnectivityKind::DnsFiltering
            | ConnectivityKind::EndpointRestricted
            | ConnectivityKind::Unreachable => RuntimeHealth::Attention,
        }
    }

    fn health_detail(&self, health: RuntimeHealth) -> String {
        let connected = self.feeds.values().filter(|feed| feed.connected).count();
        let total = self.feeds.len().max(3);
        match health {
            RuntimeHealth::Starting => format!("Connecting feeds • {connected}/{total} ready"),
            RuntimeHealth::Active => format!("{connected}/{total} live feeds connected"),
            RuntimeHealth::Degraded => self
                .connectivity
                .as_ref()
                .map(|status| format!("{} • {connected}/{total} feeds connected", status.headline))
                .unwrap_or_else(|| "One or more live services are reconnecting".to_string()),
            RuntimeHealth::Attention => self
                .connectivity
                .as_ref()
                .map(|status| status.headline.clone())
                .unwrap_or_else(|| "PolyTread needs attention".to_string()),
        }
    }
}

impl Drop for RuntimeUi {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), Show, LeaveAlternateScreen);
        set_tui_terminal_active(false);
    }
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);
    set_tui_terminal_active(false);
}

fn is_actionable_key(key: KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

fn is_cancel_key(key: KeyEvent) -> bool {
    key.code == KeyCode::Esc
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
}

fn is_runtime_detach_key(key: KeyEvent) -> bool {
    is_cancel_key(key) || matches!(key.code, KeyCode::Char('q' | 'Q'))
}

fn is_copy_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('c' | 'C')) && !key.modifiers.contains(KeyModifiers::CONTROL)
}

fn copy_text_to_clipboard(text: &str) -> Result<()> {
    let mut clipboard =
        arboard::Clipboard::new().context("operating-system clipboard is unavailable")?;
    clipboard
        .set_text(text.to_string())
        .context("failed copying the dashboard link")
}

fn render_launch(frame: &mut Frame<'_>, tick: u64, progress: &LaunchProgress) {
    render_background(frame);
    if render_size_warning(frame) {
        return;
    }

    let card = centered_rect(frame.area(), 94, 26);
    let block = card_block(" RETURNING USER CHECKS ", ACCENT);
    let inner = inset(block.inner(card), 2, 1);
    frame.render_widget(block, card);
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Min(8),
        Constraint::Length(2),
    ])
    .split(inner);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "POLYTREAD",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  /  Secure launch", Style::default().fg(MUTED)),
            Span::styled(
                format!(
                    "    {:02}/{:02}",
                    progress.completed_count(),
                    progress.steps.len()
                ),
                Style::default().fg(TEXT),
            ),
        ])),
        chunks[0],
    );

    let gauge_area = Rect {
        y: chunks[1].y + 1,
        height: 1,
        ..chunks[1]
    };
    frame.render_widget(
        Gauge::default()
            .ratio(progress.ratio())
            .label(format!(
                "{} of {} checks complete",
                progress.completed_count(),
                progress.steps.len()
            ))
            .style(Style::default().fg(MUTED).bg(SURFACE_RAISED))
            .gauge_style(Style::default().fg(pulse_color(tick)).bg(SURFACE_RAISED))
            .use_unicode(true),
        gauge_area,
    );
    render_launch_steps(frame, chunks[2], tick, progress);
    render_fineprint(
        frame,
        chunks[3],
        "Startup checks running  •  Saved permissions skipped  •  Esc to stop",
    );
}

fn render_launch_steps(frame: &mut Frame<'_>, area: Rect, tick: u64, progress: &LaunchProgress) {
    if progress.steps.is_empty() || area.height == 0 {
        return;
    }
    let row_height = if area.height >= progress.steps.len() as u16 * 2 {
        2
    } else {
        1
    };
    for (index, step) in progress.steps.iter().enumerate() {
        let y = area.y.saturating_add(index as u16 * row_height);
        if y >= area.bottom() {
            break;
        }
        let (icon, icon_color, label_color) = match step.state {
            LaunchStepState::Pending => ("○", BORDER, MUTED),
            LaunchStepState::Running => (
                SPINNER_FRAMES[tick as usize % SPINNER_FRAMES.len()],
                pulse_color(tick),
                TEXT,
            ),
            LaunchStepState::Complete => ("✓", SUCCESS, TEXT),
            LaunchStepState::Warning => ("!", WARNING, TEXT),
            LaunchStepState::Skipped => ("↷", ACCENT, TEXT),
            LaunchStepState::Failed => ("×", DANGER, TEXT),
        };
        let row = Rect {
            x: area.x,
            y,
            width: area.width,
            height: 1,
        };
        let mut spans = vec![
            Span::styled(format!(" {icon}  "), Style::default().fg(icon_color)),
            Span::styled(
                &step.label,
                Style::default().fg(label_color).add_modifier(
                    if step.state == LaunchStepState::Running {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    },
                ),
            ),
        ];
        if row_height == 1
            && let Some(detail) = step.detail.as_deref()
        {
            spans.push(Span::styled("  ", Style::default()));
            spans.push(Span::styled(detail, Style::default().fg(MUTED)));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(if step.state == LaunchStepState::Running {
                Style::default().bg(SURFACE_RAISED)
            } else {
                Style::default()
            }),
            row,
        );
        if row_height == 2
            && let Some(detail) = step.detail.as_deref()
        {
            frame.render_widget(
                Paragraph::new(detail).style(Style::default().fg(MUTED)),
                Rect {
                    x: area.x.saturating_add(5),
                    y: y.saturating_add(1),
                    width: area.width.saturating_sub(5),
                    height: 1,
                },
            );
        }
    }
}

fn render_runtime(
    frame: &mut Frame<'_>,
    tick: u64,
    dashboard_url: &str,
    dashboard_notice: Option<&DashboardNotice>,
    health: RuntimeHealth,
    health_detail: &str,
    logs: &VecDeque<RuntimeLog>,
) {
    render_background(frame);
    if render_size_warning(frame) {
        return;
    }
    let card = centered_rect(frame.area(), MAX_CARD_WIDTH, 26);
    let block = card_block(" POLYTREAD RUNTIME ", ACCENT);
    let inner = inset(block.inner(card), 2, 1);
    frame.render_widget(block, card);
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(5),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Min(4),
        Constraint::Length(2),
    ])
    .split(inner);

    render_runtime_brand(frame, chunks[0]);
    render_dashboard_link(frame, chunks[1], dashboard_url, dashboard_notice);
    render_health(frame, chunks[3], tick, health, health_detail);
    render_logs(frame, chunks[5], logs);
    render_runtime_footer(frame, chunks[6]);
}

fn render_dashboard_link(
    frame: &mut Frame<'_>,
    area: Rect,
    dashboard_url: &str,
    dashboard_notice: Option<&DashboardNotice>,
) {
    let block = modal_block(" DASHBOARD ", ACCENT);
    let inner = inset(block.inner(area), 1, 0);
    frame.render_widget(block, area);
    let access_offset = dashboard_url
        .find("#access=")
        .unwrap_or(dashboard_url.len());
    let (address, private_access) = dashboard_url.split_at(access_offset);
    let (notice, notice_color) =
        dashboard_notice.map_or(("C  Copy complete private URL", MUTED), |notice| {
            let color = match notice.level {
                RuntimeLogLevel::Success => SUCCESS,
                RuntimeLogLevel::Warning => WARNING,
                RuntimeLogLevel::Error => DANGER,
                RuntimeLogLevel::Info => MUTED,
            };
            (notice.message.as_str(), color)
        });
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(vec![
                Span::styled(address, Style::default().fg(TEXT)),
                Span::styled(private_access, Style::default().fg(MUTED)),
            ]),
            Line::from(Span::styled(notice, Style::default().fg(notice_color))),
        ]))
        .wrap(Wrap { trim: false }),
        inner,
    );
}

fn render_health(
    frame: &mut Frame<'_>,
    area: Rect,
    tick: u64,
    health: RuntimeHealth,
    detail: &str,
) {
    let (label, bright, dim) = match health {
        RuntimeHealth::Starting => ("STARTING", ACCENT, Color::Rgb(128, 77, 0)),
        RuntimeHealth::Active => ("ACTIVE", SUCCESS, Color::Rgb(0, 112, 60)),
        RuntimeHealth::Degraded => ("DEGRADED", WARNING, Color::Rgb(120, 96, 0)),
        RuntimeHealth::Attention => ("ATTENTION", DANGER, Color::Rgb(128, 38, 48)),
    };
    let indicator_color = smooth_status_pulse(tick, dim, bright);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .style(Style::default().bg(SURFACE_RAISED));
    let inner = inset(block.inner(area), 1, 0);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("●  ", Style::default().fg(indicator_color)),
            Span::styled(
                label,
                Style::default().fg(bright).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("    {detail}"), Style::default().fg(MUTED)),
        ])),
        inner,
    );
}

fn render_logs(frame: &mut Frame<'_>, area: Rect, logs: &VecDeque<RuntimeLog>) {
    let block = modal_block(" ACTIVITY • LATEST 200 ", BORDER);
    let inner = inset(block.inner(area), 1, 0);
    frame.render_widget(block, area);
    let visible_rows = inner.height as usize;
    let visible = logs
        .iter()
        .rev()
        .take(visible_rows)
        .collect::<Vec<_>>()
        .into_iter()
        .rev();
    let lines = visible
        .map(|entry| {
            let (marker, color) = match entry.level {
                RuntimeLogLevel::Info => ("•", MUTED),
                RuntimeLogLevel::Success => ("✓", SUCCESS),
                RuntimeLogLevel::Warning => ("!", WARNING),
                RuntimeLogLevel::Error => ("×", DANGER),
            };
            Line::from(vec![
                Span::styled(format!("{}  ", entry.timestamp), Style::default().fg(MUTED)),
                Span::styled(format!("{marker}  "), Style::default().fg(color)),
                Span::styled(&entry.message, Style::default().fg(TEXT)),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_failure(frame: &mut Frame<'_>, error: &str) {
    render_background(frame);
    if render_size_warning(frame) {
        return;
    }
    let area = centered_rect(frame.area(), 88, 17);
    let block = card_block(" STARTUP STOPPED SAFELY ", DANGER);
    let inner = inset(block.inner(area), 2, 1);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    let chunks = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(2),
        Constraint::Min(4),
        Constraint::Length(2),
    ])
    .split(inner);
    render_brand(frame, chunks[0]);
    frame.render_widget(
        Paragraph::new("No permissions or system settings were changed.")
            .style(Style::default().fg(DANGER).add_modifier(Modifier::BOLD))
            .alignment(Alignment::Center),
        chunks[1],
    );
    frame.render_widget(
        Paragraph::new(error)
            .style(Style::default().fg(TEXT))
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        chunks[2],
    );
    render_fineprint(
        frame,
        chunks[3],
        "Enter: close  •  Diagnose network: `polytread diagnose`",
    );
}

fn render_background(frame: &mut Frame<'_>) {
    frame.render_widget(
        Block::default().style(Style::default().bg(BACKGROUND)),
        frame.area(),
    );
}

fn render_size_warning(frame: &mut Frame<'_>) -> bool {
    let area = frame.area();
    if area.width >= MIN_TERMINAL_WIDTH && area.height >= MIN_TERMINAL_HEIGHT {
        return false;
    }
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(Span::styled(
                "Terminal too small",
                Style::default().fg(WARNING).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!("Resize to at least {MIN_TERMINAL_WIDTH} × {MIN_TERMINAL_HEIGHT}."),
                Style::default().fg(MUTED),
            )),
        ]))
        .alignment(Alignment::Center)
        .block(card_block(" POLYTREAD ", WARNING)),
        centered_rect(area, 48, 7),
    );
    true
}

fn render_runtime_brand(frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "POLYTREAD",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  /  LOCAL • PRIVATE • MANUAL", Style::default().fg(MUTED)),
        ]))
        .alignment(Alignment::Center),
        area,
    );
}

fn render_runtime_footer(frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from("C: copy URL  •  Esc / Q / Ctrl+C: close this view"),
            Line::from("Service keeps running  •  Stop: polytread shutdown"),
        ]))
        .style(Style::default().fg(MUTED))
        .alignment(Alignment::Center),
        area,
    );
}

fn render_brand(frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(Span::styled(
                "P O L Y T R E A D",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "LOCAL • PRIVATE • MANUAL",
                Style::default().fg(MUTED),
            )),
        ]))
        .alignment(Alignment::Center),
        area,
    );
}

fn render_fineprint(frame: &mut Frame<'_>, area: Rect, text: &str) {
    frame.render_widget(
        Paragraph::new(text)
            .style(Style::default().fg(MUTED))
            .alignment(Alignment::Center),
        area,
    );
}

fn card_block(title: &str, color: Color) -> Block<'_> {
    Block::default()
        .title(Span::styled(
            title,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .style(Style::default().bg(SURFACE))
}

fn modal_block(title: &str, color: Color) -> Block<'_> {
    Block::default()
        .title(Span::styled(
            title,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
        .style(Style::default().bg(SURFACE_RAISED))
}

fn centered_rect(area: Rect, max_width: u16, preferred_height: u16) -> Rect {
    let width = area.width.saturating_sub(4).min(max_width).max(1);
    let height = area.height.saturating_sub(2).min(preferred_height).max(1);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn inset(area: Rect, horizontal: u16, vertical: u16) -> Rect {
    let horizontal = horizontal.min(area.width / 2);
    let vertical = vertical.min(area.height / 2);
    Rect {
        x: area.x + horizontal,
        y: area.y + vertical,
        width: area.width.saturating_sub(horizontal.saturating_mul(2)),
        height: area.height.saturating_sub(vertical.saturating_mul(2)),
    }
}

fn pulse_color(tick: u64) -> Color {
    const PULSE: [Color; 8] = [
        Color::Rgb(184, 110, 0),
        Color::Rgb(214, 128, 0),
        Color::Rgb(238, 143, 0),
        ACCENT,
        ACCENT,
        Color::Rgb(238, 143, 0),
        Color::Rgb(214, 128, 0),
        Color::Rgb(184, 110, 0),
    ];
    PULSE[tick as usize % PULSE.len()]
}

fn smooth_status_pulse(tick: u64, dim: Color, bright: Color) -> Color {
    let phase = (tick % STATUS_PULSE_FRAMES) as f64 / STATUS_PULSE_FRAMES as f64;
    let blend = (1.0 - (phase * std::f64::consts::TAU).cos()) / 2.0;
    blend_color(dim, bright, blend)
}

fn blend_color(from: Color, to: Color, amount: f64) -> Color {
    let (Color::Rgb(from_r, from_g, from_b), Color::Rgb(to_r, to_g, to_b)) = (from, to) else {
        return to;
    };
    let channel = |from: u8, to: u8| {
        (f64::from(from) + (f64::from(to) - f64::from(from)) * amount)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Color::Rgb(
        channel(from_r, to_r),
        channel(from_g, to_g),
        channel(from_b, to_b),
    )
}

fn truncate_log_message(message: &str) -> String {
    let mut chars = message.chars();
    let mut truncated = chars
        .by_ref()
        .take(MAX_LOG_MESSAGE_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        truncated.push('…');
    }
    truncated
}

fn dashboard_display_origin(url: &str) -> String {
    url.split(['?', '#']).next().unwrap_or(url).to_string()
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    use super::*;

    fn render_buffer(width: u16, height: u16, render: impl FnOnce(&mut Frame<'_>)) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal.draw(render).expect("render test frame");
        terminal.backend().buffer().clone()
    }

    fn buffer_text(buffer: &Buffer) -> String {
        let area = buffer.area;
        let mut output = String::new();
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                output.push_str(buffer[(x, y)].symbol());
            }
            output.push('\n');
        }
        output
    }

    fn foreground_for_symbol(buffer: &Buffer, symbol: &str) -> Option<Color> {
        let area = buffer.area;
        (area.y..area.bottom()).find_map(|y| {
            (area.x..area.right()).find_map(|x| {
                let cell = &buffer[(x, y)];
                (cell.symbol() == symbol).then_some(cell.fg)
            })
        })
    }

    fn progress() -> LaunchProgress {
        LaunchProgress::new([
            "Load saved configuration",
            "Open credential vault",
            "Verify Polymarket connectivity",
            "Authenticate and check funds",
            "Reuse saved permissions",
            "Start local dashboard",
        ])
    }

    fn logs() -> VecDeque<RuntimeLog> {
        [
            (RuntimeLogLevel::Success, "Saved configuration loaded"),
            (RuntimeLogLevel::Info, "Saved permission prompt skipped"),
            (RuntimeLogLevel::Warning, "Market feed is reconnecting"),
        ]
        .into_iter()
        .map(|(level, message)| RuntimeLog {
            timestamp: "12:34:56".to_string(),
            level,
            message: message.to_string(),
        })
        .collect()
    }

    #[test]
    fn skipped_permissions_count_as_a_completed_launch_check() {
        let mut progress = progress();
        progress.update(0, LaunchStepState::Complete, "Loaded".to_string());
        progress.update(
            4,
            LaunchStepState::Skipped,
            "Previously approved — prompt skipped".to_string(),
        );
        assert_eq!(progress.completed_count(), 2);
        assert!((progress.ratio() - (2.0 / 6.0)).abs() < f64::EPSILON);

        let rendered = buffer_text(&render_buffer(100, 30, |frame| {
            render_launch(frame, 2, &progress)
        }));
        assert!(rendered.contains("Reuse saved permissions"));
        assert!(rendered.contains("Previously approved — prompt skipped"));
        assert!(rendered.contains("Saved permissions skipped"));
    }

    #[test]
    fn runtime_page_shows_dashboard_link_status_and_bounded_log_notice() {
        let rendered = buffer_text(&render_buffer(110, 32, |frame| {
            render_runtime(
                frame,
                0,
                "http://127.0.0.1:9878/?run=abc#access=secret",
                None,
                RuntimeHealth::Active,
                "3/3 live feeds connected",
                &logs(),
            )
        }));
        assert!(rendered.contains("DASHBOARD"));
        assert!(rendered.contains("http://127.0.0.1:9878/?run=abc#access=secret"));
        assert!(rendered.contains("Copy complete private URL"));
        assert!(rendered.contains("ACTIVE"));
        assert!(rendered.contains("ACTIVITY • LATEST 200"));
        assert!(rendered.contains("Saved permission prompt skipped"));
        assert!(rendered.contains("close this view"));
        assert!(rendered.contains("Service keeps running"));
        assert!(rendered.contains("Stop: polytread shutdown"));
    }

    #[test]
    fn only_the_status_dot_pulses_and_the_box_stays_steady() {
        let frames = [0, 4, 8, 12, 16]
            .into_iter()
            .map(|tick| {
                render_buffer(60, 3, |frame| {
                    let area = frame.area();
                    render_health(frame, area, tick, RuntimeHealth::Active, "3/3 feeds")
                })
            })
            .collect::<Vec<_>>();
        let dot_colors = frames
            .iter()
            .map(|buffer| foreground_for_symbol(buffer, "●").expect("status dot"))
            .collect::<Vec<_>>();

        assert!(dot_colors.windows(2).all(|pair| pair[0] != pair[1]));
        assert_eq!(dot_colors.first().copied(), Some(Color::Rgb(0, 112, 60)));
        assert_eq!(dot_colors.last().copied(), Some(SUCCESS));
        assert!(frames.iter().all(|buffer| buffer[(0, 0)].fg == BORDER));
    }

    #[test]
    fn rolling_logs_drop_old_entries_and_bound_each_message() {
        let mut logs = VecDeque::new();
        for index in 0..=MAX_LOG_ENTRIES {
            if logs.len() == MAX_LOG_ENTRIES {
                logs.pop_front();
            }
            logs.push_back(RuntimeLog {
                timestamp: "00:00:00".to_string(),
                level: RuntimeLogLevel::Info,
                message: truncate_log_message(&format!("entry-{index}")),
            });
        }
        assert_eq!(logs.len(), MAX_LOG_ENTRIES);
        assert_eq!(
            logs.front().map(|entry| entry.message.as_str()),
            Some("entry-1")
        );

        let long = "x".repeat(MAX_LOG_MESSAGE_CHARS + 50);
        let bounded = truncate_log_message(&long);
        assert_eq!(bounded.chars().count(), MAX_LOG_MESSAGE_CHARS + 1);
        assert!(bounded.ends_with('…'));
    }

    #[test]
    fn runtime_palette_matches_first_run_black_and_orange_theme() {
        assert_eq!(BACKGROUND, Color::Rgb(0, 0, 0));
        assert_eq!(SURFACE, Color::Rgb(10, 10, 10));
        assert_eq!(SURFACE_RAISED, Color::Rgb(21, 21, 21));
        assert_eq!(BORDER, Color::Rgb(36, 36, 36));
        assert_eq!(TEXT, Color::Rgb(232, 232, 232));
        assert_eq!(MUTED, Color::Rgb(153, 153, 153));
        assert_eq!(ACCENT, Color::Rgb(255, 153, 0));
        assert_eq!(SUCCESS, Color::Rgb(0, 255, 136));
        assert_eq!(WARNING, Color::Rgb(255, 204, 0));
        assert_eq!(DANGER, Color::Rgb(255, 77, 95));
    }

    #[test]
    fn minimum_terminal_size_keeps_runtime_controls_visible() {
        let rendered = buffer_text(&render_buffer(80, 24, |frame| {
            render_runtime(
                frame,
                0,
                concat!(
                    "http://127.0.0.1:9878/?run=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "#access=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                ),
                None,
                RuntimeHealth::Starting,
                "Starting live services",
                &logs(),
            )
        }));
        assert!(rendered.contains("DASHBOARD"));
        assert!(rendered.contains("Copy complete private URL"));
        assert!(rendered.contains("STARTING"));
        assert!(rendered.contains("ACTIVITY"));
        assert!(rendered.contains("Ctrl+C: close this view"));
        assert!(rendered.contains("Service keeps running"));
    }

    #[test]
    fn undersized_terminal_renders_resize_guidance() {
        let rendered = buffer_text(&render_buffer(79, 23, |frame| {
            render_runtime(
                frame,
                0,
                "http://127.0.0.1:9878/",
                None,
                RuntimeHealth::Starting,
                "Starting",
                &logs(),
            )
        }));
        assert!(rendered.contains("Terminal too small"));
        assert!(rendered.contains("Resize to at least 80 × 24"));
    }

    #[test]
    fn dashboard_origin_hides_rotating_access_material_from_log_messages() {
        assert_eq!(
            dashboard_display_origin("http://127.0.0.1:9878/?run=abc#access=secret"),
            "http://127.0.0.1:9878/"
        );
    }

    #[test]
    fn runtime_keys_copy_or_detach_without_claiming_to_stop_the_service() {
        let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let quit = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let copy = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE);

        assert!(is_runtime_detach_key(escape));
        assert!(is_runtime_detach_key(quit));
        assert!(is_runtime_detach_key(ctrl_c));
        assert!(!is_runtime_detach_key(copy));
        assert!(is_copy_key(copy));
        assert!(!is_copy_key(ctrl_c));
    }
}

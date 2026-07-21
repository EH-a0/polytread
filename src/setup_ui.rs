use std::error::Error;
use std::fmt;
use std::future::Future;
use std::io::{self, Stdout, Write};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
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
use tracing_subscriber::fmt::MakeWriter;
use zeroize::{Zeroize, Zeroizing};

const FRAME_INTERVAL: Duration = Duration::from_millis(80);
const MIN_TERMINAL_WIDTH: u16 = 80;
const MIN_TERMINAL_HEIGHT: u16 = 24;
const MAX_CARD_WIDTH: u16 = 92;

// Keep setup visually aligned with the browser dashboard's black/orange palette.
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

static SETUP_TERMINAL_ACTIVE: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_tui_terminal_active(active: bool) {
    SETUP_TERMINAL_ACTIVE.store(active, Ordering::Release);
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TuiSafeStderr;

pub enum TuiSafeWriter {
    Stderr(io::Stderr),
    Sink(io::Sink),
}

impl<'writer> MakeWriter<'writer> for TuiSafeStderr {
    type Writer = TuiSafeWriter;

    fn make_writer(&'writer self) -> Self::Writer {
        if SETUP_TERMINAL_ACTIVE.load(Ordering::Acquire) {
            TuiSafeWriter::Sink(io::sink())
        } else {
            TuiSafeWriter::Stderr(io::stderr())
        }
    }
}

impl Write for TuiSafeWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Self::Stderr(stderr) => stderr.write(buffer),
            Self::Sink(sink) => sink.write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Stderr(stderr) => stderr.flush(),
            Self::Sink(sink) => sink.flush(),
        }
    }
}

#[derive(Debug)]
pub struct SetupCancelled;

impl fmt::Display for SetupCancelled {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("setup cancelled; no configuration was saved")
    }
}

impl Error for SetupCancelled {}

pub fn is_cancelled(error: &anyhow::Error) -> bool {
    error.downcast_ref::<SetupCancelled>().is_some()
}

#[derive(Debug)]
pub enum PrivateKeyAction {
    Submitted(Zeroizing<String>),
    Back,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    Pending,
    Running,
    Complete,
    Warning,
    AwaitingInput,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupStep {
    label: String,
    detail: Option<String>,
    state: StepState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupProgress {
    steps: Vec<SetupStep>,
}

impl SetupProgress {
    pub fn new(labels: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            steps: labels
                .into_iter()
                .map(|label| SetupStep {
                    label: label.into(),
                    detail: None,
                    state: StepState::Pending,
                })
                .collect(),
        }
    }

    pub fn running(&mut self, index: usize, detail: impl Into<String>) {
        self.update(index, StepState::Running, Some(detail.into()));
    }

    pub fn complete(&mut self, index: usize, detail: impl Into<String>) {
        self.update(index, StepState::Complete, Some(detail.into()));
    }

    pub fn warning(&mut self, index: usize, detail: impl Into<String>) {
        self.update(index, StepState::Warning, Some(detail.into()));
    }

    pub fn awaiting_input(&mut self, index: usize, detail: impl Into<String>) {
        self.update(index, StepState::AwaitingInput, Some(detail.into()));
    }

    pub fn fail_active(&mut self, detail: impl Into<String>) {
        let index = self
            .steps
            .iter()
            .position(|step| matches!(step.state, StepState::Running | StepState::AwaitingInput))
            .or_else(|| {
                self.steps
                    .iter()
                    .position(|step| step.state == StepState::Pending)
            });
        if let Some(index) = index {
            self.update(index, StepState::Failed, Some(detail.into()));
        }
    }

    fn update(&mut self, index: usize, state: StepState, detail: Option<String>) {
        let step = self
            .steps
            .get_mut(index)
            .expect("setup progress index is defined by a static setup step");
        step.state = state;
        step.detail = detail;
    }

    fn completed_count(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| matches!(step.state, StepState::Complete | StepState::Warning))
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

pub struct SetupUi {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    tick: u64,
}

impl SetupUi {
    pub fn enter() -> Result<Self> {
        enable_raw_mode().context("failed to enable terminal raw mode")?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, EnableBracketedPaste, Hide) {
            restore_terminal();
            return Err(error).context("failed to open the PolyTread setup screen");
        }
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(error) => {
                restore_terminal();
                return Err(error).context("failed to initialize the PolyTread setup screen");
            }
        };
        if let Err(error) = terminal.clear() {
            restore_terminal();
            return Err(error).context("failed to clear the PolyTread setup screen");
        }
        SETUP_TERMINAL_ACTIVE.store(true, Ordering::Release);
        Ok(Self { terminal, tick: 0 })
    }

    pub fn select_setup(&mut self) -> Result<()> {
        loop {
            let tick = self.tick;
            self.draw(|frame| render_menu(frame, tick))?;
            match poll_event(FRAME_INTERVAL)? {
                Some(Event::Key(key)) if is_actionable_key(key) => {
                    if is_cancel_key(key) {
                        return cancelled();
                    }
                    if key.code == KeyCode::Enter {
                        return Ok(());
                    }
                }
                _ => {}
            }
            self.tick = self.tick.wrapping_add(1);
        }
    }

    pub fn read_private_key(&mut self, initial_error: Option<&str>) -> Result<PrivateKeyAction> {
        let mut value = Zeroizing::new(String::with_capacity(70));
        let mut input_error = initial_error.map(str::to_owned);
        loop {
            let tick = self.tick;
            let secret_len = value.chars().count();
            self.draw(|frame| render_private_key(frame, tick, secret_len, input_error.as_deref()))?;
            match poll_event(FRAME_INTERVAL)? {
                Some(Event::Key(key)) if is_actionable_key(key) => {
                    if is_ctrl_c(key) {
                        return cancelled();
                    }
                    match key.code {
                        KeyCode::Esc => return Ok(PrivateKeyAction::Back),
                        KeyCode::Enter => {
                            if value.is_empty() {
                                input_error = Some("The private key cannot be empty.".to_string());
                            } else {
                                return Ok(PrivateKeyAction::Submitted(value));
                            }
                        }
                        KeyCode::Backspace => {
                            value.pop();
                            input_error = None;
                        }
                        KeyCode::Char(character)
                            if !key.modifiers.intersects(
                                KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                            ) && !character.is_whitespace()
                                && value.len() < 128 =>
                        {
                            value.push(character);
                            input_error = None;
                        }
                        _ => {}
                    }
                }
                Some(Event::Paste(mut pasted)) => {
                    for character in pasted.chars() {
                        if !character.is_whitespace()
                            && !character.is_control()
                            && value.len() < 128
                        {
                            value.push(character);
                        }
                    }
                    pasted.zeroize();
                    input_error = None;
                }
                _ => {}
            }
            self.tick = self.tick.wrapping_add(1);
        }
    }

    pub async fn animate_while<F>(
        &mut self,
        progress: &SetupProgress,
        future: Pin<Box<F>>,
    ) -> Result<F::Output>
    where
        F: Future + ?Sized,
    {
        self.animate(progress, future, true).await
    }

    pub async fn animate_while_locked<F>(
        &mut self,
        progress: &SetupProgress,
        future: Pin<Box<F>>,
    ) -> Result<F::Output>
    where
        F: Future + ?Sized,
    {
        self.animate(progress, future, false).await
    }

    async fn animate<F>(
        &mut self,
        progress: &SetupProgress,
        mut future: Pin<Box<F>>,
        cancellable: bool,
    ) -> Result<F::Output>
    where
        F: Future + ?Sized,
    {
        let mut interval = tokio::time::interval(FRAME_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                output = future.as_mut() => {
                    let tick = self.tick;
                    let panel = if cancellable {
                        ProgressPanel::None
                    } else {
                        ProgressPanel::Locked
                    };
                    self.draw(|frame| render_progress(frame, tick, progress, panel))?;
                    if !cancellable {
                        self.discard_progress_events()?;
                    }
                    return Ok(output);
                }
                _ = interval.tick() => {
                    let tick = self.tick;
                    let panel = if cancellable {
                        ProgressPanel::None
                    } else {
                        ProgressPanel::Locked
                    };
                    self.draw(|frame| render_progress(frame, tick, progress, panel))?;
                    self.tick = self.tick.wrapping_add(1);
                    if cancellable {
                        self.consume_progress_events()?;
                    } else {
                        self.discard_progress_events()?;
                    }
                }
            }
        }
    }

    pub async fn animate_briefly(
        &mut self,
        progress: &SetupProgress,
        duration: Duration,
    ) -> Result<()> {
        self.animate_while(progress, Box::pin(tokio::time::sleep(duration)))
            .await
    }

    pub fn prompt_funding_wallet(
        &mut self,
        progress: &SetupProgress,
        signer_address: &str,
        initial_error: Option<&str>,
    ) -> Result<String> {
        let mut value = String::with_capacity(42);
        let mut input_error = initial_error.map(str::to_owned);
        loop {
            let tick = self.tick;
            let panel = ProgressPanel::FundingWallet {
                value: &value,
                signer_address,
                error: input_error.as_deref(),
            };
            self.draw(|frame| render_progress(frame, tick, progress, panel))?;
            match poll_event(FRAME_INTERVAL)? {
                Some(Event::Key(key)) if is_actionable_key(key) => {
                    if is_cancel_key(key) {
                        return cancelled();
                    }
                    match key.code {
                        KeyCode::Enter => return Ok(value.trim().to_string()),
                        KeyCode::Backspace => {
                            value.pop();
                            input_error = None;
                        }
                        KeyCode::Char(character)
                            if !key.modifiers.intersects(
                                KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                            ) && !character.is_whitespace()
                                && value.len() < 64 =>
                        {
                            value.push(character);
                            input_error = None;
                        }
                        _ => {}
                    }
                }
                Some(Event::Paste(pasted)) => {
                    for character in pasted.chars() {
                        if !character.is_whitespace() && !character.is_control() && value.len() < 64
                        {
                            value.push(character);
                        }
                    }
                    input_error = None;
                }
                _ => {}
            }
            self.tick = self.tick.wrapping_add(1);
        }
    }

    pub fn prompt_wallet_type(&mut self, progress: &SetupProgress) -> Result<u8> {
        loop {
            let tick = self.tick;
            self.draw(|frame| render_progress(frame, tick, progress, ProgressPanel::WalletType))?;
            match poll_event(FRAME_INTERVAL)? {
                Some(Event::Key(key)) if is_actionable_key(key) => {
                    if is_cancel_key(key) {
                        return cancelled();
                    }
                    match key.code {
                        KeyCode::Char('1') => return Ok(1),
                        KeyCode::Char('2') => return Ok(2),
                        KeyCode::Char('3') => return Ok(3),
                        _ => {}
                    }
                }
                _ => {}
            }
            self.tick = self.tick.wrapping_add(1);
        }
    }

    pub fn confirm_dns_change(
        &mut self,
        progress: &SetupProgress,
        remediation: &str,
        detail: &str,
    ) -> Result<bool> {
        let mut value = String::with_capacity(3);
        let mut input_error = None;
        loop {
            let tick = self.tick;
            let panel = ProgressPanel::DnsConfirmation {
                remediation,
                detail,
                value: &value,
                error: input_error,
            };
            self.draw(|frame| render_progress(frame, tick, progress, panel))?;
            match poll_event(FRAME_INTERVAL)? {
                Some(Event::Key(key)) if is_actionable_key(key) => {
                    if is_cancel_key(key) {
                        return cancelled();
                    }
                    match key.code {
                        KeyCode::Enter if value.is_empty() => return Ok(false),
                        KeyCode::Enter if value == "YES" => return Ok(true),
                        KeyCode::Enter => {
                            input_error = Some("Type YES exactly, or clear the field to stop.");
                        }
                        KeyCode::Backspace => {
                            value.pop();
                            input_error = None;
                        }
                        KeyCode::Char(character)
                            if !key.modifiers.intersects(
                                KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                            ) && value.len() < 3 =>
                        {
                            value.push(character.to_ascii_uppercase());
                            input_error = None;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            self.tick = self.tick.wrapping_add(1);
        }
    }

    pub fn wait_for_required_enter(
        &mut self,
        progress: &SetupProgress,
        title: &str,
        detail: &str,
    ) -> Result<()> {
        loop {
            let tick = self.tick;
            let panel = ProgressPanel::LockedNotice { title, detail };
            self.draw(|frame| render_progress(frame, tick, progress, panel))?;
            match poll_event(FRAME_INTERVAL)? {
                Some(Event::Key(key)) if is_actionable_key(key) => {
                    if key.code == KeyCode::Enter {
                        return Ok(());
                    }
                }
                Some(Event::Paste(mut pasted)) => pasted.zeroize(),
                _ => {}
            }
            self.tick = self.tick.wrapping_add(1);
        }
    }

    pub fn confirm_browser_trading(&mut self, progress: &SetupProgress) -> Result<bool> {
        loop {
            let tick = self.tick;
            self.draw(|frame| {
                render_progress(frame, tick, progress, ProgressPanel::BrowserTrading)
            })?;
            match poll_event(FRAME_INTERVAL)? {
                Some(Event::Key(key)) if is_actionable_key(key) => {
                    if is_cancel_key(key) {
                        return cancelled();
                    }
                    match key.code {
                        KeyCode::Char('y' | 'Y') => return Ok(true),
                        KeyCode::Char('n' | 'N') => return Ok(false),
                        _ => {}
                    }
                }
                _ => {}
            }
            self.tick = self.tick.wrapping_add(1);
        }
    }

    pub fn show_complete(&mut self, progress: &SetupProgress, browser_trading: bool) -> Result<()> {
        loop {
            let tick = self.tick;
            let panel = ProgressPanel::Complete { browser_trading };
            self.draw(|frame| render_progress(frame, tick, progress, panel))?;
            match poll_event(FRAME_INTERVAL)? {
                Some(Event::Key(key))
                    if is_actionable_key(key)
                        && (is_ctrl_c(key)
                            || matches!(key.code, KeyCode::Enter | KeyCode::Esc)) =>
                {
                    return Ok(());
                }
                _ => {}
            }
            self.tick = self.tick.wrapping_add(1);
        }
    }

    pub fn show_failure(&mut self, progress: &SetupProgress, error: &str) -> Result<()> {
        loop {
            let tick = self.tick;
            let panel = ProgressPanel::Failure { error };
            self.draw(|frame| render_progress(frame, tick, progress, panel))?;
            match poll_event(FRAME_INTERVAL)? {
                Some(Event::Key(key))
                    if is_actionable_key(key)
                        && (is_ctrl_c(key)
                            || matches!(key.code, KeyCode::Enter | KeyCode::Esc)) =>
                {
                    return Ok(());
                }
                _ => {}
            }
            self.tick = self.tick.wrapping_add(1);
        }
    }

    fn consume_progress_events(&mut self) -> Result<()> {
        while event::poll(Duration::ZERO).context("failed to poll setup input")? {
            match event::read().context("failed to read setup input")? {
                Event::Key(key) if is_actionable_key(key) && is_cancel_key(key) => {
                    return cancelled();
                }
                Event::Paste(mut pasted) => pasted.zeroize(),
                _ => {}
            }
        }
        Ok(())
    }

    fn discard_progress_events(&mut self) -> Result<()> {
        while event::poll(Duration::ZERO).context("failed to poll setup input")? {
            if let Event::Paste(mut pasted) = event::read().context("failed to read setup input")? {
                pasted.zeroize();
            }
        }
        Ok(())
    }

    fn draw(&mut self, render: impl FnOnce(&mut Frame<'_>)) -> Result<()> {
        self.terminal
            .draw(render)
            .context("failed to draw the PolyTread setup screen")?;
        Ok(())
    }
}

impl Drop for SetupUi {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            DisableBracketedPaste,
            Show,
            LeaveAlternateScreen
        );
        SETUP_TERMINAL_ACTIVE.store(false, Ordering::Release);
    }
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        DisableBracketedPaste,
        Show,
        LeaveAlternateScreen
    );
}

fn poll_event(timeout: Duration) -> Result<Option<Event>> {
    if event::poll(timeout).context("failed to poll setup input")? {
        event::read()
            .map(Some)
            .context("failed to read setup input")
    } else {
        Ok(None)
    }
}

fn is_actionable_key(key: KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

fn is_ctrl_c(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn is_cancel_key(key: KeyEvent) -> bool {
    key.code == KeyCode::Esc || is_ctrl_c(key)
}

fn cancelled<T>() -> Result<T> {
    Err(SetupCancelled.into())
}

#[derive(Clone, Copy)]
enum ProgressPanel<'a> {
    None,
    Locked,
    FundingWallet {
        value: &'a str,
        signer_address: &'a str,
        error: Option<&'a str>,
    },
    WalletType,
    DnsConfirmation {
        remediation: &'a str,
        detail: &'a str,
        value: &'a str,
        error: Option<&'a str>,
    },
    LockedNotice {
        title: &'a str,
        detail: &'a str,
    },
    BrowserTrading,
    Complete {
        browser_trading: bool,
    },
    Failure {
        error: &'a str,
    },
}

fn render_menu(frame: &mut Frame<'_>, tick: u64) {
    render_background(frame);
    if render_size_warning(frame) {
        return;
    }

    let card = centered_rect(frame.area(), MAX_CARD_WIDTH.min(78), 22);
    let block = card_block(" FIRST-TIME SETUP ", ACCENT);
    let inner = inset(block.inner(card), 2, 1);
    frame.render_widget(block, card);

    let chunks = Layout::vertical([
        Constraint::Length(5),
        Constraint::Length(3),
        Constraint::Length(6),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .split(inner);

    render_brand(frame, chunks[0], tick);
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(Span::styled(
                "Welcome to PolyTread",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "Set up your local trading workspace securely.",
                Style::default().fg(MUTED),
            )),
        ]))
        .alignment(Alignment::Center),
        chunks[1],
    );

    let selection = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(pulse_color(tick)))
        .style(Style::default().bg(SURFACE_RAISED));
    let selection_inner = inset(selection.inner(chunks[2]), 2, 1);
    frame.render_widget(selection, chunks[2]);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("›  ", Style::default().fg(ACCENT)),
            Span::styled(
                "1. Setup configs",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("     SELECTED", Style::default().fg(ACCENT)),
        ]))
        .alignment(Alignment::Left),
        selection_inner,
    );

    render_fineprint(frame, chunks[4], "Press Enter to continue  •  Esc to exit");
}

fn render_private_key(
    frame: &mut Frame<'_>,
    tick: u64,
    secret_len: usize,
    input_error: Option<&str>,
) {
    render_background(frame);
    if render_size_warning(frame) {
        return;
    }

    let card = centered_rect(frame.area(), MAX_CARD_WIDTH.min(82), 17);
    let block = card_block(" SECURE CREDENTIAL ", ACCENT);
    let inner = inset(block.inner(card), 2, 1);
    frame.render_widget(block, card);
    let chunks = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(1),
        Constraint::Length(5),
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .split(inner);

    render_brand(frame, chunks[0], tick);
    let field = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if input_error.is_some() {
            DANGER
        } else {
            pulse_color(tick)
        }))
        .style(Style::default().bg(SURFACE_RAISED));
    let field_inner = inset(field.inner(chunks[2]), 2, 1);
    frame.render_widget(field, chunks[2]);
    let available_mask = field_inner.width.saturating_sub(32) as usize;
    let mut spans = vec![Span::styled(
        "Trading private key (hidden): ",
        Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
    )];
    spans.push(Span::styled(
        masked_secret(secret_len, available_mask.max(4)),
        Style::default().fg(ACCENT),
    ));
    if tick % 10 < 6 {
        spans.push(Span::styled("▏", Style::default().fg(ACCENT)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), field_inner);

    if let Some(input_error) = input_error {
        frame.render_widget(
            Paragraph::new(input_error)
                .style(Style::default().fg(DANGER))
                .alignment(Alignment::Center),
            chunks[3],
        );
    }
    render_fineprint(
        frame,
        chunks[5],
        "Press Enter to continue  •  Backspace to edit  •  Esc to go back",
    );
}

fn render_progress(
    frame: &mut Frame<'_>,
    tick: u64,
    progress: &SetupProgress,
    panel: ProgressPanel<'_>,
) {
    render_background(frame);
    if render_size_warning(frame) {
        return;
    }

    let preferred_height = (progress.steps.len() as u16)
        .saturating_mul(2)
        .saturating_add(12)
        .clamp(22, 30)
        .max(if matches!(panel, ProgressPanel::BrowserTrading) {
            30
        } else {
            0
        });
    let card = centered_rect(frame.area(), MAX_CARD_WIDTH, preferred_height);
    let block = card_block(" SECURE SETUP ", ACCENT);
    let inner = inset(block.inner(card), 2, 1);
    frame.render_widget(block, card);

    let bottom_height = if matches!(panel, ProgressPanel::BrowserTrading) {
        5
    } else {
        2
    };
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Min(8),
        Constraint::Length(bottom_height),
    ])
    .split(inner);
    render_progress_header(frame, chunks[0], progress);
    render_progress_gauge(frame, chunks[1], progress, tick);
    render_progress_steps(frame, chunks[2], progress, tick);
    if matches!(panel, ProgressPanel::BrowserTrading) {
        render_browser_trading_panel(frame, chunks[3], tick);
    } else {
        render_fineprint(
            frame,
            chunks[3],
            match panel {
                ProgressPanel::None => "Setup is running  •  Esc to cancel safely",
                ProgressPanel::Locked => {
                    "Finishing an approved system change  •  Please do not close PolyTread"
                }
                _ => "Follow the instruction above  •  Esc to cancel safely",
            },
        );
    }

    match panel {
        ProgressPanel::None | ProgressPanel::Locked => {}
        ProgressPanel::FundingWallet {
            value,
            signer_address,
            error,
        } => render_funding_panel(frame, tick, value, signer_address, error),
        ProgressPanel::WalletType => render_wallet_type_panel(frame),
        ProgressPanel::DnsConfirmation {
            remediation,
            detail,
            value,
            error,
        } => render_dns_panel(frame, tick, remediation, detail, value, error),
        ProgressPanel::LockedNotice { title, detail } => {
            render_notice_panel(frame, title, detail, WARNING, true)
        }
        ProgressPanel::BrowserTrading => {}
        ProgressPanel::Complete { browser_trading } => {
            render_complete_panel(frame, browser_trading)
        }
        ProgressPanel::Failure { error } => render_failure_panel(frame, error),
    }
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
    let warning = Paragraph::new(Text::from(vec![
        Line::from(Span::styled(
            "Terminal too small",
            Style::default().fg(WARNING).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("Resize to at least {MIN_TERMINAL_WIDTH} × {MIN_TERMINAL_HEIGHT}.",),
            Style::default().fg(MUTED),
        )),
    ]))
    .alignment(Alignment::Center)
    .block(card_block(" POLYTREAD ", WARNING));
    frame.render_widget(warning, centered_rect(area, 48, 7));
    true
}

fn render_brand(frame: &mut Frame<'_>, area: Rect, tick: u64) {
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(Span::styled(
                "P O L Y T R E A D",
                Style::default()
                    .fg(pulse_color(tick))
                    .add_modifier(Modifier::BOLD),
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

fn render_progress_header(frame: &mut Frame<'_>, area: Rect, progress: &SetupProgress) {
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "POLYTREAD",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  /  First-time configuration", Style::default().fg(MUTED)),
            Span::styled(
                format!(
                    "    {:02}/{:02}",
                    progress.completed_count(),
                    progress.steps.len()
                ),
                Style::default().fg(TEXT),
            ),
        ])),
        area,
    );
}

fn render_progress_gauge(frame: &mut Frame<'_>, area: Rect, progress: &SetupProgress, tick: u64) {
    let gauge_area = Rect {
        y: area.y + 1,
        height: 1,
        ..area
    };
    let gauge = Gauge::default()
        .ratio(progress.ratio())
        .label(format!(
            "{} of {} steps complete",
            progress.completed_count(),
            progress.steps.len()
        ))
        .style(Style::default().fg(MUTED).bg(SURFACE_RAISED))
        .gauge_style(Style::default().fg(pulse_color(tick)).bg(SURFACE_RAISED))
        .use_unicode(true);
    frame.render_widget(gauge, gauge_area);
}

fn render_progress_steps(frame: &mut Frame<'_>, area: Rect, progress: &SetupProgress, tick: u64) {
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
        let row = Rect {
            x: area.x,
            y,
            width: area.width,
            height: 1,
        };
        let (icon, icon_color, label_color) = match step.state {
            StepState::Pending => ("○", BORDER, MUTED),
            StepState::Running => (
                SPINNER_FRAMES[tick as usize % SPINNER_FRAMES.len()],
                pulse_color(tick),
                TEXT,
            ),
            StepState::Complete => ("✓", SUCCESS, TEXT),
            StepState::Warning => ("!", WARNING, TEXT),
            StepState::AwaitingInput => ("?", ACCENT, TEXT),
            StepState::Failed => ("×", DANGER, TEXT),
        };
        let mut spans = vec![
            Span::styled(format!(" {icon}  "), Style::default().fg(icon_color)),
            Span::styled(
                &step.label,
                Style::default().fg(label_color).add_modifier(
                    if matches!(step.state, StepState::Running | StepState::AwaitingInput) {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    },
                ),
            ),
        ];
        if row_height == 1
            && let Some(detail) = &step.detail
        {
            spans.push(Span::styled("  ", Style::default()));
            spans.push(Span::styled(detail, Style::default().fg(MUTED)));
        }
        let style = if matches!(step.state, StepState::Running | StepState::AwaitingInput) {
            Style::default().bg(SURFACE_RAISED)
        } else {
            Style::default()
        };
        frame.render_widget(Paragraph::new(Line::from(spans)).style(style), row);
        if row_height == 2
            && let Some(detail) = &step.detail
        {
            frame.render_widget(
                Paragraph::new(detail.as_str()).style(Style::default().fg(MUTED)),
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

fn render_funding_panel(
    frame: &mut Frame<'_>,
    tick: u64,
    value: &str,
    signer_address: &str,
    error: Option<&str>,
) {
    let area = modal_rect(frame.area(), 80, 10);
    let block = modal_block(
        " FUNDING WALLET ",
        if error.is_some() { DANGER } else { ACCENT },
    );
    let inner = inset(block.inner(area), 2, 1);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(2),
        Constraint::Length(2),
        Constraint::Min(1),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new("Profile discovery was inconclusive. Confirm the public funding address.")
            .style(Style::default().fg(TEXT))
            .wrap(Wrap { trim: true }),
        chunks[0],
    );
    let cursor = if tick % 10 < 6 { "▏" } else { "" };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Funding wallet: ", Style::default().fg(MUTED)),
            Span::styled(value, Style::default().fg(ACCENT)),
            Span::styled(cursor, Style::default().fg(ACCENT)),
        ])),
        chunks[1],
    );
    let hint = error
        .map(str::to_owned)
        .unwrap_or_else(|| format!("Press Enter to use the signer: {signer_address}"));
    frame.render_widget(
        Paragraph::new(hint)
            .style(Style::default().fg(if error.is_some() { DANGER } else { MUTED }))
            .wrap(Wrap { trim: true }),
        chunks[2],
    );
    render_fineprint(
        frame,
        chunks[3],
        "Press Enter to continue  •  Backspace to edit  •  Esc to exit",
    );
}

fn render_wallet_type_panel(frame: &mut Frame<'_>) {
    let area = modal_rect(frame.area(), 72, 12);
    let block = modal_block(" WALLET TYPE ", ACCENT);
    let inner = inset(block.inner(area), 2, 1);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    let content = Text::from(vec![
        Line::from(Span::styled(
            "Automatic detection was inconclusive. Choose the type shown by Polymarket:",
            Style::default().fg(TEXT),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("[1] ", Style::default().fg(ACCENT)),
            Span::styled("Legacy proxy", Style::default().fg(TEXT)),
        ]),
        Line::from(vec![
            Span::styled("[2] ", Style::default().fg(ACCENT)),
            Span::styled("Gnosis Safe", Style::default().fg(TEXT)),
        ]),
        Line::from(vec![
            Span::styled("[3] ", Style::default().fg(ACCENT)),
            Span::styled("Deposit wallet (POLY_1271)", Style::default().fg(TEXT)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "Press 1, 2, or 3 to continue  •  Esc to exit",
            Style::default().fg(MUTED),
        )),
    ]);
    frame.render_widget(Paragraph::new(content).wrap(Wrap { trim: true }), inner);
}

fn render_dns_panel(
    frame: &mut Frame<'_>,
    tick: u64,
    remediation: &str,
    detail: &str,
    value: &str,
    error: Option<&str>,
) {
    let area = modal_rect(frame.area(), 84, 22);
    let block = modal_block(" CONNECTIVITY REMEDIATION ", WARNING);
    let inner = inset(block.inner(area), 2, 1);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(8),
        Constraint::Length(2),
        Constraint::Length(2),
        Constraint::Length(2),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(format!("PolyTread can try {remediation}."))
            .style(Style::default().fg(TEXT))
            .wrap(Wrap { trim: true }),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(detail)
            .style(Style::default().fg(MUTED))
            .wrap(Wrap { trim: true }),
        chunks[1],
    );
    let cursor = if tick % 10 < 6 { "▏" } else { "" };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Type YES to approve: ", Style::default().fg(TEXT)),
            Span::styled(value, Style::default().fg(WARNING)),
            Span::styled(cursor, Style::default().fg(WARNING)),
        ])),
        chunks[2],
    );
    frame.render_widget(
        Paragraph::new(error.unwrap_or("Press Enter on an empty field to stop without changes."))
            .style(Style::default().fg(if error.is_some() { DANGER } else { MUTED })),
        chunks[3],
    );
    render_fineprint(frame, chunks[4], "Press Enter to continue  •  Esc to exit");
}

fn render_notice_panel(
    frame: &mut Frame<'_>,
    title: &str,
    detail: &str,
    color: Color,
    locked: bool,
) {
    let area = modal_rect(frame.area(), 80, 10);
    let panel_title = format!(" {title} ");
    let block = modal_block(&panel_title, color);
    let inner = inset(block.inner(area), 2, 1);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    let chunks = Layout::vertical([Constraint::Min(3), Constraint::Length(2)]).split(inner);
    frame.render_widget(
        Paragraph::new(detail)
            .style(Style::default().fg(TEXT))
            .wrap(Wrap { trim: true }),
        chunks[0],
    );
    render_fineprint(
        frame,
        chunks[1],
        if locked {
            "Press Enter to continue  •  Finish this system step before closing"
        } else {
            "Press Enter to continue  •  Esc to exit"
        },
    );
}

fn render_browser_trading_panel(frame: &mut Frame<'_>, area: Rect, tick: u64) {
    let block = modal_block(" FINAL SAFETY CHOICE ", pulse_color(tick));
    let inner = inset(block.inner(area), 2, 0);
    frame.render_widget(block, area);
    let text = Text::from(vec![
        Line::from(vec![
            Span::styled(
                "Enable manual browser trading?  ",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("Dashboard starts disarmed.", Style::default().fg(MUTED)),
        ]),
        Line::from(vec![
            Span::styled(
                "[Y] YES",
                Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  Enable manual orders     ", Style::default().fg(TEXT)),
            Span::styled(
                "[N] NO ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  Keep view-only", Style::default().fg(TEXT)),
        ]),
        Line::from(Span::styled(
            "Press Y for Yes or N for No  •  Esc to exit",
            Style::default().fg(MUTED),
        )),
    ]);
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), inner);
}

fn render_complete_panel(frame: &mut Frame<'_>, browser_trading: bool) {
    let area = modal_rect(frame.area(), 78, 11);
    let block = modal_block(" SETUP COMPLETE ", SUCCESS);
    let inner = inset(block.inner(area), 2, 1);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    let mode = if browser_trading {
        "Enabled — the dashboard will still start disarmed"
    } else {
        "View-only"
    };
    let text = Text::from(vec![
        Line::from(Span::styled(
            "✓  PolyTread is configured securely",
            Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("Browser trading: ", Style::default().fg(MUTED)),
            Span::styled(mode, Style::default().fg(TEXT)),
        ]),
        Line::from(Span::styled(
            "Secrets are stored in your operating-system credential vault.",
            Style::default().fg(MUTED),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Press Enter to continue",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
    ]);
    frame.render_widget(
        Paragraph::new(text)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        inner,
    );
}

fn render_failure_panel(frame: &mut Frame<'_>, error: &str) {
    let area = modal_rect(frame.area(), 84, 13);
    let block = modal_block(" SETUP STOPPED SAFELY ", DANGER);
    let inner = inset(block.inner(area), 2, 1);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(4),
        Constraint::Length(2),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new("No local configuration was completed.")
            .style(Style::default().fg(DANGER).add_modifier(Modifier::BOLD))
            .alignment(Alignment::Center),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(error)
            .style(Style::default().fg(TEXT))
            .wrap(Wrap { trim: true }),
        chunks[1],
    );
    render_fineprint(
        frame,
        chunks[2],
        "Press Enter to return to the terminal  •  Esc to close",
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

fn modal_rect(area: Rect, max_width: u16, preferred_height: u16) -> Rect {
    centered_rect(area, max_width, preferred_height)
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

fn masked_secret(secret_len: usize, available: usize) -> String {
    if secret_len == 0 {
        return String::new();
    }
    if secret_len <= available {
        return "•".repeat(secret_len);
    }
    let shown = available.saturating_sub(1);
    format!("{}…", "•".repeat(shown))
}

#[cfg(test)]
mod tests {
    use std::mem::{size_of, size_of_val};

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    use super::*;

    fn render_text(render: impl FnOnce(&mut Frame<'_>)) -> String {
        buffer_text(&render_buffer(100, 32, render))
    }

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

    fn buffer_has_foreground(buffer: &Buffer, color: Color) -> bool {
        let area = buffer.area;
        (area.y..area.bottom()).any(|y| {
            (area.x..area.right()).any(|x| {
                let cell = &buffer[(x, y)];
                cell.fg == color && cell.symbol() != " "
            })
        })
    }

    fn buffer_has_background(buffer: &Buffer, color: Color) -> bool {
        let area = buffer.area;
        (area.y..area.bottom()).any(|y| {
            (area.x..area.right()).any(|x| {
                let cell = &buffer[(x, y)];
                cell.bg == color
            })
        })
    }

    fn setup_progress() -> SetupProgress {
        SetupProgress::new([
            "Derived signer",
            "Verify Polymarket connectivity",
            "Discover funding wallet",
            "Detect wallet type",
            "Authenticate and check funds",
            "Browser trading",
        ])
    }

    #[test]
    fn menu_has_one_selected_setup_action_and_enter_instruction() {
        let rendered = render_text(|frame| render_menu(frame, 0));
        assert!(rendered.contains("1. Setup configs"));
        assert!(rendered.contains("SELECTED"));
        assert!(rendered.contains("Press Enter to continue"));
    }

    #[test]
    fn private_key_page_never_renders_the_secret() {
        let secret = "0x1234567890abcdef";
        let rendered = render_text(|frame| render_private_key(frame, 0, secret.len(), None));
        assert!(rendered.contains("Trading private key (hidden):"));
        assert!(rendered.contains('•'));
        assert!(!rendered.contains(secret));
        assert!(rendered.contains("Press Enter to continue"));
    }

    #[test]
    fn private_key_page_renders_empty_input_validation() {
        let rendered = render_text(|frame| {
            render_private_key(frame, 0, 0, Some("The private key cannot be empty."))
        });
        assert!(rendered.contains("The private key cannot be empty."));
        assert!(rendered.contains("Esc to go back"));
    }

    #[test]
    fn progress_model_counts_warnings_as_completed_checks() {
        let mut progress = SetupProgress::new(["One", "Two", "Three"]);
        progress.complete(0, "done");
        progress.warning(1, "usable with warning");
        progress.running(2, "working");
        assert_eq!(progress.completed_count(), 2);
        assert!((progress.ratio() - (2.0 / 3.0)).abs() < f64::EPSILON);
    }

    #[test]
    fn progress_page_renders_step_results_and_browser_shortcuts() {
        let mut progress = setup_progress();
        progress.complete(0, "0x99d5e7Da1aB7930Cc08CC63460259c8f7d85c81D");
        progress.complete(1, "Endpoints reachable");
        progress.complete(2, "0x1111111111111111111111111111111111111111");
        progress.complete(3, "Gnosis Safe");
        progress.complete(4, "pUSD $10.0000 • std $10.0000 • neg-risk $10.0000");
        progress.awaiting_input(5, "Choose Y or N");
        let rendered = render_text(|frame| {
            render_progress(frame, 2, &progress, ProgressPanel::BrowserTrading)
        });
        assert!(rendered.contains("Derived signer"));
        assert!(rendered.contains("0x99d5e7Da1aB7930Cc08CC63460259c8f7d85c81D"));
        assert!(rendered.contains("Verify Polymarket connectivity"));
        assert!(rendered.contains("Discover funding wallet"));
        assert!(rendered.contains("Detect wallet type"));
        assert!(rendered.contains("Authenticate and check funds"));
        assert!(rendered.contains("Browser trading"));
        assert!(rendered.contains("[Y] YES"));
        assert!(rendered.contains("[N] NO"));
        assert!(rendered.contains("Press Y for Yes or N for No"));
    }

    #[test]
    fn running_and_locked_progress_states_render_their_safety_instructions() {
        let mut progress = setup_progress();
        progress.running(0, "Deriving the Polygon signing address...");

        let running =
            render_text(|frame| render_progress(frame, 3, &progress, ProgressPanel::None));
        assert!(running.contains("Deriving the Polygon signing address"));
        assert!(running.contains("Setup is running"));
        assert!(running.contains("Esc to cancel safely"));

        let locked =
            render_text(|frame| render_progress(frame, 3, &progress, ProgressPanel::Locked));
        assert!(locked.contains("Finishing an approved system change"));
        assert!(locked.contains("Please do not close PolyTread"));
    }

    #[test]
    fn funding_wallet_and_wallet_type_fallbacks_render_completely() {
        let mut progress = setup_progress();
        progress.complete(0, "0x9999999999999999999999999999999999999999");
        progress.complete(1, "Endpoints reachable");
        progress.awaiting_input(2, "Confirm the funding wallet");

        let funding = render_text(|frame| {
            render_progress(
                frame,
                1,
                &progress,
                ProgressPanel::FundingWallet {
                    value: "0x1111",
                    signer_address: "0x9999999999999999999999999999999999999999",
                    error: Some("Enter a valid 0x funding-wallet address."),
                },
            )
        });
        assert!(funding.contains("FUNDING WALLET"));
        assert!(funding.contains("Funding wallet: 0x1111"));
        assert!(funding.contains("Enter a valid 0x funding-wallet address."));
        assert!(funding.contains("Press Enter to continue"));

        progress.complete(2, "0x9999999999999999999999999999999999999999");
        progress.awaiting_input(3, "Choose the wallet type");
        let wallet_type =
            render_text(|frame| render_progress(frame, 1, &progress, ProgressPanel::WalletType));
        assert!(wallet_type.contains("WALLET TYPE"));
        assert!(wallet_type.contains("[1] Legacy proxy"));
        assert!(wallet_type.contains("[2] Gnosis Safe"));
        assert!(wallet_type.contains("[3] Deposit wallet"));
        assert!(wallet_type.contains("Press 1, 2, or 3 to continue"));
    }

    #[test]
    fn dns_confirmation_and_required_system_step_render_completely() {
        let mut progress = setup_progress();
        progress.complete(0, "Signer ready");
        progress.awaiting_input(1, "DNS resolution is unavailable");

        let dns = render_text(|frame| {
            render_progress(
                frame,
                4,
                &progress,
                ProgressPanel::DnsConfirmation {
                    remediation: "a temporary DNS resolver change",
                    detail: "The original DNS settings are saved for rollback.",
                    value: "YE",
                    error: Some("Type YES exactly, or clear the field to stop."),
                },
            )
        });
        assert!(dns.contains("CONNECTIVITY REMEDIATION"));
        assert!(dns.contains("temporary DNS resolver change"));
        assert!(dns.contains("Type YES exactly"));
        assert!(dns.contains("Press Enter to continue"));

        let required_step = render_text(|frame| {
            render_progress(
                frame,
                4,
                &progress,
                ProgressPanel::LockedNotice {
                    title: "OPERATING-SYSTEM STEP",
                    detail: "Approve the network change in the system dialog.",
                },
            )
        });
        assert!(required_step.contains("OPERATING-SYSTEM STEP"));
        assert!(required_step.contains("Approve the network change"));
        assert!(!required_step.contains("SApprove"));
        assert!(required_step.contains("Press Enter to continue"));
        assert!(
            required_step.contains("before closing"),
            "required-step screen:\n{required_step}"
        );
    }

    #[test]
    fn both_completion_modes_and_failure_state_render_completely() {
        let mut progress = setup_progress();
        for index in 0..6 {
            progress.complete(index, "Complete");
        }

        let enabled = render_text(|frame| {
            render_progress(
                frame,
                0,
                &progress,
                ProgressPanel::Complete {
                    browser_trading: true,
                },
            )
        });
        assert!(enabled.contains("SETUP COMPLETE"));
        assert!(enabled.contains("Enabled"));
        assert!(
            enabled.contains("start disarmed"),
            "enabled completion screen:\n{enabled}"
        );
        assert!(!enabled.contains("│erived signer"));
        assert!(enabled.contains("Press Enter to continue"));

        let view_only = render_text(|frame| {
            render_progress(
                frame,
                0,
                &progress,
                ProgressPanel::Complete {
                    browser_trading: false,
                },
            )
        });
        assert!(view_only.contains("View-only"));

        let failure = render_text(|frame| {
            render_progress(
                frame,
                0,
                &progress,
                ProgressPanel::Failure {
                    error: "The endpoint check failed before credentials were saved.",
                },
            )
        });
        assert!(failure.contains("SETUP STOPPED SAFELY"));
        assert!(failure.contains("No local configuration was completed"));
        assert!(failure.contains("endpoint check failed"));
        assert!(failure.contains("Press Enter to return to the terminal"));
    }

    #[test]
    fn undersized_terminal_renders_resize_guidance() {
        let rendered = buffer_text(&render_buffer(79, 23, |frame| render_menu(frame, 0)));
        assert!(rendered.contains("Terminal too small"));
        assert!(rendered.contains("Resize to at least 80 × 24"));
    }

    #[test]
    fn minimum_supported_terminal_keeps_longest_setup_instructions_visible() {
        let private_key = buffer_text(&render_buffer(80, 24, |frame| {
            render_private_key(frame, 0, 64, None)
        }));
        assert!(private_key.contains("Trading private key (hidden):"));
        assert!(private_key.contains("Press Enter to continue"));

        let mut progress = setup_progress();
        progress.complete(0, "0x9999999999999999999999999999999999999999");
        progress.awaiting_input(1, "DNS or ISP filtering detected");
        let diagnostic = concat!(
            "The system resolver differs from encrypted DNS, and a real CLOB request succeeds ",
            "through the encrypted-DNS destination. Browser Secure DNS may still work while ",
            "terminal applications fail. CLOB REST failed: connection timed out. Market REST ",
            "failed: connection timed out. Market WebSocket timed out. This changes DNS ",
            "resolution only, not your public IP or trading eligibility. PolyTread keeps a local ",
            "rollback record for `polytread restore-dns`."
        );
        let dns = buffer_text(&render_buffer(80, 24, |frame| {
            render_progress(
                frame,
                0,
                &progress,
                ProgressPanel::DnsConfirmation {
                    remediation: "Windows encrypted DNS on the active network adapter",
                    detail: diagnostic,
                    value: "",
                    error: None,
                },
            )
        }));
        assert!(dns.contains("CONNECTIVITY REMEDIATION"));
        assert!(dns.contains("polytread restore-dns"));
        assert!(dns.contains("Press Enter to continue"));

        for index in 1..5 {
            progress.complete(index, "Complete");
        }
        progress.awaiting_input(5, "Choose Y to enable or N to stay view-only");
        let browser = buffer_text(&render_buffer(80, 24, |frame| {
            render_progress(frame, 0, &progress, ProgressPanel::BrowserTrading)
        }));
        assert!(browser.contains("Browser trading"));
        assert!(browser.contains("Dashboard starts disarmed"));
        assert!(browser.contains("Press Y for Yes or N for No"));

        progress.complete(5, "View-only");
        let complete = buffer_text(&render_buffer(80, 24, |frame| {
            render_progress(
                frame,
                0,
                &progress,
                ProgressPanel::Complete {
                    browser_trading: false,
                },
            )
        }));
        assert!(complete.contains("SETUP COMPLETE"));
        assert!(complete.contains("Press Enter to continue"));
    }

    #[test]
    fn setup_palette_matches_the_dashboard_black_orange_theme() {
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

        let rendered = render_buffer(100, 32, |frame| render_menu(frame, 3));
        assert!(buffer_has_background(&rendered, BACKGROUND));
        assert!(buffer_has_background(&rendered, SURFACE));
        assert!(buffer_has_background(&rendered, SURFACE_RAISED));
        assert!(buffer_has_foreground(&rendered, ACCENT));
        assert!(!buffer_has_foreground(&rendered, Color::Rgb(67, 211, 255)));
    }

    #[test]
    fn setup_operation_handle_is_pointer_sized_before_animation() {
        let operation = Box::pin(async {
            let dependency_state = [0_u8; 64 * 1024];
            std::future::pending::<()>().await;
            dependency_state.len()
        });
        assert!(size_of_val(&operation) <= size_of::<usize>() * 2);
    }

    #[test]
    fn long_secret_masks_are_bounded_without_exposing_content() {
        assert_eq!(masked_secret(3, 8), "•••");
        assert_eq!(masked_secret(9, 5), "••••…");
    }
}

#[cfg(test)]
#[path = "setup_ui_gallery.rs"]
mod gallery;

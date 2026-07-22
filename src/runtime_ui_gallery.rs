use std::env;
use std::fs;
use std::path::Path;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use serde::Serialize;

use super::*;

const PREVIEW_WIDTH: u16 = 100;
const PREVIEW_HEIGHT: u16 = 32;
const EXAMPLE_DASHBOARD_URL: &str =
    "http://127.0.0.1:9878/?run=example#access=example-private-token";

struct GalleryShot {
    slug: &'static str,
    category: &'static str,
    title: &'static str,
    description: &'static str,
    buffer: Buffer,
}

#[derive(Serialize)]
struct GalleryExport {
    source: &'static str,
    note: &'static str,
    shots: Vec<ExportShot>,
}

#[derive(Serialize)]
struct ExportShot {
    number: usize,
    slug: &'static str,
    category: &'static str,
    title: &'static str,
    description: &'static str,
    width: u16,
    height: u16,
    cells: Vec<ExportCell>,
}

#[derive(Serialize)]
struct ExportCell {
    symbol: String,
    foreground: String,
    background: String,
    bold: bool,
}

#[test]
#[ignore = "writes the complete runtime-state gallery requested for documentation"]
fn export_complete_runtime_state_gallery() {
    let output_dir = env::var_os("POLYTREAD_RUNTIME_GALLERY_DIR")
        .expect("set POLYTREAD_RUNTIME_GALLERY_DIR to an output directory");
    let output_dir = Path::new(&output_dir);
    fs::create_dir_all(output_dir).expect("create runtime gallery output directory");

    let export = GalleryExport {
        source: "PolyTread Ratatui TestBackend",
        note: "Every image is rendered by the production runtime renderer. Addresses, access material, balances, and log entries are synthetic examples.",
        shots: gallery_shots()
            .into_iter()
            .enumerate()
            .map(|(index, shot)| export_shot(index + 1, shot))
            .collect(),
    };
    let json = serde_json::to_vec_pretty(&export).expect("serialize runtime gallery");
    fs::write(output_dir.join("runtime-gallery.json"), json).expect("write runtime gallery export");
}

fn export_shot(number: usize, shot: GalleryShot) -> ExportShot {
    let area = shot.buffer.area;
    let mut cells = Vec::with_capacity(usize::from(area.width) * usize::from(area.height));
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            let cell = &shot.buffer[(x, y)];
            cells.push(ExportCell {
                symbol: if cell.symbol().is_empty() {
                    " ".to_string()
                } else {
                    cell.symbol().to_string()
                },
                foreground: foreground_hex(cell.fg),
                background: background_hex(cell.bg),
                bold: cell.modifier.contains(Modifier::BOLD),
            });
        }
    }
    ExportShot {
        number,
        slug: shot.slug,
        category: shot.category,
        title: shot.title,
        description: shot.description,
        width: area.width,
        height: area.height,
        cells,
    }
}

fn gallery_shots() -> Vec<GalleryShot> {
    let mut shots = Vec::new();

    let mut progress = launch_progress();
    progress.update(
        0,
        LaunchStepState::Running,
        "Reading the saved local setup...".to_string(),
    );
    shots.push(launch_shot(
        "checks-starting",
        "Returning-user checks",
        "Secure launch — starting",
        "The first screen on later launches, before the saved setup is trusted.",
        &progress,
    ));

    let mut progress = launch_progress();
    progress.update(
        0,
        LaunchStepState::Complete,
        "Saved setup loaded".to_string(),
    );
    progress.update(
        1,
        LaunchStepState::Running,
        "Opening the operating-system credential vault...".to_string(),
    );
    shots.push(launch_shot(
        "checks-vault",
        "Returning-user checks",
        "Secure launch — credential vault",
        "The saved private key and shutdown token are checked without displaying either secret.",
        &progress,
    ));

    let mut progress = launch_progress();
    complete_step(&mut progress, 0, "Saved setup loaded");
    complete_step(&mut progress, 1, "Credential vault opened");
    progress.update(
        2,
        LaunchStepState::Warning,
        "REST is available; one live feed is reconnecting".to_string(),
    );
    progress.update(
        3,
        LaunchStepState::Running,
        "Authenticating and refreshing balances...".to_string(),
    );
    shots.push(launch_shot(
        "checks-degraded",
        "Returning-user checks",
        "Secure launch — degraded connectivity",
        "A recoverable network warning remains visible while safe checks continue.",
        &progress,
    ));

    let mut progress = launch_progress();
    complete_step(&mut progress, 0, "Saved setup loaded");
    complete_step(&mut progress, 1, "Credential vault opened");
    complete_step(&mut progress, 2, "Polymarket endpoints reachable");
    complete_step(&mut progress, 3, "Wallet authenticated • pUSD $25.00");
    progress.update(
        4,
        LaunchStepState::Skipped,
        "Previously approved — prompt skipped".to_string(),
    );
    progress.update(
        5,
        LaunchStepState::Running,
        "Opening the private localhost listener...".to_string(),
    );
    shots.push(launch_shot(
        "checks-permissions-skipped",
        "Returning-user checks",
        "Secure launch — saved permissions reused",
        "Later launches show that prior choices were reused instead of asking again.",
        &progress,
    ));

    let mut progress = completed_launch_progress();
    progress.update(
        4,
        LaunchStepState::Skipped,
        "Previously approved — prompt skipped".to_string(),
    );
    shots.push(launch_shot(
        "checks-complete",
        "Returning-user checks",
        "Secure launch — all checks complete",
        "The handoff point immediately before the runtime page appears.",
        &progress,
    ));

    shots.push(runtime_shot(
        "runtime-starting",
        "Live runtime",
        "Runtime — starting",
        "The dashboard listener is ready while live feeds are still connecting.",
        RuntimeHealth::Starting,
        "Starting live services",
        None,
        &starting_logs(),
    ));
    shots.push(runtime_shot(
        "runtime-active",
        "Live runtime",
        "Runtime — active",
        "The normal healthy state with all public feeds connected.",
        RuntimeHealth::Active,
        "3/3 live feeds connected",
        None,
        &active_logs(),
    ));
    shots.push(runtime_shot(
        "runtime-degraded",
        "Live runtime",
        "Runtime — degraded",
        "The service remains available while one public feed reconnects.",
        RuntimeHealth::Degraded,
        "2/3 live feeds connected",
        None,
        &degraded_logs(),
    ));
    shots.push(runtime_shot(
        "runtime-attention",
        "Live runtime",
        "Runtime — attention required",
        "A serious connectivity or portfolio problem is called out without hiding the dashboard link.",
        RuntimeHealth::Attention,
        "Polymarket connectivity needs attention",
        None,
        &attention_logs(),
    ));

    let copied_notice = DashboardNotice {
        message: "Copied complete private dashboard URL".to_string(),
        level: RuntimeLogLevel::Success,
        frames_remaining: COPY_NOTICE_FRAMES,
    };
    shots.push(runtime_shot(
        "runtime-url-copied",
        "Live runtime",
        "Runtime — dashboard URL copied",
        "Pressing C shows a short success notice without exposing the secret elsewhere.",
        RuntimeHealth::Active,
        "3/3 live feeds connected",
        Some(&copied_notice),
        &active_logs(),
    ));

    let copy_error = DashboardNotice {
        message: "Clipboard unavailable — copy the complete link manually".to_string(),
        level: RuntimeLogLevel::Error,
        frames_remaining: COPY_NOTICE_FRAMES,
    };
    shots.push(runtime_shot(
        "runtime-copy-failure",
        "Failures and constraints",
        "Runtime — clipboard unavailable",
        "The fallback explains that the complete link can still be copied manually.",
        RuntimeHealth::Active,
        "3/3 live feeds connected",
        Some(&copy_error),
        &active_logs(),
    ));

    shots.push(capture(
        "startup-failure",
        "Failures and constraints",
        "Startup stopped safely",
        "A failed returning-user check closes without changing permissions or system settings.",
        PREVIEW_WIDTH,
        PREVIEW_HEIGHT,
        |frame| {
            render_failure(
                frame,
                "The saved setup could not authenticate. Run `polytread diagnose`, then try again.",
            )
        },
    ));

    shots.push(capture(
        "terminal-too-small",
        "Failures and constraints",
        "Terminal too small",
        "The resize-only fallback below the supported 80 × 24 terminal size.",
        72,
        20,
        |frame| {
            render_runtime(
                frame,
                0,
                EXAMPLE_DASHBOARD_URL,
                None,
                RuntimeHealth::Starting,
                "Starting live services",
                &starting_logs(),
            )
        },
    ));

    shots
}

fn launch_progress() -> LaunchProgress {
    LaunchProgress::new([
        "Load saved configuration",
        "Open credential vault",
        "Verify Polymarket connectivity",
        "Authenticate and check funds",
        "Reuse saved permissions",
        "Start local dashboard",
    ])
}

fn completed_launch_progress() -> LaunchProgress {
    let mut progress = launch_progress();
    complete_step(&mut progress, 0, "Saved setup loaded");
    complete_step(&mut progress, 1, "Credential vault opened");
    complete_step(&mut progress, 2, "Polymarket endpoints reachable");
    complete_step(&mut progress, 3, "Wallet authenticated • pUSD $25.00");
    complete_step(&mut progress, 4, "Saved browser choice reused");
    complete_step(&mut progress, 5, "Private dashboard ready");
    progress
}

fn complete_step(progress: &mut LaunchProgress, index: usize, detail: &str) {
    progress.update(index, LaunchStepState::Complete, detail.to_string());
}

fn starting_logs() -> VecDeque<RuntimeLog> {
    logs(&[
        (RuntimeLogLevel::Success, "Saved configuration loaded"),
        (
            RuntimeLogLevel::Success,
            "Dashboard listener opened on localhost",
        ),
        (RuntimeLogLevel::Info, "Waiting for live public feeds"),
    ])
}

fn active_logs() -> VecDeque<RuntimeLog> {
    logs(&[
        (RuntimeLogLevel::Success, "Saved permissions reused"),
        (
            RuntimeLogLevel::Success,
            "Current BTC five-minute market discovered",
        ),
        (
            RuntimeLogLevel::Success,
            "Binance, Chainlink, and market feeds are live",
        ),
    ])
}

fn degraded_logs() -> VecDeque<RuntimeLog> {
    logs(&[
        (
            RuntimeLogLevel::Success,
            "Dashboard listener opened on localhost",
        ),
        (RuntimeLogLevel::Warning, "Market feed is reconnecting"),
        (
            RuntimeLogLevel::Info,
            "Binance and Chainlink feeds remain live",
        ),
    ])
}

fn attention_logs() -> VecDeque<RuntimeLog> {
    logs(&[
        (
            RuntimeLogLevel::Success,
            "Dashboard listener opened on localhost",
        ),
        (
            RuntimeLogLevel::Error,
            "Required Polymarket endpoints are unavailable",
        ),
        (
            RuntimeLogLevel::Warning,
            "Orders remain unavailable until connectivity recovers",
        ),
    ])
}

fn logs(entries: &[(RuntimeLogLevel, &str)]) -> VecDeque<RuntimeLog> {
    entries
        .iter()
        .enumerate()
        .map(|(index, (level, message))| RuntimeLog {
            timestamp: format!("12:34:{:02}", 50 + index),
            level: *level,
            message: (*message).to_string(),
        })
        .collect()
}

fn launch_shot(
    slug: &'static str,
    category: &'static str,
    title: &'static str,
    description: &'static str,
    progress: &LaunchProgress,
) -> GalleryShot {
    capture(
        slug,
        category,
        title,
        description,
        PREVIEW_WIDTH,
        PREVIEW_HEIGHT,
        |frame| render_launch(frame, 3, progress),
    )
}

#[allow(clippy::too_many_arguments)]
fn runtime_shot(
    slug: &'static str,
    category: &'static str,
    title: &'static str,
    description: &'static str,
    health: RuntimeHealth,
    health_detail: &'static str,
    notice: Option<&DashboardNotice>,
    logs: &VecDeque<RuntimeLog>,
) -> GalleryShot {
    capture(
        slug,
        category,
        title,
        description,
        PREVIEW_WIDTH,
        PREVIEW_HEIGHT,
        |frame| {
            render_runtime(
                frame,
                12,
                EXAMPLE_DASHBOARD_URL,
                notice,
                health,
                health_detail,
                logs,
            )
        },
    )
}

fn capture(
    slug: &'static str,
    category: &'static str,
    title: &'static str,
    description: &'static str,
    width: u16,
    height: u16,
    render: impl FnOnce(&mut Frame<'_>),
) -> GalleryShot {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("runtime gallery terminal");
    terminal.draw(render).expect("render runtime gallery frame");
    GalleryShot {
        slug,
        category,
        title,
        description,
        buffer: terminal.backend().buffer().clone(),
    }
}

fn foreground_hex(color: Color) -> String {
    color_hex(color, "#e8e8e8")
}

fn background_hex(color: Color) -> String {
    color_hex(color, "#000000")
}

fn color_hex(color: Color, reset: &str) -> String {
    match color {
        Color::Reset => reset.to_string(),
        Color::Black => "#000000".to_string(),
        Color::Red => "#800000".to_string(),
        Color::Green => "#008000".to_string(),
        Color::Yellow => "#808000".to_string(),
        Color::Blue => "#000080".to_string(),
        Color::Magenta => "#800080".to_string(),
        Color::Cyan => "#008080".to_string(),
        Color::Gray => "#c0c0c0".to_string(),
        Color::DarkGray => "#808080".to_string(),
        Color::LightRed => "#ff0000".to_string(),
        Color::LightGreen => "#00ff00".to_string(),
        Color::LightYellow => "#ffff00".to_string(),
        Color::LightBlue => "#0000ff".to_string(),
        Color::LightMagenta => "#ff00ff".to_string(),
        Color::LightCyan => "#00ffff".to_string(),
        Color::White => "#ffffff".to_string(),
        Color::Indexed(index) => indexed_color(index),
        Color::Rgb(red, green, blue) => format!("#{red:02x}{green:02x}{blue:02x}"),
    }
}

fn indexed_color(index: u8) -> String {
    if index < 16 {
        const BASIC: [&str; 16] = [
            "#000000", "#800000", "#008000", "#808000", "#000080", "#800080", "#008080", "#c0c0c0",
            "#808080", "#ff0000", "#00ff00", "#ffff00", "#0000ff", "#ff00ff", "#00ffff", "#ffffff",
        ];
        return BASIC[usize::from(index)].to_string();
    }
    if index < 232 {
        let cube = index - 16;
        let red = cube / 36;
        let green = (cube % 36) / 6;
        let blue = cube % 6;
        let channel = |value: u8| if value == 0 { 0 } else { 55 + value * 40 };
        return format!(
            "#{:02x}{:02x}{:02x}",
            channel(red),
            channel(green),
            channel(blue)
        );
    }
    let gray = 8 + (index - 232) * 10;
    format!("#{gray:02x}{gray:02x}{gray:02x}")
}

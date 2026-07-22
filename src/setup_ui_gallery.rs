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
#[ignore = "writes the complete setup-state gallery requested for design review"]
fn export_complete_setup_state_gallery() {
    let output_dir = env::var_os("POLYTREAD_SETUP_GALLERY_DIR")
        .expect("set POLYTREAD_SETUP_GALLERY_DIR to an output directory");
    let output_dir = Path::new(&output_dir);
    fs::create_dir_all(output_dir).expect("create setup gallery output directory");

    let export = GalleryExport {
        source: "PolyTread Ratatui TestBackend",
        note: "Every image is rendered by the production setup renderer. Animation states use one stable frame.",
        shots: gallery_shots()
            .into_iter()
            .enumerate()
            .map(|(index, shot)| export_shot(index + 1, shot))
            .collect(),
    };
    let json = serde_json::to_vec_pretty(&export).expect("serialize setup gallery");
    fs::write(output_dir.join("setup-gallery.json"), json).expect("write setup gallery export");
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

    shots.push(capture(
        "welcome",
        "Entry and credential input",
        "Welcome / setup selection",
        "The single selected setup action before any credential is requested.",
        PREVIEW_WIDTH,
        PREVIEW_HEIGHT,
        |frame| render_menu(frame, 3),
    ));
    shots.push(capture(
        "private-key-empty",
        "Entry and credential input",
        "Private key — empty",
        "The untouched hidden credential field.",
        PREVIEW_WIDTH,
        PREVIEW_HEIGHT,
        |frame| render_private_key(frame, 3, 0, None),
    ));
    shots.push(capture(
        "private-key-masked",
        "Entry and credential input",
        "Private key — masked input",
        "A full-length entered key; the actual value is never rendered.",
        PREVIEW_WIDTH,
        PREVIEW_HEIGHT,
        |frame| render_private_key(frame, 3, 64, None),
    ));
    shots.push(capture(
        "private-key-empty-error",
        "Entry and credential input",
        "Private key — empty validation",
        "Shown after Enter is pressed without entering a key.",
        PREVIEW_WIDTH,
        PREVIEW_HEIGHT,
        |frame| render_private_key(frame, 3, 0, Some("The private key cannot be empty.")),
    ));
    shots.push(capture(
        "private-key-invalid-error",
        "Entry and credential input",
        "Private key — invalid validation",
        "Shown when the entered value is not a valid Polygon signing key.",
        PREVIEW_WIDTH,
        PREVIEW_HEIGHT,
        |frame| {
            render_private_key(
                frame,
                3,
                18,
                Some("That is not a valid Polygon private key. Check it and try again."),
            )
        },
    ));

    let mut progress = setup_progress();
    progress.running(0, "Deriving the Polygon signing address...");
    shots.push(progress_shot(
        "derive-signer-running",
        "Connectivity and DNS",
        "Step 1 — deriving signer",
        "The first animated progress row after the key is accepted.",
        &progress,
        ProgressPanel::None,
    ));

    let mut progress = setup_progress();
    progress.complete(0, "0x19E7E376E7C213B7E7e7e46cc70A5dD086DAff2A");
    progress.running(1, "Checking real REST and WebSocket endpoints...");
    shots.push(progress_shot(
        "connectivity-running",
        "Connectivity and DNS",
        "Step 2 — connectivity check",
        "Live endpoint checks with the derived signer retained above.",
        &progress,
        ProgressPanel::None,
    ));

    let mut progress = setup_progress();
    progress.complete(0, "0x19E7E376E7C213B7E7e7e46cc70A5dD086DAff2A");
    progress.warning(
        1,
        "Required REST endpoints work; WebSocket checks will continue in the dashboard",
    );
    progress.running(2, "Reading the public Polymarket profile...");
    shots.push(progress_shot(
        "connectivity-degraded",
        "Connectivity and DNS",
        "Connectivity degraded — continuing",
        "A non-blocking network warning counts as a completed check.",
        &progress,
        ProgressPanel::None,
    ));

    let diagnostic = concat!(
        "The system resolver differs from encrypted DNS, and a real CLOB request succeeds ",
        "through the encrypted-DNS destination. Browser Secure DNS may still work while ",
        "terminal applications fail. CLOB REST failed: connection timed out. Market REST ",
        "failed: connection timed out. Market WebSocket timed out."
    );
    let remediation = "Windows encrypted DNS on the active network adapter";
    let mut progress = setup_progress();
    progress.complete(0, "0x19E7E376E7C213B7E7e7e46cc70A5dD086DAff2A");
    progress.awaiting_input(1, "DNS or ISP filtering detected");
    shots.push(progress_shot(
        "dns-acknowledgement-empty",
        "Connectivity and DNS",
        "DNS acknowledgement — initial",
        "Plain-language approval with an optional shortcut to the technical explanation.",
        &progress,
        ProgressPanel::DnsConfirmation {
            value: "",
            error: None,
        },
    ));
    shots.push(progress_shot(
        "dns-change-details",
        "Connectivity and DNS",
        "DNS acknowledgement — more details",
        "The optional technical and rollback explanation opened with the I key.",
        &progress,
        ProgressPanel::DnsDetails {
            remediation,
            detail: diagnostic,
        },
    ));
    shots.push(progress_shot(
        "dns-acknowledgement-yes",
        "Connectivity and DNS",
        "DNS acknowledgement — YES entered",
        "The exact acknowledgement immediately before approval is submitted.",
        &progress,
        ProgressPanel::DnsConfirmation {
            value: "YES",
            error: None,
        },
    ));
    shots.push(progress_shot(
        "dns-acknowledgement-invalid",
        "Connectivity and DNS",
        "DNS acknowledgement — invalid response",
        "Recovery guidance after a value other than exact YES is submitted.",
        &progress,
        ProgressPanel::DnsConfirmation {
            value: "YEP",
            error: Some("Type YES exactly, or clear the field to stop."),
        },
    ));

    let mut progress = setup_progress();
    progress.complete(0, "0x19E7E376E7C213B7E7e7e46cc70A5dD086DAff2A");
    progress.running(1, "Requesting the approved operating-system DNS change...");
    shots.push(progress_shot(
        "dns-change-locked",
        "Connectivity and DNS",
        "DNS change — locked operation",
        "Cancellation is temporarily disabled while an approved system mutation finishes.",
        &progress,
        ProgressPanel::Locked,
    ));

    let mut progress = setup_progress();
    progress.complete(0, "0x19E7E376E7C213B7E7e7e46cc70A5dD086DAff2A");
    progress.awaiting_input(1, "Complete the operating-system step, then continue");
    shots.push(progress_shot(
        "dns-operating-system-step",
        "Connectivity and DNS",
        "DNS change — operating-system step",
        "A required Enter acknowledgement after the external system action.",
        &progress,
        ProgressPanel::LockedNotice {
            title: "OPERATING-SYSTEM STEP",
            detail: "Approve the encrypted-DNS change in the Windows system dialog, then return to PolyTread.",
        },
    ));

    let mut progress = setup_progress();
    progress.complete(0, "0x19E7E376E7C213B7E7e7e46cc70A5dD086DAff2A");
    progress.running(1, "Checking real REST and WebSocket endpoints...");
    progress.fail_active("Stopped safely — see the explanation below");
    shots.push(progress_shot(
        "connectivity-failure",
        "Connectivity and DNS",
        "Connectivity — hard failure",
        "A non-remediable endpoint failure before credentials are saved.",
        &progress,
        ProgressPanel::Failure {
            error: "Setup stopped before saving credentials because Polymarket connectivity is unavailable.",
        },
    ));

    let signer = "0x19E7E376E7C213B7E7e7e46cc70A5dD086DAff2A";
    let mut progress = completed_through(1);
    progress.awaiting_input(2, "Profile lookup was inconclusive — confirm the address");
    shots.push(progress_shot(
        "funding-wallet-empty",
        "Wallet and authentication",
        "Funding wallet — fallback",
        "Empty Enter uses the signer address when profile discovery is inconclusive.",
        &progress,
        ProgressPanel::FundingWallet {
            value: "",
            signer_address: signer,
            error: None,
        },
    ));
    shots.push(progress_shot(
        "funding-wallet-invalid",
        "Wallet and authentication",
        "Funding wallet — invalid address",
        "Inline recovery after an invalid public funding-wallet address.",
        &progress,
        ProgressPanel::FundingWallet {
            value: "0x1234",
            signer_address: signer,
            error: Some("Enter a valid 0x funding-wallet address, or clear it to use the signer."),
        },
    ));

    let mut progress = completed_through(2);
    progress.awaiting_input(
        3,
        "Automatic detection was inconclusive — choose the wallet type",
    );
    shots.push(progress_shot(
        "wallet-type-selection",
        "Wallet and authentication",
        "Wallet type — manual selection",
        "The numbered fallback when supported wallet modes cannot be detected automatically.",
        &progress,
        ProgressPanel::WalletType,
    ));

    let mut progress = completed_through(3);
    progress.running(
        4,
        "Authenticating and checking pUSD balance and allowances...",
    );
    shots.push(progress_shot(
        "credentials-authenticating",
        "Wallet and authentication",
        "Credentials — authenticating",
        "The credential and balance validation operation in progress.",
        &progress,
        ProgressPanel::None,
    ));

    let mut progress = completed_through(3);
    progress.warning(
        4,
        "Valid, no available pUSD  •  std $0.0000  •  neg-risk $0.0000",
    );
    progress.awaiting_input(5, "Choose Y to enable or N to stay view-only");
    shots.push(progress_shot(
        "browser-choice-no-funds",
        "Wallet and authentication",
        "Credentials valid — no available pUSD",
        "A valid account can continue, but the no-funds condition remains visible.",
        &progress,
        ProgressPanel::BrowserTrading,
    ));

    let mut progress = completed_through(3);
    progress.complete(4, "pUSD $25.0000  •  std $25.0000  •  neg-risk $25.0000");
    progress.awaiting_input(5, "Choose Y to enable or N to stay view-only");
    shots.push(progress_shot(
        "browser-choice-funded",
        "Wallet and authentication",
        "Browser trading — final Y/N choice",
        "The final safety choice for a funded, authenticated account.",
        &progress,
        ProgressPanel::BrowserTrading,
    ));

    let mut progress = completed_through(4);
    progress.running(5, "Securing credentials and local settings...");
    shots.push(progress_shot(
        "saving-configuration",
        "Wallet and authentication",
        "Saving credentials and settings",
        "The short final operation after the browser-trading choice.",
        &progress,
        ProgressPanel::None,
    ));

    let mut enabled = completed_through(4);
    enabled.complete(5, "Enabled — dashboard still starts disarmed");
    shots.push(progress_shot(
        "complete-enabled",
        "Outcomes and constraints",
        "Setup complete — browser trading enabled",
        "Successful completion with manual browser orders allowed but still disarmed.",
        &enabled,
        ProgressPanel::Complete {
            browser_trading: true,
        },
    ));

    let mut view_only = completed_through(4);
    view_only.complete(5, "View-only");
    shots.push(progress_shot(
        "complete-view-only",
        "Outcomes and constraints",
        "Setup complete — view-only",
        "Successful completion without browser trading permission.",
        &view_only,
        ProgressPanel::Complete {
            browser_trading: false,
        },
    ));

    let mut progress = completed_through(3);
    progress.running(
        4,
        "Authenticating and checking pUSD balance and allowances...",
    );
    progress.fail_active("Stopped safely — see the explanation below");
    shots.push(progress_shot(
        "authentication-failure",
        "Outcomes and constraints",
        "Authentication — failure",
        "A credential or wallet mismatch stops before local configuration completes.",
        &progress,
        ProgressPanel::Failure {
            error: "Credential validation failed. Verify the funding wallet and wallet type before trying again.",
        },
    ));

    let mut progress = setup_progress();
    progress.complete(0, "0x19E7E376E7C213B7E7e7e46cc70A5dD086DAff2A");
    progress.running(
        1,
        "Connectivity is still unavailable — restoring the original DNS settings...",
    );
    progress.fail_active("Stopped safely — see the explanation below");
    shots.push(progress_shot(
        "dns-rollback-failure",
        "Outcomes and constraints",
        "DNS rollback — failure",
        "The longest recovery error includes the explicit restore command.",
        &progress,
        ProgressPanel::Failure {
            error: "Connectivity remained unavailable and automatic DNS rollback failed. Run `polytread restore-dns` before trying setup again.",
        },
    ));

    shots.push(capture(
        "terminal-too-small",
        "Outcomes and constraints",
        "Terminal too small",
        "The resize-only fallback below the supported 80 × 24 terminal size.",
        72,
        20,
        |frame| render_menu(frame, 0),
    ));

    shots
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

fn completed_through(last_index: usize) -> SetupProgress {
    let details = [
        "0x19E7E376E7C213B7E7e7e46cc70A5dD086DAff2A",
        "Polymarket endpoints reachable",
        "0x9999999999999999999999999999999999999999",
        "Gnosis Safe",
        "pUSD $25.0000  •  std $25.0000  •  neg-risk $25.0000",
        "View-only",
    ];
    let mut progress = setup_progress();
    for (index, detail) in details.iter().enumerate().take(last_index + 1) {
        progress.complete(index, *detail);
    }
    progress
}

fn progress_shot(
    slug: &'static str,
    category: &'static str,
    title: &'static str,
    description: &'static str,
    progress: &SetupProgress,
    panel: ProgressPanel<'_>,
) -> GalleryShot {
    capture(
        slug,
        category,
        title,
        description,
        PREVIEW_WIDTH,
        PREVIEW_HEIGHT,
        |frame| render_progress(frame, 3, progress, panel),
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
    let mut terminal = Terminal::new(backend).expect("setup gallery terminal");
    terminal.draw(render).expect("render setup gallery frame");
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

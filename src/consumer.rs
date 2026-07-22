use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::str::FromStr;
use std::time::Duration;

use alloy::primitives::Address;
use alloy::signers::Signer as _;
use alloy::signers::local::PrivateKeySigner;
use anyhow::{Context, Result, anyhow, bail};
use directories::ProjectDirs;
use keyring::Entry;
use polymarket_client_sdk_v2::{POLYGON, derive_proxy_wallet, derive_safe_wallet};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

use crate::app::{self, DashboardIdentity, RunOutcome};
use crate::config::{
    DEFAULT_POLYGON_RPC_URL, GAMMA_API_URL, RELAYER_API_URL, ServeArgs, TradingArgs,
};
use crate::connectivity;
use crate::dns_remediation;
use crate::local_control;
use crate::runtime_ui::{RuntimeUi, is_cancelled as runtime_ui_was_cancelled};
use crate::setup_ui::{
    PrivateKeyAction, SetupProgress, SetupUi, is_cancelled as setup_was_cancelled,
};
use crate::trading::{TradingRuntimeConfig, validate_trading_config};

const CONFIG_VERSION: u8 = 1;
const DEFAULT_BIND: &str = "127.0.0.1:9878";
const VAULT_SERVICE: &str = "xyz.polytread.cli";
const PRIVATE_KEY_VAULT_NAME: &str = "trading-private-key";
const CONTROL_TOKEN_VAULT_NAME: &str = "local-control-token";
const BACKGROUND_BOOTSTRAP_VERSION: u8 = 1;
const MAX_BACKGROUND_BOOTSTRAP_BYTES: u64 = 4_096;
const BACKGROUND_READY_ATTEMPTS: usize = 100;

const STEP_DERIVE_SIGNER: usize = 0;
const STEP_CONNECTIVITY: usize = 1;
const STEP_FUNDING_WALLET: usize = 2;
const STEP_WALLET_TYPE: usize = 3;
const STEP_VALIDATE_CREDENTIALS: usize = 4;
const STEP_BROWSER_TRADING: usize = 5;

const RETURN_STEP_CONFIG: usize = 0;
const RETURN_STEP_VAULT: usize = 1;
const RETURN_STEP_CONNECTIVITY: usize = 2;
const RETURN_STEP_CREDENTIALS: usize = 3;
const RETURN_STEP_PERMISSIONS: usize = 4;
const RETURN_STEP_DASHBOARD: usize = 5;

const POST_SETUP_STEP_CONFIGURATION: usize = 0;
const POST_SETUP_STEP_VAULT: usize = 1;
const POST_SETUP_STEP_DASHBOARD: usize = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConsumerConfig {
    version: u8,
    signer_address: String,
    funder_address: String,
    signature_type: u8,
    bind: String,
    allow_web_trading: bool,
}

#[derive(Debug, Clone)]
struct ConsumerPaths {
    config_file: PathBuf,
    data_dir: PathBuf,
    dns_backup_file: PathBuf,
}

struct CompletedSetup {
    config: ConsumerConfig,
    start_runtime: bool,
}

#[derive(Serialize, Deserialize)]
struct BackgroundBootstrap {
    version: u8,
    dashboard_identity: DashboardIdentity,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PublicProfile {
    proxy_wallet: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeployedResponse {
    deployed: bool,
}

pub async fn start() -> Result<()> {
    match read_config()? {
        Some(config) if io::stdin().is_terminal() && io::stdout().is_terminal() => {
            start_returning_user(config).await
        }
        Some(config) => app::run(serve_args(&config)?).await,
        None => {
            let completed = setup(false).await?;
            if completed.start_runtime {
                start_after_setup(completed.config).await
            } else {
                Ok(())
            }
        }
    }
}

pub async fn setup_and_start(force: bool) -> Result<()> {
    let completed = setup(force).await?;
    if completed.start_runtime {
        start_after_setup(completed.config).await
    } else {
        Ok(())
    }
}

async fn start_returning_user(config: ConsumerConfig) -> Result<()> {
    let mut ui = RuntimeUi::enter([
        "Load saved configuration",
        "Open credential vault",
        "Verify Polymarket connectivity",
        "Authenticate and check funds",
        "Reuse saved permissions",
        "Start local dashboard",
    ])?;

    let args = match Box::pin(run_returning_checks(&mut ui, config)).await {
        Ok(args) => args,
        Err(error) if runtime_ui_was_cancelled(&error) => return Err(error),
        Err(error) => {
            ui.fail_active("Startup check failed — no permissions or settings were changed");
            let error_text = format!("{error:#}");
            let _ = ui.show_failure(&error_text);
            return Err(error);
        }
    };

    run_runtime_with_ui(args, ui).await
}

async fn start_after_setup(config: ConsumerConfig) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return app::run(serve_args(&config)?).await;
    }

    let mut ui = RuntimeUi::enter([
        "Use completed setup",
        "Open credential vault",
        "Start local dashboard",
    ])?;
    ui.complete(
        POST_SETUP_STEP_CONFIGURATION,
        "Configuration, wallet checks, and permissions are ready",
    );
    ui.running(
        POST_SETUP_STEP_VAULT,
        "Loading the credentials saved during setup...",
    );
    let config_for_vault = config.clone();
    let args_result = ui
        .animate_while(Box::pin(async move {
            tokio::task::spawn_blocking(move || serve_args(&config_for_vault))
                .await
                .context("post-setup credential-vault task failed")?
        }))
        .await
        .and_then(|result| result);
    let args = match args_result {
        Ok(args) => args,
        Err(error) => {
            ui.fail_active("The saved credentials could not be reopened");
            let error_text = format!("{error:#}");
            let _ = ui.show_failure(&error_text);
            return Err(error);
        }
    };
    ui.complete(
        POST_SETUP_STEP_VAULT,
        "New credentials loaded from the operating-system vault",
    );
    ui.running(
        POST_SETUP_STEP_DASHBOARD,
        "Opening the configured loopback listener...",
    );
    run_runtime_with_ui(args, ui).await
}

async fn run_runtime_with_ui(args: ServeArgs, mut ui: RuntimeUi) -> Result<()> {
    ui.animate_briefly(Duration::from_millis(160)).await?;
    let bind = args.bind.clone();
    match app::run_with_ui(args, &mut ui).await {
        Ok(RunOutcome::Stopped) => Ok(()),
        Ok(RunOutcome::Detach {
            identity,
            dashboard_url,
        }) => {
            drop(ui);
            handoff_to_background(identity, &bind).await?;
            println!("PolyTread is running in the background.");
            println!("Dashboard: {dashboard_url}");
            println!("Stop the service with: polytread shutdown");
            Ok(())
        }
        Err(error) => {
            ui.fail_active("PolyTread stopped before the runtime became healthy");
            let error_text = format!("{error:#}");
            let _ = ui.show_failure(&error_text);
            Err(error)
        }
    }
}

pub async fn background_worker() -> Result<()> {
    if io::stdin().is_terminal() {
        bail!("the internal background worker can only be started by PolyTread");
    }
    let bootstrap = read_background_bootstrap()?;
    if bootstrap.version != BACKGROUND_BOOTSTRAP_VERSION {
        bail!(
            "unsupported background bootstrap version {}",
            bootstrap.version
        );
    }
    bootstrap.dashboard_identity.validate()?;
    let config = read_config()?.ok_or_else(|| anyhow!("PolyTread has not been set up yet"))?;
    let args = serve_args(&config)?;
    app::run_background(args, bootstrap.dashboard_identity).await
}

async fn handoff_to_background(identity: DashboardIdentity, bind: &str) -> Result<()> {
    let mut child = spawn_background_worker(identity.clone())?;
    let readiness = wait_for_background_worker(&mut child, bind, &identity).await;
    if let Err(error) = readiness {
        let child_id = child.id();
        let _ = child.kill();
        let _ = child.wait();
        return Err(error).with_context(|| {
            format!("background worker {child_id} did not take over the local dashboard")
        });
    }
    Ok(())
}

fn spawn_background_worker(identity: DashboardIdentity) -> Result<Child> {
    let executable = std::env::current_exe()
        .context("failed locating the current PolyTread executable for background handoff")?;
    let bootstrap = BackgroundBootstrap {
        version: BACKGROUND_BOOTSTRAP_VERSION,
        dashboard_identity: identity,
    };
    let payload = Zeroizing::new(
        serde_json::to_vec(&bootstrap).context("failed preparing the background handoff")?,
    );
    let mut command = ProcessCommand::new(executable);
    command
        .arg("__background-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;

        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;

        command.process_group(0);
    }

    let mut child = command
        .spawn()
        .context("failed starting the PolyTread background worker")?;
    let write_result = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("background worker stdin was unavailable"))
        .and_then(|mut stdin| {
            stdin
                .write_all(payload.as_slice())
                .context("failed sending the private background bootstrap")?;
            stdin
                .flush()
                .context("failed flushing the private background bootstrap")
        });
    if let Err(error) = write_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    Ok(child)
}

fn read_background_bootstrap() -> Result<BackgroundBootstrap> {
    let mut payload = Vec::new();
    io::stdin()
        .take(MAX_BACKGROUND_BOOTSTRAP_BYTES + 1)
        .read_to_end(&mut payload)
        .context("failed reading the private background bootstrap")?;
    if payload.len() as u64 > MAX_BACKGROUND_BOOTSTRAP_BYTES {
        payload.zeroize();
        bail!("background bootstrap exceeded its size limit");
    }
    let payload = Zeroizing::new(payload);
    serde_json::from_slice(payload.as_slice()).context("invalid background bootstrap")
}

async fn wait_for_background_worker(
    child: &mut Child,
    bind: &str,
    identity: &DashboardIdentity,
) -> Result<()> {
    let client = Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .context("failed building the background readiness client")?;
    let auth_url = format!("http://{bind}/_auth/session");
    for _ in 0..BACKGROUND_READY_ATTEMPTS {
        if let Some(status) = child
            .try_wait()
            .context("failed checking the PolyTread background worker")?
        {
            bail!("background worker exited early ({status})");
        }
        if let Ok(response) = client
            .post(&auth_url)
            .bearer_auth(identity.access_token())
            .send()
            .await
            && response.status().is_success()
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("background dashboard did not become ready within 10 seconds")
}

async fn run_returning_checks(ui: &mut RuntimeUi, config: ConsumerConfig) -> Result<ServeArgs> {
    ui.running(
        RETURN_STEP_CONFIG,
        "Validating the saved local configuration...",
    );
    ui.animate_briefly(Duration::from_millis(180)).await?;
    validate_config(&config)?;
    ui.complete(
        RETURN_STEP_CONFIG,
        format!(
            "{} wallet configuration loaded",
            wallet_type_label(config.signature_type)
        ),
    );

    ui.running(
        RETURN_STEP_VAULT,
        "Checking the operating-system credential vault...",
    );
    let config_for_vault = config.clone();
    let args = ui
        .animate_while(Box::pin(async move {
            tokio::task::spawn_blocking(move || serve_args(&config_for_vault))
                .await
                .context("credential-vault check task failed")?
        }))
        .await??;
    ui.complete(
        RETURN_STEP_VAULT,
        "Private key and local control token are available",
    );

    let client = setup_http_client()?;
    ui.running(
        RETURN_STEP_CONNECTIVITY,
        "Checking real REST and WebSocket endpoints...",
    );
    let connectivity_status = ui
        .animate_while(Box::pin(connectivity::probe(&client)))
        .await?;
    ui.seed_connectivity(connectivity_status.clone());
    if !connectivity_status.is_usable_for_setup() {
        bail!(
            "returning launch stopped because {}: {}. No DNS or system change was requested",
            connectivity_status.headline,
            connectivity_status.detail
        );
    }
    if connectivity_status.kind == connectivity::ConnectivityKind::Degraded {
        ui.warning(
            RETURN_STEP_CONNECTIVITY,
            "Required REST endpoints work; live WebSocket monitors will keep retrying",
        );
    } else {
        ui.complete(RETURN_STEP_CONNECTIVITY, connectivity_status.headline);
    }

    ui.running(
        RETURN_STEP_CREDENTIALS,
        "Authenticating and checking pUSD balance and allowances...",
    );
    let runtime = trading_runtime_from_serve_args(&args)?;
    let validation = ui
        .animate_while(Box::pin(validate_trading_config(&runtime)))
        .await??;
    if validation.available_pusd <= 0.0 {
        ui.warning(
            RETURN_STEP_CREDENTIALS,
            format!(
                "Credentials valid; no pUSD available • std ${:.4} • neg-risk ${:.4}",
                validation.regular_allowance_pusd, validation.neg_risk_allowance_pusd
            ),
        );
    } else {
        ui.complete(
            RETURN_STEP_CREDENTIALS,
            format!(
                "pUSD ${:.4} • std ${:.4} • neg-risk ${:.4}",
                validation.available_pusd,
                validation.regular_allowance_pusd,
                validation.neg_risk_allowance_pusd
            ),
        );
    }

    ui.skipped(
        RETURN_STEP_PERMISSIONS,
        saved_permission_detail(config.allow_web_trading),
    );
    ui.animate_briefly(Duration::from_millis(160)).await?;
    ui.running(
        RETURN_STEP_DASHBOARD,
        "Opening the configured loopback listener...",
    );
    Ok(args)
}

fn trading_runtime_from_serve_args(args: &ServeArgs) -> Result<TradingRuntimeConfig> {
    Ok(TradingRuntimeConfig {
        signer_address: args
            .trading
            .signer_address
            .clone()
            .ok_or_else(|| anyhow!("saved signer address is unavailable"))?,
        funder_address: args
            .trading
            .funder_address
            .clone()
            .ok_or_else(|| anyhow!("saved funding wallet is unavailable"))?,
        private_key: args
            .trading
            .private_key
            .clone()
            .ok_or_else(|| anyhow!("saved private key is unavailable"))?,
        signature_type: args
            .trading
            .signature_type
            .ok_or_else(|| anyhow!("saved wallet type is unavailable"))?,
    })
}

fn saved_permission_detail(allow_web_trading: bool) -> &'static str {
    if allow_web_trading {
        "Previously enabled — prompt skipped; dashboard still starts disarmed"
    } else {
        "Saved view-only choice — prompt skipped"
    }
}

async fn setup(force: bool) -> Result<CompletedSetup> {
    if let Some(existing) = read_config()?
        && !force
    {
        println!(
            "PolyTread is already configured for {} ({}).",
            existing.funder_address,
            wallet_type_label(existing.signature_type)
        );
        println!("Run `polytread setup --force` to replace the local setup.");
        return Ok(CompletedSetup {
            config: existing,
            start_runtime: false,
        });
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!(
            "first-run setup requires an interactive terminal; run `polytread` directly in a terminal"
        );
    }

    let mut ui = SetupUi::enter()?;
    ui.select_setup()?;

    let mut key_error = None;
    let (private_key, signer) = loop {
        match ui.read_private_key(key_error.as_deref())? {
            PrivateKeyAction::Back => {
                key_error = None;
                ui.select_setup()?;
            }
            PrivateKeyAction::Submitted(private_key) => {
                match PrivateKeySigner::from_str(private_key.as_str()) {
                    Ok(signer) => break (private_key, signer.with_chain_id(Some(POLYGON))),
                    Err(_) => {
                        key_error = Some(
                            "That is not a valid Polygon private key. Check it and try again."
                                .to_string(),
                        );
                    }
                }
            }
        }
    };

    let mut progress = SetupProgress::new([
        "Derived signer",
        "Verify Polymarket connectivity",
        "Discover funding wallet",
        "Detect wallet type",
        "Authenticate and check funds",
        "Browser trading",
    ]);
    match Box::pin(run_setup_wizard(
        &mut ui,
        &mut progress,
        private_key,
        signer,
    ))
    .await
    {
        Ok((config, allow_web_trading)) => {
            let start_runtime = ui.show_complete(&progress, allow_web_trading)?;
            Ok(CompletedSetup {
                config,
                start_runtime,
            })
        }
        Err(error) if setup_was_cancelled(&error) => Err(error),
        Err(error) => {
            progress.fail_active("Stopped safely — see the explanation below");
            let error_text = format!("{error:#}");
            let _ = ui.show_failure(&progress, &error_text);
            Err(error)
        }
    }
}

async fn run_setup_wizard(
    ui: &mut SetupUi,
    progress: &mut SetupProgress,
    private_key: Zeroizing<String>,
    signer: PrivateKeySigner,
) -> Result<(ConsumerConfig, bool)> {
    progress.running(
        STEP_DERIVE_SIGNER,
        "Deriving the Polygon signing address...",
    );
    ui.animate_briefly(progress, Duration::from_millis(280))
        .await?;
    let signer_address = signer.address();
    progress.complete(STEP_DERIVE_SIGNER, signer_address.to_string());

    let client = Box::pin(setup_connectivity_preflight(ui, progress)).await?;

    progress.running(
        STEP_FUNDING_WALLET,
        "Reading the public Polymarket profile...",
    );
    let discovered_funder = ui
        .animate_while(
            progress,
            Box::pin(fetch_profile_funder(&client, signer_address)),
        )
        .await?;
    let funder_address = match discovered_funder {
        Ok(Some(address)) => address,
        Ok(None) | Err(_) => {
            progress.awaiting_input(
                STEP_FUNDING_WALLET,
                "Profile lookup was inconclusive — confirm the address",
            );
            let mut address_error = None;
            loop {
                let entered = ui.prompt_funding_wallet(
                    progress,
                    &signer_address.to_string(),
                    address_error.as_deref(),
                )?;
                if entered.is_empty() {
                    break signer_address;
                }
                match Address::from_str(&entered) {
                    Ok(address) => break address,
                    Err(_) => {
                        address_error = Some(
                            "Enter a valid 0x funding-wallet address, or clear it to use the signer."
                                .to_string(),
                        );
                    }
                }
            }
        }
    };
    progress.complete(STEP_FUNDING_WALLET, funder_address.to_string());

    progress.running(STEP_WALLET_TYPE, "Matching supported wallet modes...");
    let detected = ui
        .animate_while(
            progress,
            Box::pin(detect_wallet_type(&client, signer_address, funder_address)),
        )
        .await?;
    let (signature_type, manually_confirmed) = match detected {
        Ok(value) => (value, false),
        Err(_) => {
            progress.awaiting_input(
                STEP_WALLET_TYPE,
                "Automatic detection was inconclusive — choose the wallet type",
            );
            (ui.prompt_wallet_type(progress)?, true)
        }
    };
    progress.complete(
        STEP_WALLET_TYPE,
        if manually_confirmed {
            format!("{} — confirmed manually", wallet_type_label(signature_type))
        } else {
            wallet_type_label(signature_type).to_string()
        },
    );

    let runtime = TradingRuntimeConfig {
        signer_address: signer_address.to_string(),
        funder_address: funder_address.to_string(),
        private_key: private_key.to_string(),
        signature_type,
    };
    progress.running(
        STEP_VALIDATE_CREDENTIALS,
        "Authenticating and checking pUSD balance and allowances...",
    );
    let validation = ui
        .animate_while(progress, Box::pin(validate_trading_config(&runtime)))
        .await??;
    let validation_detail = format!(
        "pUSD ${:.4}  •  std ${:.4}  •  neg-risk ${:.4}",
        validation.available_pusd,
        validation.regular_allowance_pusd,
        validation.neg_risk_allowance_pusd
    );
    if validation.available_pusd <= 0.0 {
        progress.warning(
            STEP_VALIDATE_CREDENTIALS,
            format!(
                "Valid, no available pUSD  •  std ${:.4}  •  neg-risk ${:.4}",
                validation.regular_allowance_pusd, validation.neg_risk_allowance_pusd
            ),
        );
    } else {
        progress.complete(STEP_VALIDATE_CREDENTIALS, validation_detail);
    }

    progress.awaiting_input(
        STEP_BROWSER_TRADING,
        "Choose Y to enable or N to stay view-only",
    );
    let allow_web_trading = ui.confirm_browser_trading(progress)?;
    progress.running(
        STEP_BROWSER_TRADING,
        "Securing credentials and local settings...",
    );
    ui.animate_briefly(progress, Duration::from_millis(160))
        .await?;

    let config = ConsumerConfig {
        version: CONFIG_VERSION,
        signer_address: signer_address.to_string(),
        funder_address: funder_address.to_string(),
        signature_type,
        bind: DEFAULT_BIND.to_string(),
        allow_web_trading,
    };
    validate_config(&config)?;
    let control_token = Zeroizing::new(format!(
        "{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    ));
    vault_entry(PRIVATE_KEY_VAULT_NAME)?
        .set_password(private_key.as_str())
        .context("the operating-system credential vault refused the private key")?;
    vault_entry(CONTROL_TOKEN_VAULT_NAME)?
        .set_password(control_token.as_str())
        .context("the operating-system credential vault refused the local control token")?;
    write_config(&config)?;

    progress.complete(
        STEP_BROWSER_TRADING,
        if allow_web_trading {
            "Enabled — dashboard still starts disarmed"
        } else {
            "View-only"
        },
    );
    Ok((config, allow_web_trading))
}

pub async fn shutdown() -> Result<()> {
    let config = read_config()?.ok_or_else(|| anyhow!("PolyTread has not been set up yet"))?;
    if local_control::request_shutdown().await? {
        report_shutdown_completion(&config.bind).await;
        return Ok(());
    }
    let token = read_vault_secret(CONTROL_TOKEN_VAULT_NAME)?;
    let response = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?
        .post(format!("http://{}/_control/shutdown", config.bind))
        .bearer_auth(token)
        .send()
        .await
        .context("PolyTread is not reachable on its configured localhost address")?;
    if !response.status().is_success() {
        bail!(
            "shutdown was rejected by the local service ({})",
            response.status()
        );
    }
    report_shutdown_completion(&config.bind).await;
    Ok(())
}

async fn report_shutdown_completion(bind: &str) {
    let client = match Client::builder()
        .timeout(Duration::from_millis(300))
        .build()
    {
        Ok(client) => client,
        Err(_) => {
            println!("Graceful shutdown requested.");
            return;
        }
    };
    let health_url = format!("http://{bind}/healthz");
    for _ in 0..50 {
        if client.get(&health_url).send().await.is_err() {
            println!("PolyTread stopped safely.");
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    println!("Graceful shutdown requested; PolyTread is finishing its cleanup.");
}

pub async fn status() -> Result<()> {
    let config = read_config()?.ok_or_else(|| anyhow!("PolyTread has not been set up yet"))?;
    let response = Client::builder()
        .timeout(Duration::from_secs(3))
        .build()?
        .get(format!("http://{}/healthz", config.bind))
        .send()
        .await;
    match response {
        Ok(response) if response.status().is_success() => {
            println!("PolyTread is running at http://{}/", config.bind);
            Ok(())
        }
        Ok(response) => bail!(
            "PolyTread returned an unhealthy status ({})",
            response.status()
        ),
        Err(_) => bail!("PolyTread is not running at http://{}/", config.bind),
    }
}

pub async fn restore_dns() -> Result<()> {
    let path = consumer_paths()?.dns_backup_file;
    let outcome = dns_remediation::restore(&path).await?;
    if let Some(step) = outcome.user_step {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            bail!("restoring macOS encrypted DNS requires an interactive terminal");
        }
        println!("{step}");
        let _ = prompt_line("Press Enter after the operating-system step is complete: ")?;
        dns_remediation::mark_restored_after_user_step(&path)?;
    }
    println!("The saved DNS configuration has been restored.");
    Ok(())
}

pub async fn diagnose() -> Result<()> {
    let client = setup_http_client()?;
    let status = connectivity::probe(&client).await;
    println!("{}", status.headline);
    println!("{}", status.detail);
    println!(
        "CLOB REST: {} | Market REST: {} | Market WebSocket: {}",
        connectivity_label(status.clob_rest_ok),
        connectivity_label(status.market_rest_ok),
        connectivity_label(status.market_websocket_ok)
    );
    if status.is_usable_for_setup() {
        Ok(())
    } else {
        bail!("the required Polymarket connectivity path is not ready")
    }
}

fn serve_args(config: &ConsumerConfig) -> Result<ServeArgs> {
    validate_config(config)?;
    let private_key = read_vault_secret(PRIVATE_KEY_VAULT_NAME)?;
    let control_token = read_vault_secret(CONTROL_TOKEN_VAULT_NAME)?;
    let paths = consumer_paths()?;
    fs::create_dir_all(&paths.data_dir).with_context(|| {
        format!(
            "failed to create PolyTread data directory {}",
            paths.data_dir.display()
        )
    })?;
    let polygon_rpc_url = std::env::var("POLYTREAD_POLYGON_RPC_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_POLYGON_RPC_URL.to_string());
    Ok(ServeArgs {
        bind: config.bind.clone(),
        data_dir: paths.data_dir,
        allow_web_trading: config.allow_web_trading,
        history_seconds: 600,
        discovery_poll_seconds: 15,
        websocket_heartbeat_seconds: 5,
        duration_seconds: None,
        control_token: Some(control_token),
        polygon_rpc_url,
        trading: TradingArgs {
            signer_address: Some(config.signer_address.clone()),
            funder_address: Some(config.funder_address.clone()),
            private_key: Some(private_key),
            signature_type: Some(config.signature_type),
        },
    })
}

fn read_config() -> Result<Option<ConsumerConfig>> {
    let path = consumer_paths()?.config_file;
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed reading config {}", path.display()));
        }
    };
    let config = serde_json::from_slice::<ConsumerConfig>(&bytes)
        .with_context(|| format!("invalid PolyTread config {}", path.display()))?;
    validate_config(&config)?;
    Ok(Some(config))
}

fn write_config(config: &ConsumerConfig) -> Result<()> {
    let path = consumer_paths()?.config_file;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("config path has no parent directory"))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed creating config directory {}", parent.display()))?;
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("failed opening config {}", path.display()))?;
    serde_json::to_writer_pretty(&mut file, config).context("failed serializing local config")?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn validate_config(config: &ConsumerConfig) -> Result<()> {
    if config.version != CONFIG_VERSION {
        bail!(
            "unsupported local config version {}; run `polytread setup --force`",
            config.version
        );
    }
    Address::from_str(&config.signer_address).context("invalid configured signer address")?;
    Address::from_str(&config.funder_address).context("invalid configured funder address")?;
    if config.signature_type > 3 {
        bail!("invalid configured wallet type {}", config.signature_type);
    }
    let bind: SocketAddr = config
        .bind
        .parse()
        .context("invalid configured bind address")?;
    if !bind.ip().is_loopback() {
        bail!("consumer mode requires a loopback-only bind address");
    }
    Ok(())
}

fn consumer_paths() -> Result<ConsumerPaths> {
    let dirs = ProjectDirs::from("xyz", "PolyTread", "polytread")
        .ok_or_else(|| anyhow!("the operating system did not provide local app directories"))?;
    Ok(ConsumerPaths {
        config_file: dirs.config_dir().join("config.json"),
        data_dir: dirs.data_local_dir().join("history"),
        dns_backup_file: dirs.config_dir().join("dns-rollback.json"),
    })
}

async fn setup_connectivity_preflight(
    ui: &mut SetupUi,
    progress: &mut SetupProgress,
) -> Result<Client> {
    let mut client = setup_http_client()?;
    progress.running(
        STEP_CONNECTIVITY,
        "Checking real REST and WebSocket endpoints...",
    );
    let mut status = ui
        .animate_while(progress, Box::pin(connectivity::probe(&client)))
        .await?;

    if status.is_usable_for_setup() {
        if status.kind == connectivity::ConnectivityKind::Degraded {
            progress.warning(
                STEP_CONNECTIVITY,
                "Required REST endpoints work; WebSocket checks will continue in the dashboard",
            );
        } else {
            progress.complete(STEP_CONNECTIVITY, status.headline);
        }
        return Ok(client);
    }

    if !status.needs_dns_remediation() {
        bail!(
            "setup stopped before saving credentials because {}: {}",
            status.headline,
            status.detail
        );
    }

    progress.awaiting_input(STEP_CONNECTIVITY, status.headline.clone());
    let dns_detail = status.detail.clone();
    if !ui.confirm_dns_change(progress, dns_remediation::remediation_label(), &dns_detail)? {
        bail!("setup stopped before saving credentials; DNS was not changed");
    }

    let backup_path = consumer_paths()?.dns_backup_file;
    progress.running(
        STEP_CONNECTIVITY,
        "Requesting the approved operating-system DNS change...",
    );
    let outcome = match ui
        .animate_while_locked(progress, Box::pin(dns_remediation::apply(&backup_path)))
        .await?
    {
        Ok(outcome) => outcome,
        Err(error) => {
            return Err(error).context("setup stopped before saving credentials");
        }
    };
    if let Some(step) = outcome.user_step {
        progress.awaiting_input(
            STEP_CONNECTIVITY,
            "Complete the operating-system step, then continue",
        );
        ui.wait_for_required_enter(progress, "OPERATING-SYSTEM STEP", &step)?;
    }

    progress.running(
        STEP_CONNECTIVITY,
        "Waiting for the network change to become active...",
    );
    ui.animate_while_locked(
        progress,
        Box::pin(tokio::time::sleep(Duration::from_secs(1))),
    )
    .await?;
    client = setup_http_client()?;
    progress.running(
        STEP_CONNECTIVITY,
        "Rechecking real REST and WebSocket endpoints...",
    );
    status = ui
        .animate_while_locked(progress, Box::pin(connectivity::probe(&client)))
        .await?;
    if status.is_usable_for_setup() {
        if status.kind == connectivity::ConnectivityKind::Degraded {
            progress.warning(
                STEP_CONNECTIVITY,
                "DNS remediation restored REST access; WebSocket checks remain degraded",
            );
        } else {
            progress.complete(
                STEP_CONNECTIVITY,
                "Endpoints reachable after approved DNS remediation",
            );
        }
        return Ok(client);
    }

    progress.running(
        STEP_CONNECTIVITY,
        "Connectivity is still unavailable — restoring the original DNS settings...",
    );
    match ui
        .animate_while_locked(progress, Box::pin(dns_remediation::restore(&backup_path)))
        .await?
    {
        Ok(outcome) => {
            if let Some(step) = outcome.user_step {
                progress.awaiting_input(
                    STEP_CONNECTIVITY,
                    "Complete the rollback step, then continue",
                );
                ui.wait_for_required_enter(progress, "RESTORE ORIGINAL DNS", &step)?;
                dns_remediation::mark_restored_after_user_step(&backup_path)?;
            }
        }
        Err(error) => {
            bail!(
                "connectivity remained unavailable and automatic DNS rollback failed: {error}; run `polytread restore-dns` before trying setup again"
            )
        }
    }
    bail!(
        "setup stopped before saving credentials because Polymarket connectivity is still unavailable: {}",
        status.detail
    )
}

fn setup_http_client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(12))
        .build()
        .context("failed to create the setup HTTP client")
}

fn connectivity_label(connected: bool) -> &'static str {
    if connected {
        "connected"
    } else {
        "unavailable"
    }
}

fn vault_entry(name: &str) -> Result<Entry> {
    Entry::new(VAULT_SERVICE, name).context("failed to open the operating-system credential vault")
}

fn read_vault_secret(name: &str) -> Result<String> {
    vault_entry(name)?.get_password().with_context(|| {
        format!("credential vault entry {name} is unavailable; run `polytread setup --force`")
    })
}

async fn fetch_profile_funder(client: &Client, signer: Address) -> Result<Option<Address>> {
    let response = client
        .get(format!("{GAMMA_API_URL}/public-profile"))
        .query(&[("address", signer.to_string())])
        .send()
        .await
        .context("profile request failed")?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let response = response
        .error_for_status()
        .context("profile lookup failed")?;
    let profile = response
        .json::<PublicProfile>()
        .await
        .context("profile response was invalid")?;
    profile
        .proxy_wallet
        .filter(|value| !value.trim().is_empty())
        .map(|value| Address::from_str(value.trim()).context("profile returned an invalid wallet"))
        .transpose()
}

async fn detect_wallet_type(client: &Client, signer: Address, funder: Address) -> Result<u8> {
    if let Some(signature_type) = deterministic_wallet_type(signer, funder) {
        return Ok(signature_type);
    }
    // The relayer's deployed endpoint confirms that bytecode exists at an address; it does not
    // distinguish a Safe from a deposit wallet. Safe and proxy derivations must therefore be
    // checked first. Only an otherwise-unmatched deployed profile wallet is a type-3 candidate,
    // and the authenticated CLOB balance check later in setup remains the final validation.
    if relayer_reports_deployed(client, funder, "WALLET").await? {
        return Ok(3);
    }

    bail!("the funding wallet does not match a detectable EOA, proxy, Safe, or deposit wallet")
}

fn deterministic_wallet_type(signer: Address, funder: Address) -> Option<u8> {
    if signer == funder {
        return Some(0);
    }
    if derive_safe_wallet(signer, POLYGON).is_some_and(|candidate| candidate == funder) {
        return Some(2);
    }
    if derive_proxy_wallet(signer, POLYGON).is_some_and(|candidate| candidate == funder) {
        return Some(1);
    }
    None
}

async fn relayer_reports_deployed(
    client: &Client,
    address: Address,
    wallet_type: &str,
) -> Result<bool> {
    let response = client
        .get(format!("{RELAYER_API_URL}/deployed"))
        .query(&[
            ("address", address.to_string()),
            ("type", wallet_type.to_string()),
        ])
        .send()
        .await
        .with_context(|| format!("{wallet_type} deployment lookup failed"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(false);
    }
    let deployed = response
        .error_for_status()
        .with_context(|| format!("{wallet_type} deployment lookup was rejected"))?
        .json::<DeployedResponse>()
        .await
        .with_context(|| format!("{wallet_type} deployment response was invalid"))?;
    Ok(deployed.deployed)
}

fn wallet_type_label(value: u8) -> &'static str {
    match value {
        0 => "EOA",
        1 => "legacy proxy",
        2 => "Gnosis Safe",
        3 => "deposit wallet",
        _ => "unknown",
    }
}

fn prompt_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    Ok(value.trim_end_matches(['\r', '\n']).to_string())
}

#[cfg(test)]
mod tests {
    use std::mem::size_of_val;

    use super::*;

    #[test]
    fn first_run_setup_future_keeps_network_state_off_the_main_stack() {
        let future = setup(false);
        let future_size = size_of_val(&future);
        assert!(
            future_size <= 64 * 1024,
            "setup future grew to {future_size} bytes; box large async operations before awaiting"
        );
    }

    #[test]
    fn consumer_config_rejects_non_loopback_listener() {
        let config = ConsumerConfig {
            version: CONFIG_VERSION,
            signer_address: "0x0000000000000000000000000000000000000001".to_string(),
            funder_address: "0x0000000000000000000000000000000000000001".to_string(),
            signature_type: 0,
            bind: "0.0.0.0:9878".to_string(),
            allow_web_trading: false,
        };
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn wallet_labels_cover_supported_types() {
        assert_eq!(wallet_type_label(0), "EOA");
        assert_eq!(wallet_type_label(1), "legacy proxy");
        assert_eq!(wallet_type_label(2), "Gnosis Safe");
        assert_eq!(wallet_type_label(3), "deposit wallet");
    }

    #[test]
    fn returning_launch_reuses_the_saved_permission_without_prompting() {
        assert_eq!(
            saved_permission_detail(true),
            "Previously enabled — prompt skipped; dashboard still starts disarmed"
        );
        assert_eq!(
            saved_permission_detail(false),
            "Saved view-only choice — prompt skipped"
        );
    }

    #[test]
    fn eoa_wallet_type_uses_the_signer_address() {
        let signer = Address::from([0x11; 20]);
        assert_eq!(deterministic_wallet_type(signer, signer), Some(0));
    }

    #[test]
    fn deterministic_safe_is_not_misclassified_as_a_deposit_wallet() {
        let signer = Address::from([0x22; 20]);
        let safe =
            derive_safe_wallet(signer, POLYGON).expect("Polygon Safe derivation is supported");
        assert_eq!(deterministic_wallet_type(signer, safe), Some(2));
    }

    #[test]
    fn deterministic_proxy_uses_the_legacy_proxy_signature_type() {
        let signer = Address::from([0x33; 20]);
        let proxy =
            derive_proxy_wallet(signer, POLYGON).expect("Polygon proxy derivation is supported");
        assert_eq!(deterministic_wallet_type(signer, proxy), Some(1));
    }

    #[test]
    fn serialized_config_never_contains_secret_fields() {
        let config = ConsumerConfig {
            version: CONFIG_VERSION,
            signer_address: "0x0000000000000000000000000000000000000001".to_string(),
            funder_address: "0x0000000000000000000000000000000000000002".to_string(),
            signature_type: 2,
            bind: DEFAULT_BIND.to_string(),
            allow_web_trading: false,
        };
        let json = serde_json::to_string(&config).expect("serialize");
        assert!(!json.contains("private_key"));
        assert!(!json.contains("control_token"));
    }
}

use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use alloy::primitives::Address;
use alloy::signers::Signer as _;
use alloy::signers::local::PrivateKeySigner;
use anyhow::{Context, Result, anyhow, bail};
use directories::ProjectDirs;
use indicatif::{ProgressBar, ProgressStyle};
use keyring::Entry;
use polymarket_client_sdk_v2::{POLYGON, derive_proxy_wallet, derive_safe_wallet};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::Zeroize;

use crate::app;
use crate::config::{
    DEFAULT_POLYGON_RPC_URL, GAMMA_API_URL, RELAYER_API_URL, ServeArgs, TradingArgs,
};
use crate::trading::{TradingRuntimeConfig, validate_trading_config};

const CONFIG_VERSION: u8 = 1;
const DEFAULT_BIND: &str = "127.0.0.1:9878";
const VAULT_SERVICE: &str = "xyz.polytread.cli";
const PRIVATE_KEY_VAULT_NAME: &str = "trading-private-key";
const CONTROL_TOKEN_VAULT_NAME: &str = "local-control-token";

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
    let config = match read_config()? {
        Some(config) => config,
        None => setup(false).await?,
    };
    let args = serve_args(&config)?;
    app::run(args).await
}

pub async fn setup(force: bool) -> Result<ConsumerConfig> {
    if let Some(existing) = read_config()?
        && !force
    {
        println!(
            "PolyTread is already configured for {} ({}).",
            existing.funder_address,
            wallet_type_label(existing.signature_type)
        );
        println!("Run `polytread setup --force` to replace the local setup.");
        return Ok(existing);
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!(
            "first-run setup requires an interactive terminal; run `polytread` directly in a terminal"
        );
    }

    println!("PolyTread secure setup");
    println!("----------------------");
    println!(
        "Your trading private key is entered with hidden input and saved only in your operating-system credential vault."
    );
    println!(
        "It is never written to the PolyTread config, dashboard, history files, or command line."
    );
    println!();

    let mut private_key = rpassword::prompt_password("Trading private key (hidden): ")
        .context("failed to read the hidden private key")?;
    let normalized = private_key.trim().to_owned();
    private_key.zeroize();
    private_key = normalized;
    if private_key.is_empty() {
        bail!("the private key cannot be empty");
    }

    let signer = PrivateKeySigner::from_str(&private_key)
        .context("the private key is not a valid Polygon signing key")?
        .with_chain_id(Some(POLYGON));
    let signer_address = signer.address();
    println!("Derived signer: {signer_address}");

    let client = Client::builder()
        .timeout(Duration::from_secs(12))
        .build()
        .context("failed to create the setup HTTP client")?;
    let spinner = progress_spinner("Discovering the Polymarket funding wallet...");
    let discovered_funder = match fetch_profile_funder(&client, signer_address).await {
        Ok(value) => value,
        Err(error) => {
            spinner.finish_with_message("Automatic profile lookup was unavailable.");
            eprintln!("Profile lookup detail: {error}");
            None
        }
    };
    if !spinner.is_finished() {
        spinner.finish_with_message("Polymarket profile lookup complete.");
    }

    let funder_address = match discovered_funder {
        Some(address) => {
            println!("Discovered funding wallet: {address}");
            address
        }
        None => {
            let entered = prompt_line(&format!(
                "Funding wallet (press Enter if it is the signer {signer_address}): "
            ))?;
            if entered.trim().is_empty() {
                signer_address
            } else {
                Address::from_str(entered.trim()).context("invalid funding wallet address")?
            }
        }
    };

    let spinner = progress_spinner("Detecting the wallet type...");
    let signature_type = match detect_wallet_type(&client, signer_address, funder_address).await {
        Ok(value) => {
            spinner.finish_with_message(format!("Detected {} wallet.", wallet_type_label(value)));
            value
        }
        Err(error) => {
            spinner.finish_with_message("Automatic wallet-type detection was inconclusive.");
            eprintln!("Detection detail: {error}");
            prompt_wallet_type()?
        }
    };

    let runtime = TradingRuntimeConfig {
        signer_address: signer_address.to_string(),
        funder_address: funder_address.to_string(),
        private_key: private_key.clone(),
        signature_type,
    };
    let spinner = progress_spinner("Authenticating and checking the pUSD wallet balance...");
    let validation = match validate_trading_config(&runtime).await {
        Ok(validation) => {
            spinner.finish_with_message("Credentials authenticated and wallet balance checked.");
            validation
        }
        Err(error) => {
            spinner.finish_with_message("Credential validation failed.");
            private_key.zeroize();
            return Err(error).context(
                "setup stopped before saving anything; verify the funding wallet and wallet type",
            );
        }
    };
    println!("Wallet type: {}", wallet_type_label(signature_type));
    println!("Available pUSD: ${:.4}", validation.available_pusd);
    println!(
        "Trading allowance: ${:.4} standard / ${:.4} negative-risk",
        validation.regular_allowance_pusd, validation.neg_risk_allowance_pusd
    );
    if validation.available_pusd <= 0.0 {
        println!(
            "The credentials are valid, but the wallet currently has no available pUSD; buys will remain blocked until it is funded."
        );
    }
    println!();
    println!("Browser trading is safety-locked by default.");
    let trading_confirmation =
        prompt_line("Type ENABLE to allow manual browser orders, or press Enter for view-only: ")?;
    let allow_web_trading = trading_confirmation.trim() == "ENABLE";

    let config = ConsumerConfig {
        version: CONFIG_VERSION,
        signer_address: signer_address.to_string(),
        funder_address: funder_address.to_string(),
        signature_type,
        bind: DEFAULT_BIND.to_string(),
        allow_web_trading,
    };
    validate_config(&config)?;
    let control_token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    vault_entry(PRIVATE_KEY_VAULT_NAME)?
        .set_password(&private_key)
        .context("the operating-system credential vault refused the private key")?;
    private_key.zeroize();
    vault_entry(CONTROL_TOKEN_VAULT_NAME)?
        .set_password(&control_token)
        .context("the operating-system credential vault refused the local control token")?;
    write_config(&config)?;

    println!();
    println!("Setup complete.");
    println!(
        "Dashboard trading: {}",
        if allow_web_trading {
            "enabled (still requires arming in the dashboard)"
        } else {
            "view-only"
        }
    );
    Ok(config)
}

pub async fn shutdown() -> Result<()> {
    let config = read_config()?.ok_or_else(|| anyhow!("PolyTread has not been set up yet"))?;
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
    println!("Graceful shutdown requested.");
    Ok(())
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
    })
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
    if signer == funder {
        return Ok(0);
    }
    if relayer_reports_deployed(client, funder, "WALLET").await? {
        return Ok(3);
    }
    if relayer_reports_deployed(client, funder, "SAFE").await? {
        return Ok(2);
    }
    if derive_safe_wallet(signer, POLYGON).is_some_and(|candidate| candidate == funder) {
        return Ok(2);
    }
    if derive_proxy_wallet(signer, POLYGON).is_some_and(|candidate| candidate == funder) {
        return Ok(1);
    }
    bail!("the funding wallet does not match a detectable EOA, proxy, Safe, or deposit wallet")
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

fn prompt_wallet_type() -> Result<u8> {
    println!("Select the wallet type shown by your Polymarket account:");
    println!("  1 = legacy Polymarket proxy");
    println!("  2 = Gnosis Safe");
    println!("  3 = deposit wallet (POLY_1271)");
    let value = prompt_line("Wallet type [1/2/3]: ")?;
    match value.trim() {
        "1" => Ok(1),
        "2" => Ok(2),
        "3" => Ok(3),
        _ => bail!("wallet type must be 1, 2, or 3"),
    }
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

fn progress_spinner(message: impl Into<String>) -> ProgressBar {
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .expect("static spinner template is valid")
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    spinner.set_message(message.into());
    spinner.enable_steady_tick(Duration::from_millis(90));
    spinner
}

#[cfg(test)]
mod tests {
    use super::*;

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

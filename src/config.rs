use std::fmt;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

pub const CLOB_API_URL: &str = "https://clob.polymarket.com";
pub const DATA_API_URL: &str = "https://data-api.polymarket.com";
pub const GAMMA_API_URL: &str = "https://gamma-api.polymarket.com";
pub const RELAYER_API_URL: &str = "https://relayer-v2.polymarket.com";
pub const DEFAULT_POLYGON_RPC_URL: &str = "https://polygon.drpc.org";
pub const RTDS_URL: &str = "wss://ws-live-data.polymarket.com";
pub const MARKET_WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";
pub const BINANCE_SPOT_WS_URLS: [&str; 3] = [
    "wss://data-stream.binance.vision/ws/btcusdt@aggTrade",
    "wss://stream.binance.com:443/ws/btcusdt@aggTrade",
    "wss://stream.binance.com:9443/ws/btcusdt@aggTrade",
];

fn default_data_dir() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("data")
}

#[derive(Debug, Parser)]
#[command(
    name = "polytread",
    about = "Lightweight browser-backed Polymarket BTC 5-minute trading service"
)]
pub struct Cli {
    #[arg(long, global = true, env = "POLYTREAD_LOG", default_value = "info")]
    pub log_filter: String,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Configure credentials in the operating-system credential vault.
    Setup(SetupArgs),
    /// Gracefully stop the locally running PolyTread service.
    Shutdown,
    /// Check whether the local PolyTread service is running.
    Status,
    /// Run the local browser dashboard and trading backend.
    Serve(ServeArgs),
    /// Submit one explicitly configured live order for operator validation.
    TradeSmoke(TradeSmokeArgs),
}

#[derive(Clone, Args)]
pub struct TradingArgs {
    /// Polygon address that signs orders.
    #[arg(long, env = "PM_SIGNER_ADDRESS")]
    pub signer_address: Option<String>,

    /// Wallet or proxy address that holds funds.
    #[arg(long, env = "PM_FUNDER_ADDRESS")]
    pub funder_address: Option<String>,

    /// Signing key, populated internally or from PM_PRIVATE_KEY.
    ///
    /// It is intentionally not accepted as a command-line argument because command
    /// lines can be visible to other local processes.
    #[arg(skip)]
    pub private_key: Option<String>,

    /// Polymarket signature type: 0=EOA, 1=POLY_PROXY, 2=GNOSIS_SAFE, 3=POLY_1271.
    #[arg(long, env = "PM_SIGNATURE_TYPE")]
    pub signature_type: Option<u8>,
}

impl fmt::Debug for TradingArgs {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TradingArgs")
            .field("signer_address", &self.signer_address)
            .field("funder_address", &self.funder_address)
            .field(
                "private_key",
                &self.private_key.as_ref().map(|_| "<redacted>"),
            )
            .field("signature_type", &self.signature_type)
            .finish()
    }
}

#[derive(Clone, Args)]
pub struct ServeArgs {
    /// HTTP and WebSocket listener. Loopback is the safe default.
    #[arg(long, env = "POLYTREAD_BIND", default_value = "127.0.0.1:9878")]
    pub bind: String,

    /// Directory for simple session, trade, and one-second price history.
    #[arg(long, env = "POLYTREAD_DATA_DIR", default_value_os_t = default_data_dir())]
    pub data_dir: PathBuf,

    /// Allow browser-originated live orders. Disabled by default.
    #[arg(long, env = "POLYTREAD_ALLOW_WEB_TRADING", default_value_t = false)]
    pub allow_web_trading: bool,

    /// Number of one-second samples retained in the live browser snapshot.
    #[arg(long, default_value_t = 600)]
    pub history_seconds: usize,

    #[arg(long, default_value_t = 15)]
    pub discovery_poll_seconds: u64,

    #[arg(long, default_value_t = 5)]
    pub websocket_heartbeat_seconds: u64,

    /// Optional bounded runtime, primarily for local validation.
    #[arg(long)]
    pub duration_seconds: Option<u64>,

    /// Bearer token for the loopback-only shutdown endpoint. Consumer mode injects it
    /// from the operating-system credential vault; advanced serve mode leaves it off.
    #[arg(skip)]
    pub control_token: Option<String>,

    /// Polygon RPC used only for an explicitly confirmed EOA claim.
    #[arg(skip = DEFAULT_POLYGON_RPC_URL.to_string())]
    pub polygon_rpc_url: String,

    #[command(flatten)]
    pub trading: TradingArgs,
}

impl fmt::Debug for ServeArgs {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServeArgs")
            .field("bind", &self.bind)
            .field("data_dir", &self.data_dir)
            .field("allow_web_trading", &self.allow_web_trading)
            .field("history_seconds", &self.history_seconds)
            .field("discovery_poll_seconds", &self.discovery_poll_seconds)
            .field(
                "websocket_heartbeat_seconds",
                &self.websocket_heartbeat_seconds,
            )
            .field("duration_seconds", &self.duration_seconds)
            .field(
                "control_token",
                &self.control_token.as_ref().map(|_| "<redacted>"),
            )
            .field("polygon_rpc_url", &self.polygon_rpc_url)
            .field("trading", &self.trading)
            .finish()
    }
}

#[derive(Debug, Clone, Args)]
pub struct SetupArgs {
    /// Replace an existing local setup after another explicit confirmation.
    #[arg(long, default_value_t = false)]
    pub force: bool,
}

#[derive(Debug, Clone, Args)]
pub struct TradeSmokeArgs {
    #[command(flatten)]
    pub trading: TradingArgs,

    #[arg(long)]
    pub slug: String,

    #[arg(long)]
    pub market_query: Option<String>,

    #[arg(long, default_value = "yes")]
    pub outcome: String,

    #[arg(long, default_value = "buy")]
    pub side: String,

    #[arg(long, default_value = "fast-taker")]
    pub mechanism: String,

    #[arg(long, default_value_t = 1.0)]
    pub nominal_usd: f64,
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::{Cli, Command};

    #[test]
    fn cli_definition_has_unique_arguments() {
        Cli::command().debug_assert();
    }

    #[test]
    fn serve_defaults_are_local_and_non_trading() {
        let cli = Cli::try_parse_from(["polytread", "serve"]).expect("parse defaults");
        let Some(Command::Serve(args)) = cli.command else {
            panic!("expected serve command");
        };
        assert_eq!(args.bind, "127.0.0.1:9878");
        assert!(!args.allow_web_trading);
        assert_eq!(args.history_seconds, 600);
        assert!(args.control_token.is_none());
    }

    #[test]
    fn consumer_start_requires_no_subcommand() {
        let cli = Cli::try_parse_from(["polytread"]).expect("parse consumer start");
        assert!(cli.command.is_none());
    }

    #[test]
    fn private_key_is_not_a_cli_argument() {
        let error =
            Cli::try_parse_from(["polytread", "serve", "--private-key", "do-not-accept-this"])
                .expect_err("private keys must never be accepted on the command line");
        assert!(error.to_string().contains("unexpected argument"));
    }

    #[test]
    fn debug_output_redacts_runtime_secrets() {
        let mut cli = Cli::try_parse_from(["polytread", "serve"]).expect("parse defaults");
        let Some(Command::Serve(args)) = cli.command.as_mut() else {
            panic!("expected serve command");
        };
        args.trading.private_key = Some("private-test-value".to_string());
        args.control_token = Some("control-test-value".to_string());
        let debug = format!("{cli:?}");
        assert!(!debug.contains("private-test-value"));
        assert!(!debug.contains("control-test-value"));
        assert!(debug.contains("<redacted>"));
    }
}

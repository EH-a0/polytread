mod app;
mod config;
mod connectivity;
mod consumer;
mod discovery;
mod dns_remediation;
mod feeds;
mod history;
mod local_control;
mod portfolio;
mod runtime_ui;
mod setup_ui;
mod state;
mod trading;
mod ws_dashboard;

use anyhow::{Result, anyhow};
use clap::Parser;
use config::{Cli, Command};

#[tokio::main]
async fn main() -> Result<()> {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .map_err(|_| anyhow!("failed to install the process TLS crypto provider"))?;
    }
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(cli.log_filter)
        .with_target(false)
        .compact()
        .with_writer(setup_ui::TuiSafeStderr)
        .init();

    match cli.command {
        None => consumer::start().await,
        Some(Command::Setup(args)) => consumer::setup_and_start(args.force).await,
        Some(Command::Shutdown) => consumer::shutdown().await,
        Some(Command::Status) => consumer::status().await,
        Some(Command::Diagnose) => consumer::diagnose().await,
        Some(Command::RestoreDns) => consumer::restore_dns().await,
        Some(Command::BackgroundWorker) => consumer::background_worker().await,
        Some(Command::Serve(mut args)) => {
            trading::inject_private_key_from_env(&mut args.trading)?;
            app::run(args).await
        }
        Some(Command::TradeSmoke(mut args)) => {
            trading::inject_private_key_from_env(&mut args.trading)?;
            trading::run_smoke(args).await
        }
    }
}

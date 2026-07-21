use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use reqwest::Client;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::time::MissedTickBehavior;
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::ServeArgs;
use crate::connectivity;
use crate::discovery;
use crate::feeds::{binance_spot, market, rtds_chainlink};
use crate::history::HistoryStore;
use crate::portfolio::{ClaimIntent, PortfolioState, run_portfolio_task};
use crate::runtime_ui::{RuntimeAction, RuntimeLogLevel, RuntimeUi};
use crate::state::{AppEvent, AppState, now_ms};
use crate::trading::{
    MIN_MAKER_SESSION_REMAINING_MS, NOMINAL_VALUES, OrderSide, TradeIntent, TradeSide,
    TradingMechanism, run_trading_task, runtime_config_from_args,
};
use crate::ws_dashboard::{
    DashboardCmd, DashboardCommand, DashboardControl, DashboardServer, Mechanism as WebMechanism,
    TradeSide as WebTradeSide,
};

pub async fn run(args: ServeArgs) -> Result<()> {
    run_inner(args, None).await
}

pub async fn run_with_ui(args: ServeArgs, ui: &mut RuntimeUi) -> Result<()> {
    run_inner(args, Some(ui)).await
}

async fn run_inner(mut args: ServeArgs, mut runtime_ui: Option<&mut RuntimeUi>) -> Result<()> {
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("failed to build HTTP client")?;
    let trading_config = runtime_config_from_args(&mut args.trading)?;
    let (mut history, past_sessions, price_history) = HistoryStore::open(&args.data_dir).await?;
    let portfolio_state = trading_config
        .as_ref()
        .map_or_else(PortfolioState::default, |config| {
            PortfolioState::configured(config.signature_type)
        });
    let mut state = AppState::new(
        args.history_seconds.clamp(60, 3_600),
        args.allow_web_trading,
        trading_config.is_some(),
        past_sessions,
        price_history,
        portfolio_state,
    );

    let (event_tx, mut event_rx) = mpsc::channel::<AppEvent>(4_096);
    let (asset_tx, asset_rx) = watch::channel(Vec::<String>::new());
    let (shutdown_tx, _) = broadcast::channel::<()>(8);
    let (snapshot_tx, _) = broadcast::channel::<serde_json::Value>(64);
    let dashboard_token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let dashboard_run_id = Uuid::new_v4().simple();
    let dashboard_url_token = dashboard_token.clone();
    let control_enabled = args.control_token.is_some();
    let (dashboard, mut command_rx, mut control_rx) = DashboardServer::new(
        snapshot_tx.clone(),
        args.control_token.take(),
        dashboard_token,
    );
    let (ready_tx, ready_rx) = oneshot::channel();
    let mut dashboard_task = tokio::spawn({
        let bind = args.bind.clone();
        async move { dashboard.run(&bind, ready_tx).await }
    });
    let local_addr = tokio::select! {
        ready = ready_rx => ready.context("dashboard stopped before opening its listener")?,
        dashboard_result = &mut dashboard_task => {
            match dashboard_result {
                Ok(Ok(())) => return Err(anyhow!("dashboard server stopped unexpectedly")),
                Ok(Err(error)) => return Err(error).context("dashboard server failed"),
                Err(error) => return Err(error).context("dashboard task failed"),
            }
        }
    };
    let dashboard_url =
        format!("http://{local_addr}/?run={dashboard_run_id}#access={dashboard_url_token}");
    if let Some(ui) = runtime_ui.as_deref_mut() {
        ui.dashboard_ready(dashboard_url)?;
    } else {
        println!("PolyTread dashboard: {dashboard_url}");
        println!("This local access link rotates whenever PolyTread restarts.");
        if control_enabled {
            println!("Stop safely from another terminal with: polytread shutdown");
        }
    }

    let mut tasks = vec![
        tokio::spawn(binance_spot::run(
            event_tx.clone(),
            shutdown_tx.subscribe(),
            args.websocket_heartbeat_seconds,
        )),
        tokio::spawn(rtds_chainlink::run(
            event_tx.clone(),
            shutdown_tx.subscribe(),
            args.websocket_heartbeat_seconds,
        )),
        tokio::spawn(market::run(
            event_tx.clone(),
            asset_rx,
            shutdown_tx.subscribe(),
            args.websocket_heartbeat_seconds,
        )),
        tokio::spawn(run_discovery(
            client.clone(),
            event_tx.clone(),
            shutdown_tx.subscribe(),
            args.discovery_poll_seconds,
        )),
        tokio::spawn(connectivity::run_monitor(
            client.clone(),
            event_tx.clone(),
            shutdown_tx.subscribe(),
        )),
    ];
    if let Some(ui) = runtime_ui.as_deref_mut() {
        ui.push_log(
            RuntimeLogLevel::Info,
            "Market feeds, discovery, and connectivity monitors started",
        );
    }

    let mut trading_order_tx = None;
    let mut claim_tx = None;
    if let Some(config) = trading_config {
        let portfolio_config = config.clone();
        let (order_tx, order_rx) = mpsc::channel(64);
        let (trading_event_tx, mut trading_event_rx) = mpsc::channel(512);
        let forward_tx = event_tx.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(event) = trading_event_rx.recv().await {
                if forward_tx.send(AppEvent::Trading(event)).await.is_err() {
                    break;
                }
            }
        }));
        tasks.push(tokio::spawn(run_trading_task(
            config,
            trading_event_tx,
            shutdown_tx.subscribe(),
            order_rx,
            args.data_dir.clone(),
        )));
        trading_order_tx = Some(order_tx);

        let (portfolio_claim_tx, portfolio_claim_rx) = mpsc::channel(8);
        tasks.push(tokio::spawn(run_portfolio_task(
            portfolio_config,
            event_tx.clone(),
            shutdown_tx.subscribe(),
            portfolio_claim_rx,
            args.data_dir.clone(),
            args.polygon_rpc_url.clone(),
        )));
        claim_tx = Some(portfolio_claim_tx);
        if let Some(ui) = runtime_ui.as_deref_mut() {
            ui.push_log(
                RuntimeLogLevel::Info,
                "Trading and portfolio services started in disarmed mode",
            );
        }
    }

    let mut sample_tick = tokio::time::interval(Duration::from_secs(1));
    sample_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut snapshot_tick = tokio::time::interval(Duration::from_millis(500));
    snapshot_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut runtime_ui_tick = tokio::time::interval(crate::runtime_ui::FRAME_INTERVAL);
    runtime_ui_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let deadline = args
        .duration_seconds
        .map(|seconds| tokio::time::Instant::now() + Duration::from_secs(seconds));
    let deadline_future = async move {
        match deadline {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(deadline_future);

    info!(
        bind = %args.bind,
        data_dir = %args.data_dir.display(),
        web_trading = args.allow_web_trading,
        trading_configured = state.trading().configured,
        "PolyTread lightweight service started"
    );

    loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal.context("failed waiting for Ctrl-C")?;
                if let Some(ui) = runtime_ui.as_deref_mut() {
                    ui.push_log(RuntimeLogLevel::Info, "Ctrl+C received; stopping safely");
                }
                break;
            }
            _ = &mut deadline_future => {
                if let Some(ui) = runtime_ui.as_deref_mut() {
                    ui.push_log(RuntimeLogLevel::Info, "Configured runtime duration completed");
                }
                break;
            },
            Some(control) = control_rx.recv() => {
                match control {
                    DashboardControl::Shutdown => {
                        info!("authenticated local shutdown requested");
                        if let Some(ui) = runtime_ui.as_deref_mut() {
                            ui.push_log(RuntimeLogLevel::Info, "Authenticated shutdown requested");
                        }
                        break;
                    }
                }
            }
            dashboard_result = &mut dashboard_task => {
                match dashboard_result {
                    Ok(Ok(())) => return Err(anyhow!("dashboard server stopped unexpectedly")),
                    Ok(Err(error)) => return Err(error).context("dashboard server failed"),
                    Err(error) => return Err(error).context("dashboard task failed"),
                }
            }
            Some(event) = event_rx.recv() => {
                if let Some(ui) = runtime_ui.as_deref_mut() {
                    ui.observe_event(&event);
                }
                let effects = state.apply(event);
                if let Some(assets) = effects.market_assets {
                    asset_tx.send_replace(assets);
                }
                if let Some(record) = effects.observed_session {
                    match history.record_session(&record).await {
                        Ok(true) => state.add_past_session(record),
                        Ok(false) => {}
                        Err(error) => warn!(%error, "failed writing session history"),
                    }
                }
            }
            Some(command) = command_rx.recv() => {
                let is_claim = matches!(&command.command, DashboardCmd::ClaimPosition { .. });
                if let Err(error) = handle_command(
                    command,
                    &args,
                    &mut state,
                    trading_order_tx.as_ref(),
                    claim_tx.as_ref(),
                ).await {
                    if is_claim {
                        state.portfolio_mut().claim_status = format!("Claim rejected: {error}");
                        state.portfolio_mut().last_error = Some(error.to_string());
                    } else {
                        state.trading_mut().order_status = format!("Rejected: {error}");
                        state.trading_mut().last_error = Some(error.to_string());
                    }
                }
            }
            _ = sample_tick.tick() => {
                if let Some(sample) = state.sample_price(now_ms())
                    && let Err(error) = history.record_price(&sample).await
                {
                    warn!(%error, "failed writing one-second price history");
                }
            }
            _ = snapshot_tick.tick() => {
                let snapshot = serde_json::to_value(state.snapshot())
                    .context("failed serializing dashboard snapshot")?;
                let _ = snapshot_tx.send(snapshot);
            }
            _ = runtime_ui_tick.tick(), if runtime_ui.is_some() => {
                if runtime_ui
                    .as_deref_mut()
                    .expect("runtime UI exists when its select branch is enabled")
                    .tick_status()?
                    == RuntimeAction::Shutdown
                {
                    break;
                }
            }
        }
    }

    let _ = shutdown_tx.send(());
    dashboard_task.abort();
    for task in tasks {
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
    }
    if let Some(ui) = runtime_ui {
        ui.push_log(RuntimeLogLevel::Success, "PolyTread stopped safely");
    }
    info!("PolyTread service stopped");
    Ok(())
}

async fn run_discovery(
    client: Client,
    event_tx: mpsc::Sender<AppEvent>,
    mut shutdown: broadcast::Receiver<()>,
    poll_seconds: u64,
) {
    loop {
        match discovery::fetch_sessions_at(&client, "btc up or down 5 minutes", 3, now_ms()).await {
            Ok(update) => {
                if event_tx.send(AppEvent::Discovery(update)).await.is_err() {
                    return;
                }
            }
            Err(error) => warn!(%error, "market discovery failed; keeping current session"),
        }
        tokio::select! {
            _ = shutdown.recv() => return,
            _ = tokio::time::sleep(Duration::from_secs(poll_seconds.max(5))) => {}
        }
    }
}

fn map_web_buy_side(
    session: &crate::state::SessionDescriptor,
    side: WebTradeSide,
) -> (TradeSide, String, String, &'static str) {
    match side {
        WebTradeSide::BuyUp => (
            TradeSide::BuyUp,
            session.up_token_id.clone(),
            session.down_token_id.clone(),
            "UP",
        ),
        WebTradeSide::BuyDown => (
            TradeSide::BuyDown,
            session.down_token_id.clone(),
            session.up_token_id.clone(),
            "DOWN",
        ),
    }
}

async fn handle_command(
    command: DashboardCommand,
    args: &ServeArgs,
    state: &mut AppState,
    order_tx: Option<&mpsc::Sender<TradeIntent>>,
    claim_tx: Option<&mpsc::Sender<ClaimIntent>>,
) -> Result<()> {
    match command.command {
        DashboardCmd::SetEnabled { enabled } => {
            if enabled && !args.allow_web_trading {
                return Err(anyhow!(
                    "browser trading is disabled by server configuration"
                ));
            }
            if enabled && !state.trading().configured {
                return Err(anyhow!(
                    "the complete trading credential set is not configured"
                ));
            }
            if state.trading().enabled != enabled {
                state.trading_mut().toggle_enabled();
            }
            Ok(())
        }
        DashboardCmd::SubmitOrder {
            side,
            nominal_usd,
            mechanism,
            expected_session_slug,
        } => {
            if !args.allow_web_trading {
                return Err(anyhow!(
                    "browser trading is disabled by server configuration"
                ));
            }
            let order_tx =
                order_tx.ok_or_else(|| anyhow!("trading credentials are unavailable"))?;
            if !NOMINAL_VALUES
                .iter()
                .any(|allowed| (*allowed - nominal_usd).abs() < f64::EPSILON)
            {
                return Err(anyhow!("nominal must be one of {NOMINAL_VALUES:?}"));
            }
            let session = state
                .current_session()
                .cloned()
                .ok_or_else(|| anyhow!("no live market session is available"))?;
            if session.slug != expected_session_slug {
                return Err(anyhow!(
                    "session changed from {expected_session_slug} to {}; refresh before ordering",
                    session.slug
                ));
            }
            let command_now_ms = now_ms();
            if command_now_ms >= session.end_ms {
                return Err(anyhow!("session {} has ended", session.slug));
            }
            if !state.trading().enabled {
                return Err(anyhow!("trading must be armed before submitting an order"));
            }

            let (trade_side, token_id, complement_token_id, outcome_label) =
                map_web_buy_side(&session, side);
            let mechanism = match mechanism {
                WebMechanism::Taker => TradingMechanism::FastTaker,
                WebMechanism::Maker => TradingMechanism::FastMaker,
            };
            if matches!(mechanism, TradingMechanism::FastMaker)
                && session.end_ms - command_now_ms < MIN_MAKER_SESSION_REMAINING_MS
            {
                return Err(anyhow!(
                    "Fast Maker is unavailable with less than {} seconds left; use Fast Taker or wait for the next session",
                    MIN_MAKER_SESSION_REMAINING_MS / 1_000
                ));
            }
            let minimum_nominal = state
                .minimum_buy_nominal(trade_side, mechanism)
                .ok_or_else(|| {
                    anyhow!(
                        "live order constraints for {outcome_label} are unavailable; wait for a fresh order book"
                    )
                })?;
            if nominal_usd + 1e-9 < minimum_nominal {
                return Err(anyhow!(
                    "{outcome_label} requires at least ${minimum_nominal:.2} for {} at the current book; choose a larger amount",
                    mechanism.label()
                ));
            }
            state.trading_mut().set_nominal(nominal_usd);
            state.trading_mut().set_mechanism(mechanism);
            state.trading_mut().set_side(trade_side);
            if !state.trading().is_ready() {
                return Err(anyhow!(
                    "trading preflight is not ready: {}",
                    state.trading().order_status
                ));
            }

            let fingerprint = format!(
                "{}:{}:{nominal_usd:.2}:{}",
                session.slug,
                trade_side.label(),
                mechanism.label()
            );
            if state
                .trading()
                .duplicate_fingerprint_locked(&fingerprint, &session.slug)
            {
                return Err(anyhow!("the same order intent is already in flight"));
            }
            let local_id = Uuid::new_v4().to_string();
            let intent = TradeIntent {
                local_id: local_id.clone(),
                fingerprint: fingerprint.clone(),
                token_id,
                complement_token_id: Some(complement_token_id),
                trade_side,
                order_side: trade_side.order_side(),
                nominal_usd,
                mechanism,
                market_slug: session.slug.clone(),
                market_label: format!("{} {outcome_label}", session.title),
                session_end_ms: session.end_ms,
            };
            state.trading_mut().mark_intent_in_flight(
                local_id.clone(),
                fingerprint,
                session.slug,
                command_now_ms,
            );
            if let Err(error) = order_tx.try_send(intent) {
                state.trading_mut().clear_in_flight_if(&local_id);
                return Err(anyhow!("trading queue unavailable: {error}"));
            }
            state.trading_mut().order_status = format!(
                "Queued {} {} ${nominal_usd:.2}",
                match trade_side.order_side() {
                    OrderSide::Buy => "BUY",
                    OrderSide::Sell => "SELL",
                },
                outcome_label
            );
            Ok(())
        }
        DashboardCmd::ClaimPosition {
            condition_id,
            expected_redeemable_value_usd,
        } => {
            if !expected_redeemable_value_usd.is_finite() || expected_redeemable_value_usd < 0.0 {
                return Err(anyhow!("the expected claim value is invalid"));
            }
            if !state.portfolio().manual_claim_only {
                return Err(anyhow!("automatic claims are not supported"));
            }
            if !state.portfolio().direct_claim_supported {
                return Err(anyhow!(
                    "direct local claims are unavailable for {}; use the Polymarket Portfolio link",
                    state.portfolio().wallet_type
                ));
            }
            if state.portfolio().claim_in_flight_condition_id.is_some() {
                return Err(anyhow!("another manual claim is already in flight"));
            }
            let position = state
                .portfolio()
                .claimable_positions
                .iter()
                .find(|position| position.condition_id.eq_ignore_ascii_case(&condition_id))
                .ok_or_else(|| anyhow!("the position is no longer claimable"))?;
            if (position.redeemable_value_usd - expected_redeemable_value_usd).abs() > 0.01 {
                return Err(anyhow!(
                    "the claim value changed; refresh and confirm again"
                ));
            }
            let claim_tx = claim_tx.ok_or_else(|| anyhow!("claim service is unavailable"))?;
            let intent = ClaimIntent {
                condition_id: position.condition_id.clone(),
                expected_redeemable_value_usd,
            };
            claim_tx
                .try_send(intent)
                .map_err(|error| anyhow!("claim queue unavailable: {error}"))?;
            state.portfolio_mut().claim_in_flight_condition_id = Some(condition_id);
            state.portfolio_mut().claim_status = "Manual claim queued".to_string();
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_public_nominals_are_finite_and_positive() {
        assert!(
            NOMINAL_VALUES
                .iter()
                .all(|value| value.is_finite() && *value >= crate::trading::MIN_BUY_ORDER_USD)
        );
        assert_eq!(
            NOMINAL_VALUES.first().copied(),
            Some(crate::trading::MIN_BUY_ORDER_USD)
        );
        assert!(NOMINAL_VALUES.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn browser_buy_sides_map_to_the_correct_token_and_complement() {
        let session = crate::state::SessionDescriptor {
            slug: "btc-updown-5m-1".to_string(),
            title: "BTC Up or Down".to_string(),
            start_ms: 1_000,
            end_ms: 301_000,
            price_to_beat: Some(70_000.0),
            up_token_id: "up-token".to_string(),
            down_token_id: "down-token".to_string(),
            active: true,
            closed: false,
            minimum_order_size: Some(5.0),
            tick_size: Some(0.01),
        };

        assert_eq!(
            map_web_buy_side(&session, WebTradeSide::BuyUp),
            (
                TradeSide::BuyUp,
                "up-token".to_string(),
                "down-token".to_string(),
                "UP"
            )
        );
        assert_eq!(
            map_web_buy_side(&session, WebTradeSide::BuyDown),
            (
                TradeSide::BuyDown,
                "down-token".to_string(),
                "up-token".to_string(),
                "DOWN"
            )
        );
    }
}

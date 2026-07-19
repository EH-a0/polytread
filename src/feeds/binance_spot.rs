use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::{broadcast, mpsc};
use tokio::time::MissedTickBehavior;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

use crate::config::BINANCE_SPOT_WS_URLS;
use crate::state::{AppEvent, FeedKind, FeedStatusUpdate, PriceTick, now_ms};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const PROACTIVE_REFRESH_AFTER: Duration = Duration::from_secs((23 * 60 * 60) + (50 * 60));

#[derive(Debug, Deserialize)]
struct BinanceStreamEvent {
    #[serde(rename = "e")]
    event_type: String,
    #[serde(rename = "s", default)]
    symbol: String,
    #[serde(rename = "p", default)]
    price: Option<String>,
    #[serde(rename = "T", default)]
    trade_time_ms: Option<i64>,
}

enum ConnectionEnd {
    Shutdown,
    Reconnect(String),
}

pub async fn run(
    tx: mpsc::Sender<AppEvent>,
    mut shutdown: broadcast::Receiver<()>,
    heartbeat_seconds: u64,
) {
    let mut reconnects = 0_u64;
    let mut endpoint_index = 0_usize;

    loop {
        let endpoint = BINANCE_SPOT_WS_URLS[endpoint_index % BINANCE_SPOT_WS_URLS.len()];
        let connection = tokio::time::timeout(CONNECT_TIMEOUT, connect_async(endpoint));
        let result = tokio::select! {
            _ = shutdown.recv() => return,
            result = connection => result,
        };

        let end = match result {
            Ok(Ok((socket, _))) => {
                info!(
                    endpoint,
                    "connected to official Binance BTCUSDT Spot stream"
                );
                let _ = tx
                    .send(AppEvent::FeedStatus(FeedStatusUpdate {
                        feed: FeedKind::BinanceSpot,
                        connected: true,
                        reconnects,
                        last_message_ms: None,
                        last_error: None,
                        status: "connected_direct".to_string(),
                    }))
                    .await;
                run_connected(socket, &tx, &mut shutdown, heartbeat_seconds).await
            }
            Ok(Err(error)) => ConnectionEnd::Reconnect(format!("connect failed: {error}")),
            Err(_) => ConnectionEnd::Reconnect("connect timeout".to_string()),
        };

        match end {
            ConnectionEnd::Shutdown => return,
            ConnectionEnd::Reconnect(reason) => {
                warn!(endpoint, reason, "Binance Spot stream reconnecting");
                reconnects = reconnects.saturating_add(1);
                endpoint_index = (endpoint_index + 1) % BINANCE_SPOT_WS_URLS.len();
                let _ = tx
                    .send(AppEvent::FeedStatus(FeedStatusUpdate {
                        feed: FeedKind::BinanceSpot,
                        connected: false,
                        reconnects,
                        last_message_ms: None,
                        last_error: Some(reason),
                        status: "reconnecting".to_string(),
                    }))
                    .await;
            }
        }

        let delay = reconnect_delay(reconnects);
        tokio::select! {
            _ = shutdown.recv() => return,
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

async fn run_connected<S>(
    mut socket: tokio_tungstenite::WebSocketStream<S>,
    tx: &mpsc::Sender<AppEvent>,
    shutdown: &mut broadcast::Receiver<()>,
    heartbeat_seconds: u64,
) -> ConnectionEnd
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut watchdog = tokio::time::interval(Duration::from_secs(1));
    watchdog.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let stale_after = binance_stale_after(heartbeat_seconds);
    let mut last_data_message = Instant::now();
    let refresh = tokio::time::sleep(PROACTIVE_REFRESH_AFTER);
    tokio::pin!(refresh);

    loop {
        tokio::select! {
            _ = shutdown.recv() => return ConnectionEnd::Shutdown,
            _ = &mut refresh => {
                return ConnectionEnd::Reconnect("scheduled refresh before Binance 24-hour limit".to_string());
            }
            _ = watchdog.tick() => {
                if last_data_message.elapsed() > stale_after {
                    return ConnectionEnd::Reconnect(format!(
                        "no BTCUSDT aggregate trade for {} seconds",
                        last_data_message.elapsed().as_secs()
                    ));
                }
            }
            next = socket.next() => {
                match next {
                    Some(Ok(Message::Text(text))) => {
                        if is_server_shutdown(&text) {
                            return ConnectionEnd::Reconnect("Binance server shutdown notice".to_string());
                        }
                        if let Some(tick) = parse_price_tick(&text) {
                            last_data_message = Instant::now();
                            if tx.send(AppEvent::Price(tick)).await.is_err() {
                                return ConnectionEnd::Shutdown;
                            }
                        } else {
                            debug!(payload = %text, "ignored non-BTCUSDT Binance Spot message");
                        }
                    }
                    Some(Ok(Message::Ping(bytes))) => {
                        if socket.send(Message::Pong(bytes)).await.is_err() {
                            return ConnectionEnd::Reconnect("failed to answer Binance ping".to_string());
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(frame))) => {
                        return ConnectionEnd::Reconnect(format!("socket closed: {frame:?}"));
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        return ConnectionEnd::Reconnect(format!("socket error: {error}"));
                    }
                    None => return ConnectionEnd::Reconnect("socket ended".to_string()),
                }
            }
        }
    }
}

fn binance_stale_after(heartbeat_seconds: u64) -> Duration {
    Duration::from_secs(heartbeat_seconds.max(1).saturating_mul(3).max(15))
}

fn reconnect_delay(reconnects: u64) -> Duration {
    let exponent = reconnects.saturating_sub(1).min(4) as u32;
    let base_ms = 250_u64.saturating_mul(1_u64 << exponent).min(4_000);
    let jitter_ms = now_ms().unsigned_abs() % 251;
    Duration::from_millis(base_ms + jitter_ms)
}

fn is_server_shutdown(text: &str) -> bool {
    serde_json::from_str::<BinanceStreamEvent>(text)
        .map(|event| event.event_type == "serverShutdown")
        .unwrap_or(false)
}

fn parse_price_tick(text: &str) -> Option<PriceTick> {
    let recv_ms = now_ms();
    parse_price_tick_at(text, recv_ms)
}

fn parse_price_tick_at(text: &str, recv_ms: i64) -> Option<PriceTick> {
    let event: BinanceStreamEvent = serde_json::from_str(text).ok()?;
    if event.event_type != "aggTrade" || !event.symbol.eq_ignore_ascii_case("BTCUSDT") {
        return None;
    }
    let price = event.price.as_deref()?.parse::<f64>().ok()?;
    if !price.is_finite() || price <= 0.0 {
        return None;
    }
    let source_ts_ms = event.trade_time_ms.map(normalize_epoch_ms);

    Some(PriceTick {
        feed: FeedKind::BinanceSpot,
        recv_ms,
        source_ts_ms,
        value: price,
    })
}

fn normalize_epoch_ms(value: i64) -> i64 {
    match value.unsigned_abs() {
        0..=9_999_999_999 => value.saturating_mul(1_000),
        10_000_000_000..=9_999_999_999_999 => value,
        10_000_000_000_000..=9_999_999_999_999_999 => value / 1_000,
        _ => value / 1_000_000,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        binance_stale_after, is_server_shutdown, normalize_epoch_ms, parse_price_tick_at,
        reconnect_delay,
    };
    use crate::state::FeedKind;

    #[test]
    fn parses_official_btcusdt_aggregate_trade() {
        let message = r#"{
            "e":"aggTrade",
            "E":1774022788121,
            "s":"BTCUSDT",
            "a":12345,
            "p":"70001.25000000",
            "q":"0.01000000",
            "f":100,
            "l":105,
            "T":1774022788119,
            "m":true,
            "M":true
        }"#;
        let tick = parse_price_tick_at(message, 1_774_022_788_130).expect("tick");
        assert_eq!(tick.feed, FeedKind::BinanceSpot);
        assert_eq!(tick.value, 70_001.25);
        assert_eq!(tick.source_ts_ms, Some(1_774_022_788_119));
    }

    #[test]
    fn rejects_wrong_symbol_event_and_non_positive_price() {
        let wrong_symbol = r#"{"e":"aggTrade","E":1,"s":"ETHUSDT","p":"1","T":1}"#;
        let wrong_event = r#"{"e":"trade","E":1,"s":"BTCUSDT","p":"1","T":1}"#;
        let bad_price = r#"{"e":"aggTrade","E":1,"s":"BTCUSDT","p":"0","T":1}"#;
        assert!(parse_price_tick_at(wrong_symbol, 2).is_none());
        assert!(parse_price_tick_at(wrong_event, 2).is_none());
        assert!(parse_price_tick_at(bad_price, 2).is_none());
    }

    #[test]
    fn detects_server_shutdown_notice() {
        assert!(is_server_shutdown(
            r#"{"e":"serverShutdown","E":1770123456789}"#
        ));
    }

    #[test]
    fn stale_watchdog_scales_with_heartbeat_and_stays_fast() {
        assert_eq!(binance_stale_after(0), Duration::from_secs(15));
        assert_eq!(binance_stale_after(5), Duration::from_secs(15));
        assert_eq!(binance_stale_after(30), Duration::from_secs(90));
    }

    #[test]
    fn reconnect_backoff_is_bounded() {
        assert!(reconnect_delay(1) >= Duration::from_millis(250));
        assert!(reconnect_delay(100) <= Duration::from_millis(4_250));
    }

    #[test]
    fn timestamp_normalizer_accepts_seconds_and_microseconds() {
        assert_eq!(normalize_epoch_ms(1_774_022_788), 1_774_022_788_000);
        assert_eq!(normalize_epoch_ms(1_774_022_788_123_456), 1_774_022_788_123);
    }
}

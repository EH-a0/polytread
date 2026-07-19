use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{broadcast, mpsc};
use tokio::time::MissedTickBehavior;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

use crate::config::RTDS_URL;
use crate::state::{AppEvent, FeedKind, FeedStatusUpdate, PriceTick, now_ms};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const PROACTIVE_REFRESH_AFTER: Duration = Duration::from_secs(30 * 60);

enum ConnectionEnd {
    Shutdown,
    Reconnect(String),
}

#[derive(Debug, Deserialize)]
struct RtdsEnvelope {
    #[serde(default)]
    topic: String,
    #[serde(default)]
    payload: Option<RtdsPayload>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RtdsTimestamp {
    I64(i64),
    F64(f64),
    String(String),
}

impl RtdsTimestamp {
    fn as_i64(&self) -> Option<i64> {
        match self {
            Self::I64(v) => Some(*v),
            Self::String(s) => s.parse().ok(),
            Self::F64(v) => Some(*v as i64),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RtdsPayload {
    #[serde(default)]
    symbol: String,
    #[serde(default)]
    value: Option<RtdsNumber>,
    #[serde(default)]
    timestamp: Option<RtdsTimestamp>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RtdsNumber {
    I64(i64),
    F64(f64),
    String(String),
}

impl RtdsNumber {
    fn as_f64(&self) -> Option<f64> {
        match self {
            Self::F64(v) => Some(*v),
            Self::I64(v) => Some(*v as f64),
            Self::String(s) => s.parse().ok(),
        }
    }
}

pub async fn run(
    tx: mpsc::Sender<AppEvent>,
    mut shutdown: broadcast::Receiver<()>,
    heartbeat_seconds: u64,
) {
    let mut reconnects = 0_u64;
    loop {
        let connection = tokio::time::timeout(CONNECT_TIMEOUT, connect_async(RTDS_URL));
        let result = tokio::select! {
            _ = shutdown.recv() => return,
            result = connection => result,
        };

        let end = match result {
            Ok(Ok((socket, _))) => {
                info!(reconnects, "connected to Polymarket Chainlink RTDS stream");
                let _ = tx
                    .send(AppEvent::FeedStatus(FeedStatusUpdate {
                        feed: FeedKind::ChainlinkRtds,
                        connected: true,
                        reconnects,
                        last_message_ms: None,
                        last_error: None,
                        status: "connected".to_string(),
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
                warn!(reason, reconnects, "Chainlink RTDS stream reconnecting");
                reconnects = reconnects.saturating_add(1);
                let _ = tx
                    .send(AppEvent::FeedStatus(FeedStatusUpdate {
                        feed: FeedKind::ChainlinkRtds,
                        connected: false,
                        reconnects,
                        last_message_ms: None,
                        last_error: Some(reason),
                        status: "reconnecting".to_string(),
                    }))
                    .await;
            }
        }

        tokio::select! {
            _ = shutdown.recv() => return,
            _ = tokio::time::sleep(reconnect_delay(reconnects)) => {}
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
    let subscribe = json!({
        "action": "subscribe",
        "subscriptions": [
            {
                "topic": "crypto_prices_chainlink",
                "type": "*",
                "filters": "{\"symbol\":\"btc/usd\"}",
            }
        ]
    });
    if let Err(error) = socket
        .send(Message::Text(subscribe.to_string().into()))
        .await
    {
        return ConnectionEnd::Reconnect(format!("subscription send failed: {error}"));
    }

    let mut heartbeat = tokio::time::interval(Duration::from_secs(heartbeat_seconds.max(1)));
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut watchdog = tokio::time::interval(Duration::from_secs(1));
    watchdog.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let stale_after = rtds_stale_after(heartbeat_seconds);
    let mut last_fresh_tick = Instant::now();
    let refresh = tokio::time::sleep(PROACTIVE_REFRESH_AFTER);
    tokio::pin!(refresh);

    loop {
        tokio::select! {
            _ = shutdown.recv() => return ConnectionEnd::Shutdown,
            _ = &mut refresh => {
                return ConnectionEnd::Reconnect(
                    "scheduled refresh to rotate the RTDS backend".to_string()
                );
            }
            _ = watchdog.tick() => {
                if last_fresh_tick.elapsed() > stale_after {
                    return ConnectionEnd::Reconnect(format!(
                        "no fresh BTC/USD Chainlink tick for {} seconds",
                        last_fresh_tick.elapsed().as_secs()
                    ));
                }
            }
            _ = heartbeat.tick() => {
                if let Err(error) = socket.send(Message::Text("PING".into())).await {
                    return ConnectionEnd::Reconnect(format!("heartbeat send failed: {error}"));
                }
            }
            next = socket.next() => {
                match next {
                    Some(Ok(Message::Text(text))) => {
                        if let Some(tick) = parse_price_tick(&text, FeedKind::ChainlinkRtds) {
                            if !source_tick_is_fresh(&tick, stale_after) {
                                debug!(
                                    source_ts_ms = tick.source_ts_ms,
                                    recv_ms = tick.recv_ms,
                                    "ignored stale Chainlink RTDS tick"
                                );
                                continue;
                            }
                            last_fresh_tick = Instant::now();
                            if tx.send(AppEvent::Price(tick)).await.is_err() {
                                return ConnectionEnd::Shutdown;
                            }
                        }
                    }
                    Some(Ok(Message::Ping(bytes))) => {
                        if let Err(error) = socket.send(Message::Pong(bytes)).await {
                            return ConnectionEnd::Reconnect(format!("failed to answer RTDS ping: {error}"));
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

fn rtds_stale_after(heartbeat_seconds: u64) -> Duration {
    Duration::from_secs(heartbeat_seconds.max(1).saturating_mul(3).max(15))
}

fn source_tick_is_fresh(tick: &PriceTick, stale_after: Duration) -> bool {
    let Some(source_ts_ms) = tick.source_ts_ms else {
        return false;
    };
    let max_skew_ms = stale_after.as_millis().min(i64::MAX as u128) as i64;
    tick.recv_ms.abs_diff(source_ts_ms) <= max_skew_ms as u64
}

fn reconnect_delay(reconnects: u64) -> Duration {
    let exponent = reconnects.saturating_sub(1).min(4) as u32;
    let base_ms = 250_u64.saturating_mul(1_u64 << exponent).min(4_000);
    let jitter_ms = now_ms().unsigned_abs() % 251;
    Duration::from_millis(base_ms + jitter_ms)
}

fn parse_price_tick(text: &str, feed: FeedKind) -> Option<PriceTick> {
    if text == "PONG" {
        return None;
    }
    let recv_ms = now_ms();
    let envelope: RtdsEnvelope = serde_json::from_str(text).ok()?;
    if envelope.topic != "crypto_prices_chainlink" {
        debug!("ignoring RTDS topic {}", envelope.topic);
        return None;
    }
    let payload = envelope.payload.as_ref()?;
    if !payload.symbol.eq_ignore_ascii_case("btc/usd") {
        return None;
    }
    let price = payload.value.as_ref().and_then(RtdsNumber::as_f64)?;
    if !price.is_finite() || price <= 0.0 {
        return None;
    }
    let source_ts_ms = payload
        .timestamp
        .as_ref()
        .and_then(RtdsTimestamp::as_i64)
        .map(normalize_epoch_ms);
    Some(PriceTick {
        feed,
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
        normalize_epoch_ms, parse_price_tick, reconnect_delay, rtds_stale_after,
        source_tick_is_fresh,
    };
    use crate::state::{FeedKind, PriceTick};

    #[test]
    fn stale_watchdog_has_a_safe_minimum_and_scales_with_heartbeat() {
        assert_eq!(rtds_stale_after(0), Duration::from_secs(15));
        assert_eq!(rtds_stale_after(5), Duration::from_secs(15));
        assert_eq!(rtds_stale_after(30), Duration::from_secs(90));
    }

    #[test]
    fn parse_price_tick_normalizes_second_precision_timestamps() {
        let message = r#"{
            "topic":"crypto_prices_chainlink",
            "timestamp":1774022788,
            "payload":{"symbol":"btc/usd","timestamp":1774022788,"value":70002.5}
        }"#;
        let tick = parse_price_tick(message, FeedKind::ChainlinkRtds).expect("tick");
        assert_eq!(tick.source_ts_ms, Some(normalize_epoch_ms(1_774_022_788)));
    }

    #[test]
    fn source_freshness_rejects_missing_or_old_chainlink_ticks() {
        let tick = PriceTick {
            feed: FeedKind::ChainlinkRtds,
            recv_ms: 100_000,
            source_ts_ms: Some(99_000),
            value: 70_000.0,
        };
        assert!(source_tick_is_fresh(&tick, Duration::from_secs(15)));

        let mut stale = tick.clone();
        stale.source_ts_ms = Some(80_000);
        assert!(!source_tick_is_fresh(&stale, Duration::from_secs(15)));

        let mut missing = tick;
        missing.source_ts_ms = None;
        assert!(!source_tick_is_fresh(&missing, Duration::from_secs(15)));
    }

    #[test]
    fn reconnect_delay_is_bounded() {
        assert!(reconnect_delay(1) >= Duration::from_millis(250));
        assert!(reconnect_delay(99) <= Duration::from_millis(4_250));
    }
}

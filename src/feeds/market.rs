use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{broadcast, mpsc, watch};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, warn};

use crate::config::MARKET_WS_URL;
use crate::state::{AppEvent, FeedKind, FeedStatusUpdate, MarketTick, OrderLevel, now_ms};

pub async fn run(
    tx: mpsc::Sender<AppEvent>,
    mut assets_rx: watch::Receiver<Vec<String>>,
    mut shutdown: broadcast::Receiver<()>,
    heartbeat_seconds: u64,
) {
    let mut reconnects = 0_u64;
    loop {
        // Wait for non-empty assets before connecting
        let assets = assets_rx.borrow().clone();
        if assets.is_empty() {
            warn!("market feed waiting for assets...");
            let _ = tx
                .send(AppEvent::FeedStatus(FeedStatusUpdate {
                    feed: FeedKind::Market,
                    connected: false,
                    reconnects,
                    last_message_ms: None,
                    last_error: Some("waiting for session assets".to_string()),
                    status: "waiting".to_string(),
                }))
                .await;
            tokio::select! {
                _ = shutdown.recv() => return,
                changed = assets_rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    continue;
                }
            }
        }

        match connect_async(MARKET_WS_URL).await {
            Ok((mut socket, _)) => {
                let subscribe = json!({
                    "assets_ids": assets,
                    "type": "market",
                    "custom_feature_enabled": true,
                });
                if socket
                    .send(Message::Text(subscribe.to_string().into()))
                    .await
                    .is_err()
                {
                    warn!("market feed failed to send subscription");
                    reconnects += 1;
                    continue;
                }

                let _ = tx
                    .send(AppEvent::FeedStatus(FeedStatusUpdate {
                        feed: FeedKind::Market,
                        connected: true,
                        reconnects,
                        last_message_ms: Some(now_ms()),
                        last_error: None,
                        status: "streaming".to_string(),
                    }))
                    .await;

                let mut heartbeat =
                    tokio::time::interval(Duration::from_secs(heartbeat_seconds.max(1)));
                let mut _last_message_ms = now_ms();

                // Batching buffer for market ticks
                let mut batch: Vec<MarketTick> = Vec::with_capacity(32);
                let mut last_batch_time = tokio::time::Instant::now();
                let batch_timeout = Duration::from_millis(16); // ~1 frame at 60fps

                loop {
                    tokio::select! {
                        _ = shutdown.recv() => return,
                        changed = assets_rx.changed() => {
                            if changed.is_ok() {
                                let new_assets = assets_rx.borrow().clone();
                                if new_assets != assets {
                                    debug!("market assets changed, reconnecting");
                                    break;
                                }
                            }
                        }
                        _ = heartbeat.tick() => {
                            if socket.send(Message::Text("PING".into())).await.is_err() {
                                warn!("market feed ping failed");
                                break;
                            }
                        }
                        next = socket.next() => {
                            match next {
                                Some(Ok(Message::Text(text))) => {
                                    _last_message_ms = now_ms();
                                    for tick in parse_market_ticks(&text) {
                                        batch.push(tick);

                                        // Check if we should flush the batch
                                        let should_flush = batch.len() >= 32
                                            || last_batch_time.elapsed() > batch_timeout;

                                        if should_flush {
                                            let ticks = std::mem::take(&mut batch);
                                            if tx.send(AppEvent::MarketBatch(ticks)).await.is_err() {
                                                return;
                                            }
                                            last_batch_time = tokio::time::Instant::now();
                                        }
                                    }
                                }
                                Some(Ok(Message::Ping(bytes))) => {
                                    let _ = socket.send(Message::Pong(bytes)).await;
                                }
                                Some(Ok(Message::Pong(_))) => {
                                    _last_message_ms = now_ms();
                                }
                                Some(Ok(_)) => {}
                                Some(Err(error)) => {
                                    warn!("market websocket error: {error}");
                                    break;
                                }
                                None => {
                                    warn!("market websocket closed");
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            Err(error) => {
                warn!("failed connecting market websocket: {error}");
            }
        }

        reconnects += 1;
        let _ = tx
            .send(AppEvent::FeedStatus(FeedStatusUpdate {
                feed: FeedKind::Market,
                connected: false,
                reconnects,
                last_message_ms: None,
                last_error: Some("reconnecting".to_string()),
                status: "reconnecting".to_string(),
            }))
            .await;
        tokio::time::sleep(Duration::from_secs(reconnects.min(5) + 1)).await;
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum NumericField {
    I64(i64),
    F64(f64),
    String(String),
}

impl NumericField {
    fn as_f64(&self) -> Option<f64> {
        match self {
            Self::I64(value) => Some(*value as f64),
            Self::F64(value) => Some(*value),
            Self::String(value) => value.parse().ok(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RawOrderLevel {
    #[serde(default)]
    price: Option<NumericField>,
    #[serde(default)]
    size: Option<NumericField>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RawMarketEvent {
    #[serde(default, alias = "type")]
    event_type: Option<String>,
    #[serde(default, alias = "assetId")]
    asset_id: Option<String>,
    #[serde(default)]
    price: Option<NumericField>,
    #[serde(default)]
    last_trade_price: Option<NumericField>,
    #[serde(default)]
    size: Option<NumericField>,
    #[serde(default)]
    side: Option<String>,
    #[serde(default, alias = "bestBid")]
    best_bid: Option<NumericField>,
    #[serde(default, alias = "bestAsk")]
    best_ask: Option<NumericField>,
    #[serde(default)]
    tick_size: Option<NumericField>,
    #[serde(default)]
    new_tick_size: Option<NumericField>,
    #[serde(default)]
    bids: Vec<RawOrderLevel>,
    #[serde(default)]
    asks: Vec<RawOrderLevel>,
    #[serde(default)]
    price_changes: Vec<RawMarketEvent>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawMarketEnvelope {
    Events(Vec<RawMarketEvent>),
    Data { data: Vec<RawMarketEvent> },
    Single(Box<RawMarketEvent>),
}

impl RawMarketEnvelope {
    fn into_events(self) -> Vec<RawMarketEvent> {
        match self {
            Self::Events(events) => events,
            Self::Data { data } => data,
            Self::Single(event) => vec![*event],
        }
    }
}

fn parse_market_ticks(text: &str) -> Vec<MarketTick> {
    if text == "PONG" {
        return Vec::new();
    }
    let recv_ms = now_ms();
    let envelope: RawMarketEnvelope = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(_) => return Vec::new(),
    };
    let mut ticks = Vec::new();
    for event in envelope.into_events() {
        let event_type = event.event_type.as_deref().unwrap_or("unknown");
        if event_type == "price_change" && !event.price_changes.is_empty() {
            for change in &event.price_changes {
                if let Some(tick) = build_market_tick(change, event_type, recv_ms) {
                    ticks.push(tick);
                }
            }
            continue;
        }
        if let Some(tick) = build_market_tick(&event, event_type, recv_ms) {
            ticks.push(tick);
        }
    }
    ticks
}

fn build_market_tick(value: &RawMarketEvent, event_type: &str, recv_ms: i64) -> Option<MarketTick> {
    let price = value
        .price
        .as_ref()
        .and_then(NumericField::as_f64)
        .or_else(|| {
            value
                .last_trade_price
                .as_ref()
                .and_then(NumericField::as_f64)
        });
    Some(MarketTick {
        recv_ms,
        asset_id: value.asset_id.clone(),
        event_type: event_type.to_string(),
        price,
        size: value.size.as_ref().and_then(NumericField::as_f64),
        side: value.side.clone(),
        best_bid: value
            .best_bid
            .as_ref()
            .and_then(NumericField::as_f64)
            .or_else(|| best_level(&value.bids, true)),
        best_ask: value
            .best_ask
            .as_ref()
            .and_then(NumericField::as_f64)
            .or_else(|| best_level(&value.asks, false)),
        tick_size: value
            .new_tick_size
            .as_ref()
            .and_then(NumericField::as_f64)
            .or_else(|| value.tick_size.as_ref().and_then(NumericField::as_f64)),
        bids: parse_levels(&value.bids),
        asks: parse_levels(&value.asks),
    })
}

fn parse_levels(levels: &[RawOrderLevel]) -> Vec<OrderLevel> {
    levels
        .iter()
        .filter_map(|level| {
            let price = level.price.as_ref().and_then(NumericField::as_f64)?;
            let size = level.size.as_ref().and_then(NumericField::as_f64)?;
            (price.is_finite() && size.is_finite() && (0.0..=1.0).contains(&price) && size > 0.0)
                .then_some(OrderLevel { price, size })
        })
        .collect()
}

fn best_level(levels: &[RawOrderLevel], highest: bool) -> Option<f64> {
    levels
        .iter()
        .filter_map(|level| level.price.as_ref().and_then(NumericField::as_f64))
        .reduce(|left, right| {
            if highest {
                left.max(right)
            } else {
                left.min(right)
            }
        })
}

#[cfg(test)]
mod tests {
    use super::parse_market_ticks;

    #[test]
    fn parse_market_ticks_reads_last_trade_price_event() {
        let ticks = parse_market_ticks(
            r#"{
                "event_type":"last_trade_price",
                "assetId":"asset-1",
                "timestamp":"1700000000123",
                "last_trade_price":"0.64",
                "size":"42.0",
                "side":"BUY",
                "bestBid":"0.63",
                "bestAsk":"0.65",
                "hash":"abc123"
            }"#,
        );
        assert_eq!(ticks.len(), 1);
        let tick = &ticks[0];
        assert_eq!(tick.event_type, "last_trade_price");
        assert_eq!(tick.asset_id.as_deref(), Some("asset-1"));
        assert_eq!(tick.price, Some(0.64));
        assert_eq!(tick.best_bid, Some(0.63));
        assert_eq!(tick.best_ask, Some(0.65));
        assert_eq!(tick.size, Some(42.0));
        assert_eq!(tick.side.as_deref(), Some("BUY"));
    }

    #[test]
    fn parse_market_ticks_expands_price_change_entries() {
        let ticks = parse_market_ticks(
            r#"{
                "event_type":"price_change",
                "timestamp":"1700000000999",
                "price_changes":[
                    {
                        "asset_id":"asset-up",
                        "price":"0.71",
                        "size":"10",
                        "side":"BUY",
                        "best_bid":"0.70",
                        "best_ask":"0.72"
                    },
                    {
                        "asset_id":"asset-down",
                        "timestamp":"1700000001001",
                        "price":"0.29",
                        "size":"12",
                        "side":"SELL"
                    }
                ]
            }"#,
        );
        assert_eq!(ticks.len(), 2);
        assert!(ticks.iter().all(|tick| tick.event_type == "price_change"));
        assert_eq!(ticks[0].asset_id.as_deref(), Some("asset-up"));
        assert_eq!(ticks[1].asset_id.as_deref(), Some("asset-down"));
    }

    #[test]
    fn parse_market_ticks_handles_data_wrapped_book_events() {
        let ticks = parse_market_ticks(
            r#"{
                "data":[
                    {
                        "type":"book",
                        "asset_id":"asset-book",
                        "timestamp":"1700000002000",
                        "tick_size":"0.01",
                        "bids":[{"price":"0.41","size":"11.2"}],
                        "asks":[{"price":"0.59","size":"9.8"}]
                    }
                ]
            }"#,
        );
        assert_eq!(ticks.len(), 1);
        let tick = &ticks[0];
        assert_eq!(tick.event_type, "book");
        assert_eq!(tick.asset_id.as_deref(), Some("asset-book"));
        assert_eq!(tick.best_bid, Some(0.41));
        assert_eq!(tick.best_ask, Some(0.59));
        assert_eq!(tick.tick_size, Some(0.01));
        assert_eq!(
            tick.bids,
            vec![crate::state::OrderLevel {
                price: 0.41,
                size: 11.2
            }]
        );
        assert_eq!(
            tick.asks,
            vec![crate::state::OrderLevel {
                price: 0.59,
                size: 9.8
            }]
        );
    }

    #[test]
    fn parse_market_ticks_reads_tick_size_changes() {
        let ticks = parse_market_ticks(
            r#"{
                "event_type":"tick_size_change",
                "asset_id":"asset-book",
                "old_tick_size":"0.01",
                "new_tick_size":"0.001",
                "timestamp":"1700000002000"
            }"#,
        );
        assert_eq!(ticks.len(), 1);
        assert_eq!(ticks[0].event_type, "tick_size_change");
        assert_eq!(ticks[0].tick_size, Some(0.001));
    }
}

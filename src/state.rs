use std::collections::{BTreeMap, VecDeque};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::connectivity::ConnectivityStatus;
use crate::history::{PriceSample, SessionRecord};
use crate::portfolio::PortfolioState;
use crate::trading::{
    MAX_TRADING_BALANCE_AGE_MS, MIN_BUY_ORDER_USD, MIN_MAKER_SESSION_REMAINING_MS, TradeSide,
    TradingEvent, TradingMechanism, TradingState, minimum_buy_nominal_usd,
};

const ORDERBOOK_VISIBLE_LEVELS: usize = 8;
const ORDERBOOK_MAX_LEVELS_PER_SIDE: usize = 64;
const ORDERBOOK_PRICE_SCALE: f64 = 1_000_000.0;

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum FeedKind {
    #[serde(alias = "binance_rtds")]
    BinanceSpot,
    ChainlinkRtds,
    Market,
}

impl FeedKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::BinanceSpot => "binance",
            Self::ChainlinkRtds => "chainlink",
            Self::Market => "market",
        }
    }
}

impl fmt::Display for FeedKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionDescriptor {
    pub slug: String,
    pub title: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub price_to_beat: Option<f64>,
    pub up_token_id: String,
    pub down_token_id: String,
    pub active: bool,
    pub closed: bool,
    #[serde(skip, default)]
    pub minimum_order_size: Option<f64>,
    #[serde(skip, default)]
    pub tick_size: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryUpdate {
    pub fetched_at_ms: i64,
    pub sessions: Vec<SessionDescriptor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceTick {
    pub feed: FeedKind,
    pub recv_ms: i64,
    pub source_ts_ms: Option<i64>,
    pub value: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketTick {
    pub recv_ms: i64,
    pub asset_id: Option<String>,
    pub event_type: String,
    pub price: Option<f64>,
    #[serde(default)]
    pub size: Option<f64>,
    #[serde(default)]
    pub side: Option<String>,
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    #[serde(default)]
    pub tick_size: Option<f64>,
    #[serde(default)]
    pub bids: Vec<OrderLevel>,
    #[serde(default)]
    pub asks: Vec<OrderLevel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedStatusUpdate {
    pub feed: FeedKind,
    pub connected: bool,
    pub reconnects: u64,
    pub last_message_ms: Option<i64>,
    pub last_error: Option<String>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "payload")]
pub enum AppEvent {
    Discovery(DiscoveryUpdate),
    Price(PriceTick),
    Market(MarketTick),
    MarketBatch(Vec<MarketTick>),
    FeedStatus(FeedStatusUpdate),
    Connectivity(ConnectivityStatus),
    Trading(TradingEvent),
    Portfolio(PortfolioState),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct OutcomeQuote {
    pub last_trade: Option<f64>,
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    pub updated_at_ms: Option<i64>,
    pub minimum_order_size: Option<f64>,
    pub tick_size: Option<f64>,
}

impl OutcomeQuote {
    pub fn display_price(&self) -> Option<f64> {
        self.last_trade
            .or_else(|| match (self.best_bid, self.best_ask) {
                (Some(bid), Some(ask)) if ask >= bid => Some((bid + ask) / 2.0),
                _ => self.best_ask.or(self.best_bid),
            })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OrderLevel {
    pub price: f64,
    pub size: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct OrderBookView {
    pub bids: Vec<OrderLevel>,
    pub asks: Vec<OrderLevel>,
    pub updated_at_ms: Option<i64>,
}

impl OrderBookView {
    pub fn best_bid(&self) -> Option<f64> {
        self.bids.first().map(|level| level.price)
    }

    pub fn best_ask(&self) -> Option<f64> {
        self.asks.first().map(|level| level.price)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DashboardOrderBooks {
    pub up: OrderBookView,
    pub down: OrderBookView,
}

#[derive(Debug, Default)]
struct OrderBookState {
    bids: BTreeMap<i64, f64>,
    asks: BTreeMap<i64, f64>,
    updated_at_ms: Option<i64>,
}

impl OrderBookState {
    fn clear(&mut self) {
        self.bids.clear();
        self.asks.clear();
        self.updated_at_ms = None;
    }

    fn replace(&mut self, bids: &[OrderLevel], asks: &[OrderLevel], updated_at_ms: i64) {
        self.bids.clear();
        self.asks.clear();
        for level in bids {
            self.set_level(true, level.price, level.size);
        }
        for level in asks {
            self.set_level(false, level.price, level.size);
        }
        self.trim();
        self.updated_at_ms = Some(updated_at_ms);
    }

    fn apply_price_change(&mut self, side: &str, price: f64, size: f64, updated_at_ms: i64) {
        let is_bid = side.eq_ignore_ascii_case("buy") || side.eq_ignore_ascii_case("bid");
        let is_ask = side.eq_ignore_ascii_case("sell") || side.eq_ignore_ascii_case("ask");
        if !is_bid && !is_ask {
            return;
        }
        self.set_level(is_bid, price, size);
        self.trim();
        self.updated_at_ms = Some(updated_at_ms);
    }

    fn set_level(&mut self, is_bid: bool, price: f64, size: f64) {
        if !price.is_finite() || !(0.0..=1.0).contains(&price) || !size.is_finite() {
            return;
        }
        let levels = if is_bid {
            &mut self.bids
        } else {
            &mut self.asks
        };
        let key = (price * ORDERBOOK_PRICE_SCALE).round() as i64;
        if size <= 0.0 {
            levels.remove(&key);
        } else {
            levels.insert(key, size);
        }
    }

    fn trim(&mut self) {
        while self.bids.len() > ORDERBOOK_MAX_LEVELS_PER_SIDE {
            let Some(key) = self.bids.first_key_value().map(|(key, _)| *key) else {
                break;
            };
            self.bids.remove(&key);
        }
        while self.asks.len() > ORDERBOOK_MAX_LEVELS_PER_SIDE {
            let Some(key) = self.asks.last_key_value().map(|(key, _)| *key) else {
                break;
            };
            self.asks.remove(&key);
        }
    }

    fn view(&self) -> OrderBookView {
        OrderBookView {
            bids: self
                .bids
                .iter()
                .rev()
                .take(ORDERBOOK_VISIBLE_LEVELS)
                .map(|(price, size)| OrderLevel {
                    price: *price as f64 / ORDERBOOK_PRICE_SCALE,
                    size: *size,
                })
                .collect(),
            asks: self
                .asks
                .iter()
                .take(ORDERBOOK_VISIBLE_LEVELS)
                .map(|(price, size)| OrderLevel {
                    price: *price as f64 / ORDERBOOK_PRICE_SCALE,
                    size: *size,
                })
                .collect(),
            updated_at_ms: self.updated_at_ms,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DashboardSnapshot {
    pub now_ms: i64,
    pub web_trading_allowed: bool,
    pub minimum_buy_order_usd: f64,
    pub minimum_maker_remaining_ms: i64,
    pub maximum_trading_balance_age_ms: i64,
    pub current_session: Option<SessionDescriptor>,
    pub next_session: Option<SessionDescriptor>,
    pub past_sessions: Vec<SessionRecord>,
    pub binance_btc_usd: Option<f64>,
    pub chainlink_btc_usd: Option<f64>,
    pub up: OutcomeQuote,
    pub down: OutcomeQuote,
    pub orderbook: DashboardOrderBooks,
    pub price_history: Vec<PriceSample>,
    pub feeds: Vec<FeedStatusUpdate>,
    pub connectivity: ConnectivityStatus,
    pub trading: TradingState,
    pub portfolio: PortfolioState,
}

#[derive(Debug, Default)]
pub struct ApplyEffects {
    pub market_assets: Option<Vec<String>>,
    pub observed_session: Option<SessionRecord>,
}

pub struct AppState {
    history_cap: usize,
    web_trading_allowed: bool,
    current_session: Option<SessionDescriptor>,
    next_session: Option<SessionDescriptor>,
    past_sessions: VecDeque<SessionRecord>,
    binance_btc_usd: Option<f64>,
    chainlink_btc_usd: Option<f64>,
    up: OutcomeQuote,
    down: OutcomeQuote,
    up_orderbook: OrderBookState,
    down_orderbook: OrderBookState,
    price_history: VecDeque<PriceSample>,
    last_price_bucket_ms: Option<i64>,
    feed_status: BTreeMap<FeedKind, FeedStatusUpdate>,
    connectivity: ConnectivityStatus,
    trading: TradingState,
    portfolio: PortfolioState,
}

impl AppState {
    pub fn new(
        history_cap: usize,
        web_trading_allowed: bool,
        trading_configured: bool,
        past_sessions: Vec<SessionRecord>,
        price_history: Vec<PriceSample>,
        portfolio: PortfolioState,
    ) -> Self {
        let mut trading = TradingState::default();
        trading.set_configured(trading_configured);
        let last_price_bucket_ms = price_history.last().map(|sample| sample.timestamp_ms);
        Self {
            history_cap,
            web_trading_allowed,
            current_session: None,
            next_session: None,
            past_sessions: past_sessions.into_iter().rev().take(100).rev().collect(),
            binance_btc_usd: price_history
                .last()
                .and_then(|sample| sample.binance_btc_usd),
            chainlink_btc_usd: price_history
                .last()
                .and_then(|sample| sample.chainlink_btc_usd),
            up: OutcomeQuote::default(),
            down: OutcomeQuote::default(),
            up_orderbook: OrderBookState::default(),
            down_orderbook: OrderBookState::default(),
            price_history: price_history
                .into_iter()
                .rev()
                .take(history_cap)
                .rev()
                .collect(),
            last_price_bucket_ms,
            feed_status: BTreeMap::new(),
            connectivity: ConnectivityStatus::default(),
            trading,
            portfolio,
        }
    }

    pub fn apply(&mut self, event: AppEvent) -> ApplyEffects {
        let mut effects = ApplyEffects::default();
        match event {
            AppEvent::Discovery(update) => self.apply_discovery(update, &mut effects),
            AppEvent::Price(tick) => self.apply_price(tick),
            AppEvent::Market(tick) => self.apply_market(tick),
            AppEvent::MarketBatch(ticks) => {
                for tick in ticks {
                    self.apply_market(tick);
                }
            }
            AppEvent::FeedStatus(status) => {
                if !status.connected {
                    match status.feed {
                        FeedKind::BinanceSpot => self.binance_btc_usd = None,
                        FeedKind::ChainlinkRtds => self.chainlink_btc_usd = None,
                        FeedKind::Market => {}
                    }
                }
                self.feed_status.insert(status.feed, status);
            }
            AppEvent::Connectivity(status) => self.connectivity = status,
            AppEvent::Trading(event) => self.apply_trading_event(event),
            AppEvent::Portfolio(portfolio) => self.portfolio = portfolio,
        }
        effects
    }

    fn apply_discovery(&mut self, mut update: DiscoveryUpdate, effects: &mut ApplyEffects) {
        update
            .sessions
            .sort_by_key(|session| (session.start_ms, session.end_ms));
        let now = update.fetched_at_ms;
        let current = update
            .sessions
            .iter()
            .find(|session| session.start_ms <= now && now < session.end_ms)
            .cloned();
        let next = update
            .sessions
            .iter()
            .find(|session| session.start_ms > now)
            .cloned();
        let previous_slug = self
            .current_session
            .as_ref()
            .map(|session| session.slug.as_str());
        let current_slug = current.as_ref().map(|session| session.slug.as_str());
        if previous_slug != current_slug {
            self.up = OutcomeQuote::default();
            self.down = OutcomeQuote::default();
            self.up_orderbook.clear();
            self.down_orderbook.clear();
            self.trading.reset_for_new_session();
            if let Some(session) = current.as_ref() {
                self.trading.reconcile_submission_lock(&session.slug, now);
                effects.market_assets = Some(vec![
                    session.up_token_id.clone(),
                    session.down_token_id.clone(),
                ]);
                effects.observed_session = Some(SessionRecord {
                    observed_at_ms: now,
                    session: session.clone(),
                });
            } else {
                effects.market_assets = Some(Vec::new());
            }
        }
        if let Some(session) = current.as_ref() {
            let minimum_order_size = session
                .minimum_order_size
                .filter(|value| value.is_finite() && *value > 0.0);
            let tick_size = session
                .tick_size
                .filter(|value| value.is_finite() && *value > 0.0);
            self.up.minimum_order_size = minimum_order_size;
            self.down.minimum_order_size = minimum_order_size;
            self.up.tick_size = tick_size;
            self.down.tick_size = tick_size;
        }
        self.current_session = current;
        self.next_session = next;
    }

    fn apply_price(&mut self, tick: PriceTick) {
        if !tick.value.is_finite() || tick.value <= 0.0 {
            return;
        }
        match tick.feed {
            FeedKind::BinanceSpot => self.binance_btc_usd = Some(tick.value),
            FeedKind::ChainlinkRtds => {
                self.chainlink_btc_usd = Some(tick.value);
                if let Some(session) = self.current_session.as_mut()
                    && session.price_to_beat.is_none()
                {
                    session.price_to_beat = Some(tick.value);
                }
            }
            FeedKind::Market => {}
        }
    }

    fn apply_market(&mut self, tick: MarketTick) {
        let Some(session) = self.current_session.as_ref() else {
            return;
        };
        let Some(asset_id) = tick.asset_id.as_deref() else {
            return;
        };
        let (quote, orderbook) = if asset_id == session.up_token_id {
            (&mut self.up, &mut self.up_orderbook)
        } else if asset_id == session.down_token_id {
            (&mut self.down, &mut self.down_orderbook)
        } else {
            return;
        };
        match tick.event_type.as_str() {
            "book" => {
                orderbook.replace(&tick.bids, &tick.asks, tick.recv_ms);
                let view = orderbook.view();
                quote.best_bid = view.best_bid();
                quote.best_ask = view.best_ask();
                if let Some(tick_size) = tick.tick_size.filter(|value| value.is_finite()) {
                    quote.tick_size = Some(tick_size);
                }
                quote.updated_at_ms = Some(tick.recv_ms);
            }
            "price_change" => {
                if let (Some(side), Some(price), Some(size)) =
                    (tick.side.as_deref(), tick.price, tick.size)
                {
                    orderbook.apply_price_change(side, price, size, tick.recv_ms);
                }
                let view = orderbook.view();
                quote.best_bid = tick
                    .best_bid
                    .filter(|value| value.is_finite())
                    .or_else(|| view.best_bid());
                quote.best_ask = tick
                    .best_ask
                    .filter(|value| value.is_finite())
                    .or_else(|| view.best_ask());
                quote.updated_at_ms = Some(tick.recv_ms);
            }
            "last_trade_price" => {
                if let Some(price) = tick.price.filter(|value| value.is_finite()) {
                    quote.last_trade = Some(price);
                    quote.updated_at_ms = Some(tick.recv_ms);
                }
                if let Some(best_bid) = tick.best_bid.filter(|value| value.is_finite()) {
                    quote.best_bid = Some(best_bid);
                }
                if let Some(best_ask) = tick.best_ask.filter(|value| value.is_finite()) {
                    quote.best_ask = Some(best_ask);
                }
            }
            "best_bid_ask" => {
                if let Some(best_bid) = tick.best_bid.filter(|value| value.is_finite()) {
                    quote.best_bid = Some(best_bid);
                }
                if let Some(best_ask) = tick.best_ask.filter(|value| value.is_finite()) {
                    quote.best_ask = Some(best_ask);
                }
                quote.updated_at_ms = Some(tick.recv_ms);
            }
            "tick_size_change" => {
                if let Some(tick_size) = tick
                    .tick_size
                    .filter(|value| value.is_finite() && *value > 0.0)
                {
                    quote.tick_size = Some(tick_size);
                    quote.updated_at_ms = Some(tick.recv_ms);
                }
            }
            _ => {}
        }
    }

    fn apply_trading_event(&mut self, event: TradingEvent) {
        match event {
            TradingEvent::LedgerLoaded {
                entries,
                timestamp_ms,
            } => {
                self.trading.replace_ledger(entries);
                if let Some(session_slug) = self
                    .current_session
                    .as_ref()
                    .map(|session| session.slug.clone())
                {
                    self.trading
                        .reconcile_submission_lock(&session_slug, timestamp_ms);
                }
            }
            TradingEvent::LedgerUpsert { entry } => {
                if let Some(order_id) = entry.order_id.as_ref() {
                    self.trading.active_order_id =
                        (!entry.status.is_terminal()).then(|| order_id.clone());
                }
                let timestamp_ms = entry.updated_at_ms;
                self.trading.upsert_ledger_entry(entry);
                if let Some(session_slug) = self
                    .current_session
                    .as_ref()
                    .map(|session| session.slug.clone())
                {
                    self.trading
                        .reconcile_submission_lock(&session_slug, timestamp_ms);
                }
            }
            TradingEvent::BalanceUpdated {
                available_usdc,
                allowance_usdc,
                error,
                timestamp_ms,
            } => {
                self.trading.available_usdc = available_usdc;
                self.trading.allowance_usdc = allowance_usdc;
                self.trading.balance_updated_ms = Some(timestamp_ms);
                self.trading.balance_error = error;
                self.trading.ready_to_trade = self.trading.is_ready();
            }
            TradingEvent::OrderPlaced {
                local_id,
                order_id,
                side,
                price,
                size,
                mechanism,
                ..
            } => {
                self.trading.order_status =
                    format!("Placed: {side} {mechanism} @ {price:.2} ({size}sh)");
                self.trading.active_order_id = Some(order_id);
                self.trading.orders_placed = self.trading.orders_placed.saturating_add(1);
                self.trading.last_error = None;
                self.trading.clear_in_flight_if(&local_id);
            }
            TradingEvent::OrderCancelled { reason, .. } => {
                self.trading.order_status = format!("Cancelled: {reason}");
                self.trading.active_order_id = None;
                self.trading.orders_cancelled = self.trading.orders_cancelled.saturating_add(1);
            }
            TradingEvent::Error { message, .. } => {
                self.trading.order_status = format!("Error: {message}");
                self.trading.last_error = Some(message);
            }
            TradingEvent::Status { message, .. } => self.trading.order_status = message,
        }
    }

    pub fn sample_price(&mut self, timestamp_ms: i64) -> Option<PriceSample> {
        let bucket_ms = timestamp_ms.div_euclid(1_000) * 1_000;
        if self
            .last_price_bucket_ms
            .is_some_and(|previous| bucket_ms <= previous)
        {
            return None;
        }
        let binance_btc_usd = self.live_feed_value(FeedKind::BinanceSpot, self.binance_btc_usd);
        let chainlink_btc_usd =
            self.live_feed_value(FeedKind::ChainlinkRtds, self.chainlink_btc_usd);
        let up_price = self.live_feed_value(FeedKind::Market, self.up.display_price());
        let down_price = self.live_feed_value(FeedKind::Market, self.down.display_price());
        if binance_btc_usd.is_none()
            && chainlink_btc_usd.is_none()
            && up_price.is_none()
            && down_price.is_none()
        {
            return None;
        }
        self.last_price_bucket_ms = Some(bucket_ms);
        let sample = PriceSample {
            timestamp_ms: bucket_ms,
            session_slug: self
                .current_session
                .as_ref()
                .map(|session| session.slug.clone()),
            binance_btc_usd,
            chainlink_btc_usd,
            up_price,
            down_price,
        };
        if self.price_history.len() == self.history_cap {
            self.price_history.pop_front();
        }
        self.price_history.push_back(sample.clone());
        Some(sample)
    }

    fn live_feed_value(&self, feed: FeedKind, value: Option<f64>) -> Option<f64> {
        self.feed_status
            .get(&feed)
            .is_some_and(|status| status.connected)
            .then_some(value)
            .flatten()
    }

    pub fn add_past_session(&mut self, record: SessionRecord) {
        if self
            .past_sessions
            .iter()
            .any(|existing| existing.session.slug == record.session.slug)
        {
            return;
        }
        if self.past_sessions.len() == 100 {
            self.past_sessions.pop_front();
        }
        self.past_sessions.push_back(record);
    }

    pub fn snapshot(&self) -> DashboardSnapshot {
        let current_slug = self
            .current_session
            .as_ref()
            .map(|session| session.slug.as_str());
        DashboardSnapshot {
            now_ms: now_ms(),
            web_trading_allowed: self.web_trading_allowed,
            minimum_buy_order_usd: MIN_BUY_ORDER_USD,
            minimum_maker_remaining_ms: MIN_MAKER_SESSION_REMAINING_MS,
            maximum_trading_balance_age_ms: MAX_TRADING_BALANCE_AGE_MS,
            current_session: self.current_session.clone(),
            next_session: self.next_session.clone(),
            past_sessions: self
                .past_sessions
                .iter()
                .filter(|record| Some(record.session.slug.as_str()) != current_slug)
                .rev()
                .take(20)
                .cloned()
                .collect(),
            binance_btc_usd: self.binance_btc_usd,
            chainlink_btc_usd: self.chainlink_btc_usd,
            up: self.up.clone(),
            down: self.down.clone(),
            orderbook: DashboardOrderBooks {
                up: self.up_orderbook.view(),
                down: self.down_orderbook.view(),
            },
            price_history: self.price_history.iter().cloned().collect(),
            feeds: self.feed_status.values().cloned().collect(),
            connectivity: self.connectivity.clone(),
            trading: self.trading.clone(),
            portfolio: self.portfolio.clone(),
        }
    }

    pub fn current_session(&self) -> Option<&SessionDescriptor> {
        self.current_session.as_ref()
    }

    pub fn trading(&self) -> &TradingState {
        &self.trading
    }

    pub fn trading_mut(&mut self) -> &mut TradingState {
        &mut self.trading
    }

    pub fn minimum_buy_nominal(
        &self,
        trade_side: TradeSide,
        mechanism: TradingMechanism,
    ) -> Option<f64> {
        let quote = match trade_side {
            TradeSide::BuyUp => &self.up,
            TradeSide::BuyDown => &self.down,
            TradeSide::SellUp | TradeSide::SellDown => return None,
        };
        minimum_buy_nominal_usd(
            quote.best_ask?,
            quote.tick_size?,
            quote.minimum_order_size?,
            mechanism,
        )
    }

    pub fn portfolio(&self) -> &PortfolioState {
        &self.portfolio
    }

    pub fn portfolio_mut(&mut self) -> &mut PortfolioState {
        &mut self.portfolio
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn session(slug: &str, start_ms: i64) -> SessionDescriptor {
        SessionDescriptor {
            slug: slug.to_string(),
            title: "BTC Up or Down".to_string(),
            start_ms,
            end_ms: start_ms + 300_000,
            price_to_beat: Some(70_000.0),
            up_token_id: format!("{slug}-up"),
            down_token_id: format!("{slug}-down"),
            active: true,
            closed: false,
            minimum_order_size: None,
            tick_size: None,
        }
    }

    fn feed_status(feed: FeedKind, connected: bool, timestamp_ms: i64) -> AppEvent {
        AppEvent::FeedStatus(FeedStatusUpdate {
            feed,
            connected,
            reconnects: 0,
            last_message_ms: connected.then_some(timestamp_ms),
            last_error: (!connected).then(|| "offline".to_string()),
            status: if connected { "live" } else { "offline" }.to_string(),
        })
    }

    #[test]
    fn discovery_selects_current_session_and_only_two_assets() {
        let mut state = AppState::new(
            60,
            false,
            false,
            Vec::new(),
            Vec::new(),
            PortfolioState::default(),
        );
        let effects = state.apply(AppEvent::Discovery(DiscoveryUpdate {
            fetched_at_ms: 10_000,
            sessions: vec![session("current", 0), session("next", 300_000)],
        }));
        assert_eq!(
            effects.market_assets,
            Some(vec!["current-up".to_string(), "current-down".to_string()])
        );
        assert_eq!(
            state.current_session().map(|value| value.slug.as_str()),
            Some("current")
        );
    }

    #[test]
    fn one_second_history_is_bounded_and_deduplicated() {
        let mut state = AppState::new(
            2,
            false,
            false,
            Vec::new(),
            Vec::new(),
            PortfolioState::default(),
        );
        state.apply(feed_status(FeedKind::BinanceSpot, true, 1_100));
        state.apply(AppEvent::Price(PriceTick {
            feed: FeedKind::BinanceSpot,
            recv_ms: 1_100,
            source_ts_ms: Some(1_100),
            value: 70_000.0,
        }));
        assert!(state.sample_price(1_100).is_some());
        assert!(state.sample_price(1_999).is_none());
        assert!(state.sample_price(2_000).is_some());
        assert!(state.sample_price(3_000).is_some());
        assert_eq!(state.snapshot().price_history.len(), 2);
    }

    #[test]
    fn disconnected_feeds_never_persist_stale_one_second_values() {
        let mut state = AppState::new(
            10,
            false,
            false,
            Vec::new(),
            Vec::new(),
            PortfolioState::default(),
        );
        state.apply(feed_status(FeedKind::BinanceSpot, true, 1_000));
        state.apply(feed_status(FeedKind::ChainlinkRtds, true, 1_000));
        state.apply(AppEvent::Price(PriceTick {
            feed: FeedKind::BinanceSpot,
            recv_ms: 1_000,
            source_ts_ms: Some(1_000),
            value: 70_000.0,
        }));
        state.apply(AppEvent::Price(PriceTick {
            feed: FeedKind::ChainlinkRtds,
            recv_ms: 1_000,
            source_ts_ms: Some(1_000),
            value: 69_999.0,
        }));
        let first = state.sample_price(1_000).expect("live sample");
        assert_eq!(first.chainlink_btc_usd, Some(69_999.0));

        state.apply(feed_status(FeedKind::ChainlinkRtds, false, 2_000));
        state.apply(AppEvent::Price(PriceTick {
            feed: FeedKind::BinanceSpot,
            recv_ms: 2_000,
            source_ts_ms: Some(2_000),
            value: 70_001.0,
        }));
        let second = state.sample_price(2_000).expect("remaining live feed");
        assert_eq!(second.binance_btc_usd, Some(70_001.0));
        assert_eq!(second.chainlink_btc_usd, None);
        assert_eq!(state.snapshot().chainlink_btc_usd, None);
    }

    #[test]
    fn public_snapshot_schema_is_stable() {
        let state = AppState::new(
            60,
            false,
            false,
            Vec::new(),
            Vec::new(),
            PortfolioState::default(),
        );
        let value = serde_json::to_value(state.snapshot()).expect("serialize snapshot");
        let object = value.as_object().expect("snapshot object");
        let actual = object.keys().map(String::as_str).collect::<BTreeSet<_>>();
        let expected = [
            "binance_btc_usd",
            "chainlink_btc_usd",
            "connectivity",
            "current_session",
            "down",
            "feeds",
            "minimum_buy_order_usd",
            "minimum_maker_remaining_ms",
            "maximum_trading_balance_age_ms",
            "next_session",
            "now_ms",
            "orderbook",
            "past_sessions",
            "portfolio",
            "price_history",
            "trading",
            "up",
            "web_trading_allowed",
        ]
        .into_iter()
        .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn transient_market_rules_do_not_enter_session_history_records() {
        let mut descriptor = session("current", 0);
        descriptor.minimum_order_size = Some(5.0);
        descriptor.tick_size = Some(0.01);
        let value = serde_json::to_value(descriptor).expect("serialize session");
        let object = value.as_object().expect("session object");
        assert!(!object.contains_key("minimum_order_size"));
        assert!(!object.contains_key("tick_size"));
    }

    #[test]
    fn market_book_snapshot_and_deltas_drive_bounded_live_orderbook() {
        let mut state = AppState::new(
            60,
            false,
            false,
            Vec::new(),
            Vec::new(),
            PortfolioState::default(),
        );
        state.apply(AppEvent::Discovery(DiscoveryUpdate {
            fetched_at_ms: 10_000,
            sessions: vec![session("current", 0)],
        }));
        let bids = (1..=80)
            .map(|step| OrderLevel {
                price: step as f64 / 100.0,
                size: step as f64,
            })
            .collect();
        state.apply(AppEvent::Market(MarketTick {
            recv_ms: 11_000,
            asset_id: Some("current-up".to_string()),
            event_type: "book".to_string(),
            price: None,
            size: None,
            side: None,
            best_bid: None,
            best_ask: None,
            tick_size: Some(0.01),
            bids,
            asks: vec![
                OrderLevel {
                    price: 0.82,
                    size: 9.0,
                },
                OrderLevel {
                    price: 0.81,
                    size: 7.0,
                },
            ],
        }));

        assert_eq!(state.up_orderbook.bids.len(), ORDERBOOK_MAX_LEVELS_PER_SIDE);
        let snapshot = state.snapshot();
        assert_eq!(snapshot.orderbook.up.bids.len(), ORDERBOOK_VISIBLE_LEVELS);
        assert_eq!(snapshot.orderbook.up.bids[0].price, 0.80);
        assert_eq!(snapshot.orderbook.up.asks[0].price, 0.81);
        assert_eq!(snapshot.up.best_bid, Some(0.80));
        assert_eq!(snapshot.up.best_ask, Some(0.81));

        state.apply(AppEvent::Market(MarketTick {
            recv_ms: 12_000,
            asset_id: Some("current-up".to_string()),
            event_type: "price_change".to_string(),
            price: Some(0.80),
            size: Some(0.0),
            side: Some("BUY".to_string()),
            best_bid: Some(0.79),
            best_ask: Some(0.81),
            tick_size: None,
            bids: Vec::new(),
            asks: Vec::new(),
        }));
        let snapshot = state.snapshot();
        assert_eq!(snapshot.orderbook.up.bids[0].price, 0.79);
        assert_eq!(snapshot.up.best_bid, Some(0.79));
        assert_eq!(snapshot.up.last_trade, None);
    }

    #[test]
    fn price_level_changes_never_masquerade_as_last_trades() {
        let mut state = AppState::new(
            60,
            false,
            false,
            Vec::new(),
            Vec::new(),
            PortfolioState::default(),
        );
        state.apply(AppEvent::Discovery(DiscoveryUpdate {
            fetched_at_ms: 10_000,
            sessions: vec![session("current", 0)],
        }));
        state.apply(AppEvent::Market(MarketTick {
            recv_ms: 11_000,
            asset_id: Some("current-down".to_string()),
            event_type: "last_trade_price".to_string(),
            price: Some(0.44),
            size: Some(3.0),
            side: Some("BUY".to_string()),
            best_bid: Some(0.43),
            best_ask: Some(0.45),
            tick_size: None,
            bids: Vec::new(),
            asks: Vec::new(),
        }));
        state.apply(AppEvent::Market(MarketTick {
            recv_ms: 12_000,
            asset_id: Some("current-down".to_string()),
            event_type: "price_change".to_string(),
            price: Some(0.30),
            size: Some(99.0),
            side: Some("BUY".to_string()),
            best_bid: Some(0.43),
            best_ask: Some(0.45),
            tick_size: None,
            bids: Vec::new(),
            asks: Vec::new(),
        }));

        assert_eq!(state.snapshot().down.last_trade, Some(0.44));
    }

    #[test]
    fn live_market_rules_drive_the_same_minimum_as_execution() {
        let mut current = session("current", 0);
        current.minimum_order_size = Some(5.0);
        current.tick_size = Some(0.01);
        let mut state = AppState::new(
            60,
            false,
            false,
            Vec::new(),
            Vec::new(),
            PortfolioState::default(),
        );
        state.apply(AppEvent::Discovery(DiscoveryUpdate {
            fetched_at_ms: 10_000,
            sessions: vec![current],
        }));
        state.apply(AppEvent::Market(MarketTick {
            recv_ms: 11_000,
            asset_id: Some("current-up".to_string()),
            event_type: "book".to_string(),
            price: None,
            size: None,
            side: None,
            best_bid: None,
            best_ask: None,
            tick_size: Some(0.01),
            bids: vec![OrderLevel {
                price: 0.54,
                size: 100.0,
            }],
            asks: vec![OrderLevel {
                price: 0.55,
                size: 100.0,
            }],
        }));

        assert_eq!(
            state.minimum_buy_nominal(TradeSide::BuyUp, TradingMechanism::FastTaker),
            Some(2.81)
        );
        assert_eq!(
            state.minimum_buy_nominal(TradeSide::BuyUp, TradingMechanism::FastMaker),
            Some(2.60)
        );
    }

    #[test]
    fn generic_error_does_not_release_an_active_submission_lock() {
        let mut state = AppState::new(
            60,
            false,
            false,
            Vec::new(),
            Vec::new(),
            PortfolioState::default(),
        );
        state.trading.mark_intent_in_flight(
            "local-1".to_string(),
            "fingerprint-1".to_string(),
            "btc-updown-5m-1".to_string(),
            1_000,
        );
        state.apply_trading_event(TradingEvent::Error {
            message: "exchange outcome unavailable".to_string(),
            timestamp_ms: 2_000,
        });
        assert!(state.trading.in_flight_intent.is_some());
    }
}

use std::collections::{BTreeMap, VecDeque};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::connectivity::ConnectivityStatus;
use crate::history::{PriceSample, SessionRecord};
use crate::portfolio::PortfolioState;
use crate::trading::{TradingEvent, TradingState};

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
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
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

#[derive(Debug, Clone, Serialize)]
pub struct DashboardSnapshot {
    pub now_ms: i64,
    pub web_trading_allowed: bool,
    pub current_session: Option<SessionDescriptor>,
    pub next_session: Option<SessionDescriptor>,
    pub past_sessions: Vec<SessionRecord>,
    pub binance_btc_usd: Option<f64>,
    pub chainlink_btc_usd: Option<f64>,
    pub up: OutcomeQuote,
    pub down: OutcomeQuote,
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
        let quote = if asset_id == session.up_token_id {
            &mut self.up
        } else if asset_id == session.down_token_id {
            &mut self.down
        } else {
            return;
        };
        if let Some(price) = tick.price.filter(|value| value.is_finite()) {
            quote.last_trade = Some(price);
        }
        quote.best_bid = tick.best_bid.filter(|value| value.is_finite());
        quote.best_ask = tick.best_ask.filter(|value| value.is_finite());
        quote.updated_at_ms = Some(tick.recv_ms);
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
        if self.binance_btc_usd.is_none()
            && self.chainlink_btc_usd.is_none()
            && self.up.display_price().is_none()
            && self.down.display_price().is_none()
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
            binance_btc_usd: self.binance_btc_usd,
            chainlink_btc_usd: self.chainlink_btc_usd,
            up_price: self.up.display_price(),
            down_price: self.down.display_price(),
        };
        if self.price_history.len() == self.history_cap {
            self.price_history.pop_front();
        }
        self.price_history.push_back(sample.clone());
        Some(sample)
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
        }
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
            "next_session",
            "now_ms",
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

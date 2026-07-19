use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use alloy::primitives::{Address, U256};
use alloy::signers::Signer as _;
use alloy::signers::local::PrivateKeySigner;
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, TimeDelta, Utc};
use futures_util::StreamExt as _;
use polymarket_client_sdk_v2::POLYGON;
use polymarket_client_sdk_v2::auth::Normal;
use polymarket_client_sdk_v2::auth::state::Authenticated;
use polymarket_client_sdk_v2::clob::types::request::BalanceAllowanceRequest;
use polymarket_client_sdk_v2::clob::types::response::OpenOrderResponse;
use polymarket_client_sdk_v2::clob::types::{
    Amount, AssetType, OrderStatusType, OrderType as ClobOrderType, Side as ClobSide,
    SignatureType, TickSize,
};
use polymarket_client_sdk_v2::clob::ws::types::response::{OrderMessageType, TradeMessageStatus};
use polymarket_client_sdk_v2::clob::ws::{
    Client as WsClient, OrderMessage as WsOrderMessage, TradeMessage as WsTradeMessage,
};
use polymarket_client_sdk_v2::clob::{Client as ClobClient, Config as ClobConfig};
use polymarket_client_sdk_v2::error::{
    Error as ClobSdkError, Kind as ClobErrorKind, Status as ClobStatus,
};
use polymarket_client_sdk_v2::types::Decimal;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::{RwLock, broadcast, mpsc};
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::{CLOB_API_URL, DATA_API_URL, TradeSmokeArgs, TradingArgs};
use crate::state::now_ms;

const MAKER_OFFSET: f64 = 0.03;
const MAKER_TIMEOUT_SECS: i64 = 3;
const GTD_MIN_EXPIRATION_LEAD_SECS: i64 = 180;
const MAKER_GTD_EXPIRATION_LEAD_SECS: i64 = GTD_MIN_EXPIRATION_LEAD_SECS + MAKER_TIMEOUT_SECS;
const _: () = {
    assert!(MAKER_GTD_EXPIRATION_LEAD_SECS > GTD_MIN_EXPIRATION_LEAD_SECS);
};
const MIN_PRICE: f64 = 0.01;
const MAX_PRICE: f64 = 0.99;
const POLYMARKET_CTF_EXCHANGE_V2_SPENDER: &str = "0xE111180000d2663C0091e4f400237545B87B996B";
const POLYMARKET_NEG_RISK_CTF_EXCHANGE_V2_SPENDER: &str =
    "0xe2222d279d744050d28e00520010520000310F59";
const SHARE_SCALE: u32 = 2;
const MIN_MARKETABLE_BUY_USD: f64 = 1.0;
const BALANCE_POLL_SECS: u64 = 2;
const RETRY_ATTEMPTS: u8 = 2;
const ORDER_RECONCILE_POLL_SECS: u64 = 1;
const SUBMIT_LOCKOUT_MS: i64 = 1_500;
const MAX_TRADE_HISTORY: usize = 500;
const MAX_LOCAL_AUTH_CLOCK_DRIFT_SECS: i64 = 1;
const BUY_PREFLIGHT_CACHE_MAX_AGE_MS: i64 = 3_000;
const TAKER_HOLD_GUARD_TICKS: f64 = 1.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum OrderSide {
    Buy,
    Sell,
}

impl OrderSide {
    pub fn label(self) -> &'static str {
        match self {
            OrderSide::Buy => "BUY",
            OrderSide::Sell => "SELL",
        }
    }

    fn to_sdk(self) -> ClobSide {
        match self {
            OrderSide::Buy => ClobSide::Buy,
            OrderSide::Sell => ClobSide::Sell,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TradingMechanism {
    FastTaker,
    FastMaker,
}

impl TradingMechanism {
    pub fn label(self) -> &'static str {
        match self {
            TradingMechanism::FastTaker => "Fast Taker",
            TradingMechanism::FastMaker => "Fast Maker",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TradeSide {
    BuyUp,
    BuyDown,
    SellUp,
    SellDown,
}

impl TradeSide {
    pub fn label(self) -> &'static str {
        match self {
            TradeSide::BuyUp => "Buy UP",
            TradeSide::BuyDown => "Buy DOWN",
            TradeSide::SellUp => "Sell UP",
            TradeSide::SellDown => "Sell DOWN",
        }
    }

    pub fn order_side(self) -> OrderSide {
        match self {
            TradeSide::BuyUp | TradeSide::BuyDown => OrderSide::Buy,
            TradeSide::SellUp | TradeSide::SellDown => OrderSide::Sell,
        }
    }
}

use zeroize::Zeroize;

pub const NOMINAL_VALUES: &[f64] = &[0.5, 1.0, 2.0, 3.0, 4.0, 5.0];

pub struct TradingRuntimeConfig {
    pub signer_address: String,
    pub funder_address: String,
    pub private_key: String,
    pub signature_type: u8,
}

impl fmt::Debug for TradingRuntimeConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TradingRuntimeConfig")
            .field("signer_address", &self.signer_address)
            .field("funder_address", &self.funder_address)
            .field("private_key", &"<redacted>")
            .field("signature_type", &self.signature_type)
            .finish()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CredentialValidation {
    pub available_pusd: f64,
    pub regular_allowance_pusd: f64,
    pub neg_risk_allowance_pusd: f64,
}

impl Clone for TradingRuntimeConfig {
    fn clone(&self) -> Self {
        Self {
            signer_address: self.signer_address.clone(),
            funder_address: self.funder_address.clone(),
            private_key: self.private_key.clone(),
            signature_type: self.signature_type,
        }
    }
}

impl Zeroize for TradingRuntimeConfig {
    fn zeroize(&mut self) {
        self.signer_address.zeroize();
        self.funder_address.zeroize();
        self.private_key.zeroize();
        self.signature_type = 0;
    }
}

impl Drop for TradingRuntimeConfig {
    fn drop(&mut self) {
        self.zeroize();
    }
}

#[derive(Debug, Clone)]
pub struct TradeIntent {
    pub local_id: String,
    pub fingerprint: String,
    pub token_id: String,
    pub complement_token_id: Option<String>,
    pub trade_side: TradeSide,
    pub order_side: OrderSide,
    pub nominal_usd: f64,
    pub mechanism: TradingMechanism,
    pub market_slug: String,
    pub market_label: String,
    pub session_end_ms: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TradingOrderStatus {
    Submitting,
    RetryPending,
    Unresolved,
    Open,
    PartialFill,
    Filled,
    Cancelled,
    Expired,
    Rejected,
    Failed,
}

impl TradingOrderStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TradingOrderStatus::Filled
                | TradingOrderStatus::Cancelled
                | TradingOrderStatus::Expired
                | TradingOrderStatus::Rejected
                | TradingOrderStatus::Failed
        )
    }

    pub fn blocks_new_submissions(self) -> bool {
        matches!(
            self,
            TradingOrderStatus::Submitting
                | TradingOrderStatus::RetryPending
                | TradingOrderStatus::Unresolved
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradingLedgerEntry {
    pub local_id: String,
    pub order_id: Option<String>,
    #[serde(default)]
    pub fingerprint: String,
    pub session_slug: String,
    pub market_label: String,
    pub trade_side: TradeSide,
    pub order_side: OrderSide,
    pub mechanism: TradingMechanism,
    pub token_id: String,
    pub price: Option<f64>,
    pub shares: Option<f64>,
    pub nominal_usd: f64,
    pub status: TradingOrderStatus,
    pub detail: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InFlightIntent {
    pub local_id: String,
    pub fingerprint: String,
    pub session_slug: String,
    pub locked_until_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "payload")]
pub enum TradingEvent {
    LedgerLoaded {
        entries: Vec<TradingLedgerEntry>,
        timestamp_ms: i64,
    },
    LedgerUpsert {
        entry: TradingLedgerEntry,
    },
    BalanceUpdated {
        available_usdc: Option<f64>,
        allowance_usdc: Option<f64>,
        error: Option<String>,
        timestamp_ms: i64,
    },
    OrderPlaced {
        local_id: String,
        order_id: String,
        token_id: String,
        side: String,
        price: f64,
        size: f64,
        mechanism: String,
        timestamp_ms: i64,
    },
    OrderCancelled {
        order_id: String,
        reason: String,
        timestamp_ms: i64,
    },
    Error {
        message: String,
        timestamp_ms: i64,
    },
    Status {
        message: String,
        timestamp_ms: i64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradingState {
    pub enabled: bool,
    pub configured: bool,
    pub selected_nominal: f64,
    pub selected_mechanism: TradingMechanism,
    pub selected_side: Option<TradeSide>,
    pub order_status: String,
    pub ready_to_trade: bool,
    pub last_error: Option<String>,
    pub active_order_id: Option<String>,
    pub orders_placed: u64,
    pub orders_cancelled: u64,
    pub available_usdc: Option<f64>,
    pub allowance_usdc: Option<f64>,
    pub balance_updated_ms: Option<i64>,
    pub balance_error: Option<String>,
    pub ledger: Arc<Vec<TradingLedgerEntry>>,
    pub in_flight_intent: Option<InFlightIntent>,
    pub last_submitted_fingerprint: Option<String>,
    pub last_submitted_session_slug: Option<String>,
}

impl Default for TradingState {
    fn default() -> Self {
        Self {
            enabled: false,
            configured: false,
            selected_nominal: 1.0,
            selected_mechanism: TradingMechanism::FastTaker,
            selected_side: None,
            order_status: "Idle".to_string(),
            ready_to_trade: false,
            last_error: None,
            active_order_id: None,
            orders_placed: 0,
            orders_cancelled: 0,
            available_usdc: None,
            allowance_usdc: None,
            balance_updated_ms: None,
            balance_error: None,
            ledger: Arc::new(Vec::new()),
            in_flight_intent: None,
            last_submitted_fingerprint: None,
            last_submitted_session_slug: None,
        }
    }
}

impl TradingState {
    pub fn is_ready(&self) -> bool {
        self.enabled
            && self.configured
            && self.selected_side.is_some()
            && self.has_sufficient_allowance()
            && self.in_flight_intent.is_none()
    }

    fn has_sufficient_allowance(&self) -> bool {
        match self.selected_side {
            Some(TradeSide::BuyUp | TradeSide::BuyDown) => self
                .allowance_usdc
                .is_none_or(|allowance| allowance + 1e-9 >= self.selected_nominal),
            _ => true,
        }
    }

    fn approval_required_message(&self) -> Option<String> {
        match (self.selected_side, self.allowance_usdc) {
            (Some(TradeSide::BuyUp | TradeSide::BuyDown), Some(allowance))
                if allowance + 1e-9 < self.selected_nominal =>
            {
                Some(format!(
                    "Approval required: pUSD allowance ${allowance:.2} < ${:.2}",
                    self.selected_nominal
                ))
            }
            _ => None,
        }
    }

    fn update_ready_state(&mut self) {
        self.ready_to_trade = self.is_ready();
        if self.enabled
            && self.configured
            && self.selected_side.is_some()
            && self.in_flight_intent.is_none()
        {
            if let Some(message) = self.approval_required_message() {
                self.order_status = message;
            } else if self.order_status.starts_with("Approval required:") {
                self.order_status = "Trading armed".to_string();
            }
        }
    }

    pub fn set_configured(&mut self, configured: bool) {
        self.configured = configured;
        if !configured {
            self.ready_to_trade = false;
            if self.order_status == "Idle" {
                self.order_status = "Trading not configured".to_string();
            }
        } else {
            self.update_ready_state();
        }
    }

    pub fn reset_for_new_session(&mut self) {
        self.enabled = false;
        self.selected_nominal = 1.0;
        self.selected_mechanism = TradingMechanism::FastTaker;
        self.selected_side = None;
        self.ready_to_trade = false;
        self.last_error = None;
        self.active_order_id = None;
        self.in_flight_intent = None;
        self.last_submitted_fingerprint = None;
        self.last_submitted_session_slug = None;
        self.order_status = if self.configured {
            "Disabled".to_string()
        } else {
            "Trading not configured".to_string()
        };
    }

    pub fn upsert_ledger_entry(&mut self, entry: TradingLedgerEntry) {
        let mut ledger = (*self.ledger).clone();
        if let Some(existing) = ledger
            .iter_mut()
            .find(|candidate| candidate.local_id == entry.local_id)
        {
            *existing = entry;
        } else {
            ledger.push(entry);
        }
        ledger.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at_ms));
        ledger.truncate(MAX_TRADE_HISTORY);
        self.ledger = Arc::new(ledger);
    }

    pub fn replace_ledger(&mut self, mut entries: Vec<TradingLedgerEntry>) {
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at_ms));
        entries.truncate(MAX_TRADE_HISTORY);
        self.ledger = Arc::new(entries);
    }

    pub fn set_nominal(&mut self, nominal: f64) {
        if NOMINAL_VALUES
            .iter()
            .any(|candidate| (*candidate - nominal).abs() < f64::EPSILON)
        {
            self.selected_nominal = nominal;
            self.last_submitted_fingerprint = None;
            self.last_submitted_session_slug = None;
            self.update_ready_state();
        }
    }

    pub fn set_mechanism(&mut self, mechanism: TradingMechanism) {
        self.selected_mechanism = mechanism;
        self.last_submitted_fingerprint = None;
        self.last_submitted_session_slug = None;
        self.update_ready_state();
    }

    pub fn toggle_enabled(&mut self) {
        self.enabled = !self.enabled;
        if !self.enabled {
            self.selected_side = None;
            self.order_status = "Disabled".to_string();
        } else if self.configured {
            self.order_status = "Trading armed".to_string();
        } else {
            self.order_status = "Trading credentials missing".to_string();
        }
        self.update_ready_state();
    }

    pub fn set_side(&mut self, side: TradeSide) {
        if self.enabled {
            self.selected_side = Some(side);
        }
        self.last_submitted_fingerprint = None;
        self.last_submitted_session_slug = None;
        self.update_ready_state();
    }

    pub fn mark_intent_in_flight(
        &mut self,
        local_id: String,
        fingerprint: String,
        session_slug: String,
        now_ms: i64,
    ) {
        self.in_flight_intent = Some(InFlightIntent {
            local_id,
            fingerprint: fingerprint.clone(),
            session_slug: session_slug.clone(),
            locked_until_ms: now_ms + SUBMIT_LOCKOUT_MS,
        });
        self.last_submitted_fingerprint = Some(fingerprint);
        self.last_submitted_session_slug = Some(session_slug);
        self.ready_to_trade = false;
    }

    pub fn clear_in_flight_if(&mut self, local_id: &str) {
        if self
            .in_flight_intent
            .as_ref()
            .is_some_and(|intent| intent.local_id == local_id)
        {
            self.in_flight_intent = None;
            self.last_submitted_fingerprint = None;
            self.last_submitted_session_slug = None;
            self.update_ready_state();
        }
    }

    pub fn reconcile_submission_lock(&mut self, session_slug: &str, current_ms: i64) {
        let blocking_entry = self
            .ledger
            .iter()
            .filter(|entry| {
                entry.session_slug == session_slug && entry.status.blocks_new_submissions()
            })
            .max_by_key(|entry| entry.updated_at_ms)
            .cloned();

        if let Some(entry) = blocking_entry {
            self.in_flight_intent = Some(InFlightIntent {
                local_id: entry.local_id,
                fingerprint: entry.fingerprint.clone(),
                session_slug: entry.session_slug.clone(),
                locked_until_ms: current_ms.saturating_add(SUBMIT_LOCKOUT_MS),
            });
            self.last_submitted_fingerprint = Some(entry.fingerprint);
            self.last_submitted_session_slug = Some(entry.session_slug);
            self.ready_to_trade = false;
            if entry.status == TradingOrderStatus::Unresolved {
                self.order_status = entry.detail;
            }
        } else if self
            .in_flight_intent
            .as_ref()
            .is_some_and(|intent| intent.session_slug == session_slug)
        {
            self.in_flight_intent = None;
            self.last_submitted_fingerprint = None;
            self.last_submitted_session_slug = None;
            self.update_ready_state();
        }
    }

    pub fn duplicate_fingerprint_locked(&self, fingerprint: &str, session_slug: &str) -> bool {
        self.in_flight_intent.as_ref().is_some_and(|intent| {
            intent.fingerprint == fingerprint && intent.session_slug == session_slug
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BookResponse {
    #[serde(default)]
    bids: Vec<BookLevel>,
    #[serde(default)]
    asks: Vec<BookLevel>,
    min_order_size: Option<String>,
    tick_size: Option<String>,
    #[serde(default)]
    neg_risk: bool,
}

#[derive(Debug, Deserialize)]
struct PositionResponse {
    asset: String,
    size: f64,
}

#[derive(Debug, Deserialize)]
struct BookLevel {
    price: String,
    size: String,
}

#[derive(Debug, Clone, Copy)]
struct LiquidityLevel {
    price: f64,
    size: f64,
}

#[derive(Debug, Clone)]
struct BookSnapshot {
    direct_best_bid: Option<f64>,
    direct_best_ask: Option<f64>,
    complement_best_bid: Option<f64>,
    complement_best_ask: Option<f64>,
    best_bid: Option<f64>,
    best_ask: Option<f64>,
    buy_liquidity: Vec<LiquidityLevel>,
    sell_liquidity: Vec<LiquidityLevel>,
    tick_size: f64,
    min_order_size: f64,
    neg_risk: bool,
}

#[derive(Debug, Clone)]
struct ExecutionPlan {
    price: f64,
    shares: f64,
    order_type: ClobOrderType,
    amount: Amount,
    neg_risk: bool,
}

#[derive(Debug, Clone, Copy)]
struct BuyPreflightSnapshot {
    balance: f64,
    regular_allowance: f64,
    neg_risk_allowance: f64,
}

#[derive(Debug, Clone, Copy)]
struct TimedBuyPreflightSnapshot {
    snapshot: BuyPreflightSnapshot,
    fetched_at_ms: i64,
}

type SharedBuyPreflightCache = Arc<RwLock<Option<TimedBuyPreflightSnapshot>>>;

struct TradeExecutionContext<'a> {
    client: &'a AuthenticatedClient,
    signer: &'a PrivateKeySigner,
    public_http: &'a Client,
    funder_address: &'a str,
    signature_type_id: u8,
    buy_preflight_cache: &'a SharedBuyPreflightCache,
}

#[derive(Debug)]
struct OrderPostError {
    source: ClobSdkError,
}

impl fmt::Display for OrderPostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "failed posting signed order: {}", self.source)
    }
}

impl std::error::Error for OrderPostError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

fn ensure_session_open_at(session_end_ms: i64, current_ms: i64, phase: &str) -> Result<()> {
    if current_ms >= session_end_ms {
        return Err(anyhow!(
            "{phase} refused because the confirmed market session already ended"
        ));
    }
    Ok(())
}

fn ensure_session_open(intent: &TradeIntent, phase: &str) -> Result<()> {
    ensure_session_open_at(intent.session_end_ms, now_ms(), phase)
        .with_context(|| format!("session {}", intent.market_slug))
}

fn maker_expiration_for_deadline(now: DateTime<Utc>, session_end_ms: i64) -> Result<DateTime<Utc>> {
    let session_end = DateTime::<Utc>::from_timestamp_millis(session_end_ms)
        .context("invalid market session deadline")?;
    let expiration = now + TimeDelta::seconds(MAKER_GTD_EXPIRATION_LEAD_SECS);
    if expiration > session_end {
        return Err(anyhow!(
            "insufficient session lifetime for a maker order; use taker mode or wait for the next session"
        ));
    }
    Ok(expiration)
}

#[derive(Debug, Clone, Serialize)]
struct SmokeSummary {
    slug: String,
    outcome: String,
    side: String,
    mechanism: String,
    order_id: String,
    price: f64,
    shares: f64,
    token_id: String,
    status: String,
}

#[derive(Debug, Clone)]
struct TrackedOrder {
    entry: TradingLedgerEntry,
    /// Client-side cancellation target for short-lived post-only maker orders.
    cancel_at_ms: Option<i64>,
    /// Server-side GTD expiry or final reconciliation deadline.
    expires_at_ms: Option<i64>,
}

fn recover_tracked_orders(
    entries: &[TradingLedgerEntry],
    current_ms: i64,
) -> BTreeMap<String, TrackedOrder> {
    entries
        .iter()
        .filter(|entry| !entry.status.is_terminal())
        .filter_map(|entry| {
            let order_id = entry.order_id.clone()?;
            let cancel_at_ms = (matches!(entry.mechanism, TradingMechanism::FastMaker)
                && matches!(
                    entry.status,
                    TradingOrderStatus::Open | TradingOrderStatus::PartialFill
                ))
            .then_some(current_ms);
            Some((
                order_id,
                TrackedOrder {
                    entry: entry.clone(),
                    cancel_at_ms,
                    expires_at_ms: None,
                },
            ))
        })
        .collect()
}

fn mark_cancel_acknowledged(mut tracked: TrackedOrder, current_ms: i64) -> TrackedOrder {
    tracked.entry.status = TradingOrderStatus::Unresolved;
    tracked.entry.detail = "Cancellation acknowledged; reconciling final matched size".to_string();
    tracked.entry.updated_at_ms = current_ms;
    tracked.cancel_at_ms = None;
    tracked
}

fn mark_remote_lookup_unresolved(
    mut entry: TradingLedgerEntry,
    deadline_elapsed: bool,
    error: &impl fmt::Display,
    current_ms: i64,
) -> TradingLedgerEntry {
    entry.status = TradingOrderStatus::Unresolved;
    entry.detail = if deadline_elapsed {
        format!("Remote order status unresolved after the local deadline: {error}")
    } else {
        format!("Remote order status temporarily unavailable: {error}")
    };
    entry.updated_at_ms = current_ms;
    entry
}

#[derive(Clone)]
struct TradingWsClients {
    orders: WsClient<Authenticated<Normal>>,
    trades: WsClient<Authenticated<Normal>>,
}

struct TradingLedgerStore {
    path: PathBuf,
    entries: BTreeMap<String, TradingLedgerEntry>,
}

impl TradingLedgerStore {
    fn new(base_dir: PathBuf) -> Self {
        Self {
            path: base_dir.join("trades.ndjson"),
            entries: BTreeMap::new(),
        }
    }

    async fn load_today(&mut self) -> Result<Vec<TradingLedgerEntry>> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("failed creating {}", parent.display()))?;
        }
        if fs::metadata(&self.path).await.is_err() {
            return Ok(Vec::new());
        }

        let contents = fs::read_to_string(&self.path)
            .await
            .with_context(|| format!("failed reading {}", self.path.display()))?;
        for (line_number, line) in contents.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<TradingLedgerEntry>(line) {
                Ok(entry) => merge_ledger_entry(&mut self.entries, entry),
                Err(error) => warn!(
                    path = %self.path.display(),
                    line = line_number + 1,
                    %error,
                    "skipping malformed trade-history record"
                ),
            }
        }
        self.trim_entries();
        Ok(self.current_entries())
    }

    fn current_entries(&self) -> Vec<TradingLedgerEntry> {
        let mut entries = self.entries.values().cloned().collect::<Vec<_>>();
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at_ms));
        entries
    }

    async fn append(&mut self, entry: &TradingLedgerEntry) -> Result<()> {
        self.entries.insert(entry.local_id.clone(), entry.clone());
        self.trim_entries();
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("failed creating {}", parent.display()))?;
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await
            .with_context(|| format!("failed opening {}", self.path.display()))?;
        let mut encoded = serde_json::to_vec(entry)?;
        encoded.push(b'\n');
        file.write_all(&encoded).await?;
        file.flush().await?;
        Ok(())
    }

    fn trim_entries(&mut self) {
        if self.entries.len() <= MAX_TRADE_HISTORY {
            return;
        }
        let mut by_age = self
            .entries
            .iter()
            .map(|(local_id, entry)| (entry.updated_at_ms, local_id.clone()))
            .collect::<Vec<_>>();
        by_age.sort_unstable();
        let remove_count = by_age.len() - MAX_TRADE_HISTORY;
        for (_, local_id) in by_age.into_iter().take(remove_count) {
            self.entries.remove(&local_id);
        }
    }
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventBySlug {
    title: String,
    #[serde(default)]
    markets: Vec<EventMarket>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventMarket {
    question: Option<String>,
    outcomes: Option<String>,
    clob_token_ids: Option<String>,
}

pub fn runtime_config_from_args(args: &mut TradingArgs) -> Result<Option<TradingRuntimeConfig>> {
    let present = [
        args.signer_address.is_some(),
        args.funder_address.is_some(),
        args.private_key.is_some(),
        args.signature_type.is_some(),
    ];
    if present.iter().all(|value| !value) {
        return Ok(None);
    }
    if !present.iter().all(|value| *value) {
        if let Some(private_key) = args.private_key.as_mut() {
            private_key.zeroize();
        }
        args.private_key = None;
        return Err(anyhow!(
            "trading requires PM_SIGNER_ADDRESS, PM_FUNDER_ADDRESS, PM_PRIVATE_KEY, and PM_SIGNATURE_TYPE together"
        ));
    }

    let signature_type = args.signature_type.take().expect("presence checked");
    let mut private_key = args.private_key.take().expect("presence checked");
    if let Err(error) = signature_type_from_id(signature_type) {
        private_key.zeroize();
        return Err(error);
    }
    Ok(Some(TradingRuntimeConfig {
        signer_address: args.signer_address.take().expect("presence checked"),
        funder_address: args.funder_address.take().expect("presence checked"),
        private_key,
        signature_type,
    }))
}

pub fn inject_private_key_from_env(args: &mut TradingArgs) -> Result<()> {
    if args.private_key.is_some() {
        return Ok(());
    }
    let Some(value) = std::env::var_os("PM_PRIVATE_KEY") else {
        return Ok(());
    };
    let value = value
        .into_string()
        .map_err(|_| anyhow!("PM_PRIVATE_KEY is not valid Unicode"))?;
    if !value.trim().is_empty() {
        args.private_key = Some(value);
    }
    Ok(())
}

pub async fn run_trading_task(
    config: TradingRuntimeConfig,
    event_tx: mpsc::Sender<TradingEvent>,
    mut shutdown: broadcast::Receiver<()>,
    mut order_rx: mpsc::Receiver<TradeIntent>,
    results_dir: PathBuf,
) {
    let public_http = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("public trading HTTP client");
    let buy_preflight_cache: SharedBuyPreflightCache = Arc::new(RwLock::new(None));
    let balance_refresh_in_flight = Arc::new(AtomicBool::new(false));

    let mut ledger_store = TradingLedgerStore::new(results_dir);
    let mut recovered_tracked_orders = BTreeMap::new();
    match ledger_store.load_today().await {
        Ok(entries) => {
            recovered_tracked_orders = recover_tracked_orders(&entries, now_ms());
            info!(entries = entries.len(), "Loaded trade history");
            let _ = event_tx
                .send(TradingEvent::LedgerLoaded {
                    entries,
                    timestamp_ms: now_ms(),
                })
                .await;
        }
        Err(error) => {
            warn!(error = %error, "Trading ledger load failed");
            let _ = event_tx
                .send(TradingEvent::Status {
                    message: format!("Trading ledger unavailable: {error}"),
                    timestamp_ms: now_ms(),
                })
                .await;
        }
    }
    let auth = authenticate_client(&config).await;
    let (mut client, mut signer) = match auth {
        Ok(authenticated) => authenticated,
        Err(error) => {
            let _ = event_tx
                .send(TradingEvent::Error {
                    message: format!("Failed to initialize trading client: {error}"),
                    timestamp_ms: now_ms(),
                })
                .await;
            return;
        }
    };

    let _ = event_tx
        .send(TradingEvent::Status {
            message: "Trading client initialized".to_string(),
            timestamp_ms: now_ms(),
        })
        .await;
    refresh_balance_and_cache(&client, &event_tx, &buy_preflight_cache).await;
    let mut ws_clients = match build_trading_ws_clients(&client).await {
        Ok(clients) => {
            let _ = event_tx
                .send(TradingEvent::Status {
                    message: "Trading user websocket initialized".to_string(),
                    timestamp_ms: now_ms(),
                })
                .await;
            Some(clients)
        }
        Err(error) => {
            let _ = event_tx
                .send(TradingEvent::Status {
                    message: format!(
                        "Trading user websocket unavailable; fallback polling only: {error}"
                    ),
                    timestamp_ms: now_ms(),
                })
                .await;
            None
        }
    };
    let mut ws_order_rx: Option<mpsc::Receiver<WsOrderEvent>> = None;
    let mut ws_trade_rx: Option<mpsc::Receiver<WsTradeEvent>> = None;
    if let Some(ws) = ws_clients.take() {
        ws_order_rx = Some(spawn_ws_order_forwarder(ws.orders.clone()).await);
        ws_trade_rx = Some(spawn_ws_trade_forwarder(ws.trades.clone()).await);
    }

    let now = tokio::time::Instant::now();
    let balance_period = Duration::from_secs(BALANCE_POLL_SECS);
    let reconcile_period = Duration::from_secs(ORDER_RECONCILE_POLL_SECS);
    let mut balance_refresh = tokio::time::interval_at(now + balance_period, balance_period);
    let mut reconcile_refresh = tokio::time::interval_at(now + reconcile_period, reconcile_period);
    balance_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    reconcile_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut tracked_orders = recovered_tracked_orders;

    loop {
        tokio::select! {
            _ = shutdown.recv() => break,
            _ = balance_refresh.tick() => {
                spawn_balance_refresh_cached(
                    &client,
                    &event_tx,
                    &buy_preflight_cache,
                    &balance_refresh_in_flight,
                );
            }
            _ = reconcile_refresh.tick() => {
                if !tracked_orders.is_empty() {
                    reconcile_tracked_orders(
                        &client,
                        &event_tx,
                        &mut tracked_orders,
                        &mut ledger_store,
                    ).await;
                }
            }
            ws_order = recv_ws_order(&mut ws_order_rx), if ws_order_rx.is_some() => {
                match ws_order {
                    Some(Ok(order)) => {
                        apply_ws_order_update(&order, &event_tx, &mut tracked_orders, &mut ledger_store).await;
                    }
                    Some(Err(error)) => {
                        let _ = event_tx.send(TradingEvent::Status {
                            message: format!("Trading websocket order stream error; switching to polling fallback: {error}"),
                            timestamp_ms: now_ms(),
                        }).await;
                        ws_order_rx = None;
                    }
                    None => {
                        let _ = event_tx.send(TradingEvent::Status {
                            message: "Trading websocket order stream closed; switching to polling fallback".to_string(),
                            timestamp_ms: now_ms(),
                        }).await;
                        ws_order_rx = None;
                    }
                }
            }
            ws_trade = recv_ws_trade(&mut ws_trade_rx), if ws_trade_rx.is_some() => {
                match ws_trade {
                    Some(Ok(trade)) => {
                        apply_ws_trade_update(&trade, &event_tx, &mut tracked_orders, &mut ledger_store).await;
                    }
                    Some(Err(error)) => {
                        let _ = event_tx.send(TradingEvent::Status {
                            message: format!("Trading websocket trade stream error; switching to polling fallback: {error}"),
                            timestamp_ms: now_ms(),
                        }).await;
                        ws_trade_rx = None;
                    }
                    None => {
                        let _ = event_tx.send(TradingEvent::Status {
                            message: "Trading websocket trade stream closed; switching to polling fallback".to_string(),
                            timestamp_ms: now_ms(),
                        }).await;
                        ws_trade_rx = None;
                    }
                }
            }
            maybe_intent = order_rx.recv() => {
                let Some(intent) = maybe_intent else { break; };
                info!(
                    local_id = %intent.local_id,
                    token_id = %intent.token_id,
                    complement_token_id = ?intent.complement_token_id,
                    market = %intent.market_label,
                    side = %intent.order_side.label(),
                    mechanism = %intent.mechanism.label(),
                    nominal_usd = intent.nominal_usd,
                    "Received trade intent"
                );
                let mut local_entry = TradingLedgerEntry {
                    local_id: intent.local_id.clone(),
                    order_id: None,
                    fingerprint: intent.fingerprint.clone(),
                    session_slug: intent.market_slug.clone(),
                    market_label: intent.market_label.clone(),
                    trade_side: intent.trade_side,
                    order_side: intent.order_side,
                    mechanism: intent.mechanism,
                    token_id: intent.token_id.clone(),
                    price: None,
                    shares: None,
                    nominal_usd: intent.nominal_usd,
                    status: TradingOrderStatus::Submitting,
                    detail: "Submitting order".to_string(),
                    created_at_ms: now_ms(),
                    updated_at_ms: now_ms(),
                };
                persist_and_emit_ledger(&event_tx, &mut ledger_store, &local_entry).await;
                let _ = event_tx.send(TradingEvent::Status {
                    message: format!(
                        "Submitting {} {} ${:.2} (1/{RETRY_ATTEMPTS})",
                        intent.order_side.label(),
                        intent.market_label,
                        intent.nominal_usd
                    ),
                    timestamp_ms: now_ms(),
                }).await;

                let mut final_result = None;
                let mut final_unresolved = false;
                for attempt in 1..=RETRY_ATTEMPTS {
                    if now_ms() >= intent.session_end_ms {
                        final_result = Some(Err(anyhow!(
                            "order attempt stopped because session {} already ended",
                            intent.market_slug
                        )));
                        break;
                    }
                    let outcome = execute_trade_intent(
                        TradeExecutionContext {
                            client: &client,
                            signer: &signer,
                            public_http: &public_http,
                            funder_address: &config.funder_address,
                            signature_type_id: config.signature_type,
                            buy_preflight_cache: &buy_preflight_cache,
                        },
                        &intent,
                    )
                    .await;
                    match outcome {
                        Ok(summary) => {
                            final_result = Some(Ok(summary));
                            break;
                        }
                        Err(error) => {
                            let retry_class = classify_trade_retry(&error);
                            if matches!(retry_class, TradeRetryClass::Unresolved) {
                                final_unresolved = true;
                                final_result = Some(Err(error));
                                break;
                            }
                            if attempt >= RETRY_ATTEMPTS
                                || matches!(retry_class, TradeRetryClass::None)
                            {
                                final_result = Some(Err(error));
                                break;
                            }
                            let auth_retry = matches!(retry_class, TradeRetryClass::Reauthenticate);
                            local_entry.status = TradingOrderStatus::RetryPending;
                            local_entry.detail = if auth_retry {
                                format!("Retrying after auth/signing failure: {error:#}")
                            } else {
                                format!("Retrying after transient post failure: {error:#}")
                            };
                            local_entry.updated_at_ms = now_ms();
                            persist_and_emit_ledger(&event_tx, &mut ledger_store, &local_entry).await;
                            let _ = event_tx.send(TradingEvent::Status {
                                message: if auth_retry {
                                    format!("Retrying same order after auth/signing failure ({}/{})", attempt + 1, RETRY_ATTEMPTS)
                                } else {
                                    format!("Retrying same order with fresh book ({}/{})", attempt + 1, RETRY_ATTEMPTS)
                                },
                                timestamp_ms: now_ms(),
                            }).await;
                            if auth_retry {
                                match authenticate_client(&config).await {
                                    Ok((new_client, new_signer)) => {
                                        client = new_client;
                                        signer = new_signer;
                                        *buy_preflight_cache.write().await = None;
                                        refresh_balance_and_cache(
                                            &client,
                                            &event_tx,
                                            &buy_preflight_cache,
                                        ).await;
                                        if let Ok(new_ws_clients) = build_trading_ws_clients(&client).await {
                                            ws_order_rx = Some(spawn_ws_order_forwarder(new_ws_clients.orders.clone()).await);
                                            ws_trade_rx = Some(spawn_ws_trade_forwarder(new_ws_clients.trades.clone()).await);
                                        }
                                    }
                                    Err(auth_error) => {
                                        final_result = Some(Err(anyhow!("failed to re-authenticate trading client: {auth_error}")));
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }

                match final_result.unwrap_or_else(|| Err(anyhow!("trading worker exited without result"))) {
                    Ok(summary) => {
                        if matches!(intent.order_side, OrderSide::Buy) {
                            reserve_buy_preflight_balance(
                                &buy_preflight_cache,
                                intent.nominal_usd,
                            ).await;
                        }
                        info!(
                            order_id = %summary.order_id,
                            token_id = %summary.token_id,
                            price = summary.price,
                            shares = summary.shares,
                            "Trade intent posted successfully"
                        );
                        let _ = event_tx.send(TradingEvent::OrderPlaced {
                            local_id: intent.local_id.clone(),
                            order_id: summary.order_id.clone(),
                            token_id: intent.token_id.clone(),
                            side: intent.order_side.label().to_string(),
                            price: summary.price,
                            size: summary.shares,
                            mechanism: intent.mechanism.label().to_string(),
                            timestamp_ms: now_ms(),
                        }).await;
                        local_entry.order_id = Some(summary.order_id.clone());
                        local_entry.price = Some(summary.price);
                        local_entry.shares = Some(summary.shares);
                        local_entry.updated_at_ms = now_ms();
                        if matches!(intent.mechanism, TradingMechanism::FastMaker) {
                            local_entry.status = TradingOrderStatus::Open;
                            local_entry.detail = format!("Open on book at {:.4}", summary.price);
                            tracked_orders.insert(
                                summary.order_id.clone(),
                                TrackedOrder {
                                    entry: local_entry.clone(),
                                    cancel_at_ms: Some(now_ms() + MAKER_TIMEOUT_SECS * 1000),
                                    expires_at_ms: Some(intent.session_end_ms),
                                },
                            );
                            persist_and_emit_ledger(&event_tx, &mut ledger_store, &local_entry).await;
                            let _ = event_tx.send(TradingEvent::Status {
                                message: "Fast Maker order posted post-only with a 3s client cancel target and a venue expiration inside the active session".to_string(),
                                timestamp_ms: now_ms(),
                            }).await;
                        } else {
                            local_entry.status = TradingOrderStatus::Filled;
                            local_entry.detail = format!("Filled at {:.4}", summary.price);
                            tracked_orders.insert(
                                summary.order_id.clone(),
                                TrackedOrder {
                                    entry: local_entry.clone(),
                                    cancel_at_ms: None,
                                    expires_at_ms: Some(now_ms() + 60_000),
                                },
                            );
                            persist_and_emit_ledger(&event_tx, &mut ledger_store, &local_entry).await;
                        }
                        spawn_balance_refresh_cached(
                            &client,
                            &event_tx,
                            &buy_preflight_cache,
                            &balance_refresh_in_flight,
                        );
                    }
                    Err(error) => {
                        warn!(
                            local_id = %intent.local_id,
                            token_id = %intent.token_id,
                            market = %intent.market_label,
                            side = %intent.order_side.label(),
                            mechanism = %intent.mechanism.label(),
                            error = %error,
                            "Trade intent failed"
                        );
                        local_entry.status = if final_unresolved {
                            TradingOrderStatus::Unresolved
                        } else {
                            TradingOrderStatus::Failed
                        };
                        local_entry.detail = if final_unresolved {
                            format!(
                                "Order outcome unresolved; automatic retry refused: {error:#}"
                            )
                        } else {
                            format!("{error:#}")
                        };
                        local_entry.updated_at_ms = now_ms();
                        persist_and_emit_ledger(&event_tx, &mut ledger_store, &local_entry).await;
                        let _ = event_tx.send(TradingEvent::Error {
                            message: format!("{error:#}"),
                            timestamp_ms: now_ms(),
                        }).await;
                        spawn_balance_refresh_cached(
                            &client,
                            &event_tx,
                            &buy_preflight_cache,
                            &balance_refresh_in_flight,
                        );
                    }
                }
            }
        }
    }
}

pub async fn run_smoke(mut args: TradeSmokeArgs) -> Result<()> {
    let Some(config) = runtime_config_from_args(&mut args.trading)? else {
        return Err(anyhow!(
            "trade-smoke requires PM_SIGNER_ADDRESS, PM_FUNDER_ADDRESS, PM_PRIVATE_KEY, and PM_SIGNATURE_TYPE"
        ));
    };

    let public_http = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("failed to build public HTTP client")?;
    let (client, signer) = authenticate_client(&config).await?;
    let resolved = resolve_outcome_token(
        &public_http,
        &args.slug,
        args.market_query.as_deref(),
        &args.outcome,
    )
    .await?;
    let order_side = parse_order_side(&args.side)?;
    let mechanism = parse_mechanism(&args.mechanism)?;
    let intent = TradeIntent {
        local_id: Uuid::new_v4().to_string(),
        fingerprint: format!(
            "{}:{}:{:.2}:{}",
            args.slug,
            order_side.label(),
            args.nominal_usd,
            mechanism.label()
        ),
        token_id: resolved.token_id.clone(),
        complement_token_id: resolved.complement_token_id.clone(),
        trade_side: if matches!(order_side, OrderSide::Buy) {
            TradeSide::BuyUp
        } else {
            TradeSide::SellUp
        },
        order_side,
        nominal_usd: args.nominal_usd,
        mechanism,
        market_slug: args.slug.clone(),
        market_label: format!("{} {}", resolved.title, resolved.outcome_label),
        session_end_ms: now_ms() + 60_000,
    };
    let buy_preflight_cache = Arc::new(RwLock::new(None));
    let summary = execute_trade_intent(
        TradeExecutionContext {
            client: &client,
            signer: &signer,
            public_http: &public_http,
            funder_address: &config.funder_address,
            signature_type_id: config.signature_type,
            buy_preflight_cache: &buy_preflight_cache,
        },
        &intent,
    )
    .await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&SmokeSummary {
            slug: args.slug,
            outcome: resolved.outcome_label,
            side: intent.order_side.label().to_string(),
            mechanism: intent.mechanism.label().to_string(),
            order_id: summary.order_id,
            price: summary.price,
            shares: summary.shares,
            token_id: resolved.token_id,
            status: summary.status,
        })?
    );
    Ok(())
}

pub type AuthenticatedClient = ClobClient<Authenticated<Normal>>;
type WsOrderEvent = polymarket_client_sdk_v2::Result<WsOrderMessage>;
type WsTradeEvent = polymarket_client_sdk_v2::Result<WsTradeMessage>;

pub async fn authenticate_client(
    config: &TradingRuntimeConfig,
) -> Result<(AuthenticatedClient, PrivateKeySigner)> {
    let signer = PrivateKeySigner::from_str(config.private_key.trim())
        .context("invalid trading private key")?
        .with_chain_id(Some(POLYGON));

    let configured_signer: Address = config
        .signer_address
        .parse()
        .with_context(|| format!("invalid signer address {}", config.signer_address))?;
    if configured_signer != signer.address() {
        return Err(anyhow!(
            "PM_SIGNER_ADDRESS does not match the address derived from PM_PRIVATE_KEY"
        ));
    }

    let funder: Address = config
        .funder_address
        .parse()
        .with_context(|| format!("invalid funder address {}", config.funder_address))?;
    let signature_type = signature_type_from_id(config.signature_type)?;
    let use_server_time = resolve_server_time_mode().await;
    let sdk_config = ClobConfig::builder()
        .use_server_time(use_server_time)
        .build();
    let base_client = ClobClient::new(CLOB_API_URL, sdk_config)?;

    match base_client.check_geoblock().await {
        Ok(status) if status.blocked => {
            return Err(anyhow!(
                "Polymarket reports this execution location as blocked (country={}, region={})",
                status.country,
                status.region
            ));
        }
        Ok(status) => info!(
            country = %status.country,
            region = %status.region,
            "Polymarket geoblock advisory check passed"
        ),
        Err(error) => warn!(
            error = %error,
            "Polymarket geoblock advisory check failed; continuing without a hard block"
        ),
    }

    let mut auth = base_client
        .authentication_builder(&signer)
        .signature_type(signature_type);
    if signature_type == SignatureType::Eoa {
        if funder != signer.address() {
            return Err(anyhow!(
                "PM_FUNDER_ADDRESS must equal PM_SIGNER_ADDRESS for signature type 0 (EOA)"
            ));
        }
    } else {
        auth = auth.funder(funder);
    }
    let client = auth
        .authenticate()
        .await
        .context("failed to authenticate Polymarket CLOB V2 trading client")?;

    match client.version().await {
        Ok(version) => info!(version, "Pre-warmed CLOB protocol version cache"),
        Err(error) => warn!(
            error = %error,
            "CLOB version pre-warm failed; the first order build will retry lazily"
        ),
    }

    info!(
        signature_type = config.signature_type,
        "Authenticated CLOB V2 trading client"
    );
    Ok((client, signer))
}

pub async fn validate_trading_config(
    config: &TradingRuntimeConfig,
) -> Result<CredentialValidation> {
    let (client, _signer) = authenticate_client(config).await?;
    let collateral = fetch_buy_preflight(&client)
        .await
        .context("credentials authenticated, but the pUSD balance check failed")?;
    Ok(CredentialValidation {
        available_pusd: collateral.balance,
        regular_allowance_pusd: collateral.regular_allowance,
        neg_risk_allowance_pusd: collateral.neg_risk_allowance,
    })
}

async fn resolve_server_time_mode() -> bool {
    let probe = match ClobClient::new(CLOB_API_URL, ClobConfig::default()) {
        Ok(client) => client,
        Err(error) => {
            warn!(
                error = %error,
                "Unable to build CLOB clock probe; retaining per-request server time"
            );
            return true;
        }
    };
    let started_at_ms = Utc::now().timestamp_millis();
    let server_time = match probe.server_time().await {
        Ok(server_time) => server_time,
        Err(error) => {
            warn!(
                error = %error,
                "CLOB clock probe failed; retaining per-request server time"
            );
            return true;
        }
    };
    let completed_at_ms = Utc::now().timestamp_millis();
    let use_server_time = clock_requires_server_time(
        server_time,
        started_at_ms,
        completed_at_ms,
        MAX_LOCAL_AUTH_CLOCK_DRIFT_SECS,
    );
    let midpoint_seconds = ((started_at_ms + completed_at_ms) / 2).div_euclid(1_000);
    let drift_seconds = server_time - midpoint_seconds;
    info!(
        server_time,
        midpoint_seconds,
        drift_seconds,
        probe_rtt_ms = completed_at_ms.saturating_sub(started_at_ms),
        use_server_time,
        "Selected bounded CLOB authentication clock mode"
    );
    use_server_time
}

fn clock_requires_server_time(
    server_time_seconds: i64,
    request_started_at_ms: i64,
    request_completed_at_ms: i64,
    max_drift_seconds: i64,
) -> bool {
    if request_completed_at_ms < request_started_at_ms || max_drift_seconds < 0 {
        return true;
    }
    let midpoint_seconds =
        ((request_started_at_ms + request_completed_at_ms) / 2).div_euclid(1_000);
    server_time_seconds.abs_diff(midpoint_seconds) > max_drift_seconds as u64
}

fn signature_type_from_id(value: u8) -> Result<SignatureType> {
    match value {
        0 => Ok(SignatureType::Eoa),
        1 => Ok(SignatureType::Proxy),
        2 => Ok(SignatureType::GnosisSafe),
        3 => Ok(SignatureType::Poly1271),
        other => Err(anyhow!(
            "unsupported PM_SIGNATURE_TYPE {other}; expected 0, 1, 2, or 3"
        )),
    }
}

async fn build_trading_ws_clients(client: &AuthenticatedClient) -> Result<TradingWsClients> {
    let credentials = client.credentials().clone();
    let address = client.address();
    let orders = WsClient::default().authenticate(credentials.clone(), address)?;
    let trades = WsClient::default().authenticate(credentials, address)?;
    Ok(TradingWsClients { orders, trades })
}

async fn spawn_ws_order_forwarder(
    client: WsClient<Authenticated<Normal>>,
) -> mpsc::Receiver<WsOrderEvent> {
    let (tx, rx) = mpsc::channel(256);
    tokio::spawn(async move {
        match client.subscribe_orders(Vec::new()) {
            Ok(stream) => {
                let mut stream = std::pin::pin!(stream);
                while let Some(event) = stream.next().await {
                    if tx.send(event).await.is_err() {
                        break;
                    }
                }
            }
            Err(error) => {
                let _ = tx.send(Err(error)).await;
            }
        }
    });
    rx
}

async fn spawn_ws_trade_forwarder(
    client: WsClient<Authenticated<Normal>>,
) -> mpsc::Receiver<WsTradeEvent> {
    let (tx, rx) = mpsc::channel(256);
    tokio::spawn(async move {
        match client.subscribe_trades(Vec::new()) {
            Ok(stream) => {
                let mut stream = std::pin::pin!(stream);
                while let Some(event) = stream.next().await {
                    if tx.send(event).await.is_err() {
                        break;
                    }
                }
            }
            Err(error) => {
                let _ = tx.send(Err(error)).await;
            }
        }
    });
    rx
}

async fn execute_trade_intent(
    context: TradeExecutionContext<'_>,
    intent: &TradeIntent,
) -> Result<SmokeSummary> {
    ensure_session_open(intent, "order execution")?;
    let TradeExecutionContext {
        client,
        signer,
        public_http,
        funder_address,
        signature_type_id,
        buy_preflight_cache,
    } = context;

    if matches!(intent.order_side, OrderSide::Buy)
        && matches!(intent.mechanism, TradingMechanism::FastTaker)
        && intent.nominal_usd + f64::EPSILON < MIN_MARKETABLE_BUY_USD
    {
        return Err(anyhow!(
            "invalid amount for a marketable BUY order (${:.2}), min size: ${:.2}",
            intent.nominal_usd,
            MIN_MARKETABLE_BUY_USD
        ));
    }

    let book_future = resolve_book_snapshot(public_http, intent);
    let buy_preflight_future = async {
        if matches!(intent.order_side, OrderSide::Buy) {
            fetch_buy_preflight_cached(client, buy_preflight_cache)
                .await
                .map(Some)
        } else {
            Ok(None)
        }
    };
    let (book, buy_preflight) = tokio::join!(book_future, buy_preflight_future);
    let book = book?;
    let buy_preflight = buy_preflight?;
    ensure_session_open(intent, "post-preflight order execution")?;

    let token_id = U256::from_str(intent.token_id.trim())
        .with_context(|| format!("invalid CLOB token ID {}", intent.token_id))?;
    let tick_size = TickSize::try_from(decimal_from_f64(book.tick_size, 4)?.normalize())
        .context("unsupported CLOB tick size")?;
    client.set_tick_size(token_id, tick_size);
    client.set_neg_risk(token_id, book.neg_risk);

    let plan = build_execution_plan(intent, &book)?;
    info!(
        token_id = %intent.token_id,
        complement_token_id = ?intent.complement_token_id,
        direct_best_bid = ?book.direct_best_bid,
        direct_best_ask = ?book.direct_best_ask,
        complement_best_bid = ?book.complement_best_bid,
        complement_best_ask = ?book.complement_best_ask,
        resolved_best_bid = ?book.best_bid,
        resolved_best_ask = ?book.best_ask,
        tick_size = book.tick_size,
        min_order_size = book.min_order_size,
        price = plan.price,
        shares = plan.shares,
        "Resolved execution plan"
    );

    match intent.order_side {
        OrderSide::Buy => validate_buy_preflight(
            buy_preflight
                .as_ref()
                .context("buy preflight result missing")?,
            intent,
            &plan,
        )?,
        OrderSide::Sell => {
            preflight_sell(
                client,
                public_http,
                funder_address,
                signature_type_id,
                intent,
                &plan,
            )
            .await?;
        }
    }
    ensure_session_open(intent, "order construction")?;

    let order = match (intent.order_side, intent.mechanism) {
        (OrderSide::Buy, TradingMechanism::FastTaker)
        | (OrderSide::Sell, TradingMechanism::FastTaker) => {
            let worst_price = price_decimal_for_tick(plan.price, book.tick_size)?;
            client
                .market_order()
                .token_id(token_id)
                .amount(plan.amount)
                .price(worst_price)
                .side(intent.order_side.to_sdk())
                .order_type(plan.order_type)
                .build()
                .await?
        }
        (_, TradingMechanism::FastMaker) => {
            let price = price_decimal_for_tick(plan.price, book.tick_size)?;
            let size = decimal_from_f64(plan.shares, SHARE_SCALE)?;
            let expiration = maker_expiration_for_deadline(Utc::now(), intent.session_end_ms)?;
            client
                .limit_order()
                .token_id(token_id)
                .side(intent.order_side.to_sdk())
                .order_type(plan.order_type)
                .expiration(expiration)
                .post_only(true)
                .price(price)
                .size(size)
                .build()
                .await?
        }
    };

    ensure_session_open(intent, "order signing")?;
    let signed = client.sign(signer, order).await?;
    ensure_session_open(intent, "order submission")?;
    let response = client
        .post_order(signed)
        .await
        .map_err(|source| anyhow::Error::new(OrderPostError { source }))?;
    if !response.success {
        return Err(anyhow!(
            "order rejected by CLOB for {}: {:?}",
            intent.market_label,
            response
        ));
    }

    let making_amount = response.making_amount.to_string().parse::<f64>().ok();
    let taking_amount = response.taking_amount.to_string().parse::<f64>().ok();
    let (reported_price, reported_shares) = making_amount
        .zip(taking_amount)
        .and_then(|(making, taking)| derive_execution_fill(intent.order_side, making, taking))
        .unwrap_or((plan.price, plan.shares));
    info!(
        token_id = %intent.token_id,
        worst_price_limit = plan.price,
        reported_price,
        reported_shares,
        "Resolved CLOB response fill amounts"
    );

    Ok(SmokeSummary {
        slug: intent.market_slug.clone(),
        outcome: intent.market_label.clone(),
        side: intent.order_side.label().to_string(),
        mechanism: intent.mechanism.label().to_string(),
        order_id: response.order_id.to_string(),
        price: reported_price,
        shares: reported_shares,
        token_id: intent.token_id.clone(),
        status: response.status.to_string(),
    })
}
fn derive_execution_fill(
    side: OrderSide,
    making_amount: f64,
    taking_amount: f64,
) -> Option<(f64, f64)> {
    if !making_amount.is_finite()
        || !taking_amount.is_finite()
        || making_amount <= 0.0
        || taking_amount <= 0.0
    {
        return None;
    }
    let (price, shares) = match side {
        OrderSide::Buy => (making_amount / taking_amount, taking_amount),
        OrderSide::Sell => (taking_amount / making_amount, making_amount),
    };
    (price.is_finite() && shares.is_finite() && (0.0..=1.0).contains(&price))
        .then_some((price, shares))
}

async fn refresh_balance_and_cache(
    client: &AuthenticatedClient,
    event_tx: &mpsc::Sender<TradingEvent>,
    cache: &SharedBuyPreflightCache,
) {
    let event = match fetch_buy_preflight(client).await {
        Ok(snapshot) => {
            *cache.write().await = Some(TimedBuyPreflightSnapshot {
                snapshot,
                fetched_at_ms: now_ms(),
            });
            balance_updated_event(snapshot)
        }
        Err(error) => TradingEvent::BalanceUpdated {
            available_usdc: None,
            allowance_usdc: None,
            error: Some(format!("balance unavailable: {error}")),
            timestamp_ms: now_ms(),
        },
    };
    let _ = event_tx.send(event).await;
}

fn balance_updated_event(snapshot: BuyPreflightSnapshot) -> TradingEvent {
    TradingEvent::BalanceUpdated {
        available_usdc: Some(snapshot.balance),
        allowance_usdc: Some(snapshot.regular_allowance),
        error: None,
        timestamp_ms: now_ms(),
    }
}

async fn persist_and_emit_ledger(
    event_tx: &mpsc::Sender<TradingEvent>,
    ledger_store: &mut TradingLedgerStore,
    entry: &TradingLedgerEntry,
) {
    if let Err(error) = ledger_store.append(entry).await {
        let _ = event_tx
            .send(TradingEvent::Status {
                message: format!("Trading ledger append failed: {error}"),
                timestamp_ms: now_ms(),
            })
            .await;
    }
    let _ = event_tx
        .send(TradingEvent::LedgerUpsert {
            entry: entry.clone(),
        })
        .await;
}

fn spawn_balance_refresh_cached(
    client: &AuthenticatedClient,
    event_tx: &mpsc::Sender<TradingEvent>,
    cache: &SharedBuyPreflightCache,
    in_flight: &Arc<AtomicBool>,
) {
    if in_flight
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let client = client.clone();
    let event_tx = event_tx.clone();
    let cache = Arc::clone(cache);
    let in_flight = Arc::clone(in_flight);
    tokio::spawn(async move {
        refresh_balance_and_cache(&client, &event_tx, &cache).await;
        in_flight.store(false, Ordering::Release);
    });
}

async fn recv_ws_order(rx: &mut Option<mpsc::Receiver<WsOrderEvent>>) -> Option<WsOrderEvent> {
    match rx.as_mut() {
        Some(rx) => rx.recv().await,
        None => None,
    }
}

async fn recv_ws_trade(rx: &mut Option<mpsc::Receiver<WsTradeEvent>>) -> Option<WsTradeEvent> {
    match rx.as_mut() {
        Some(rx) => rx.recv().await,
        None => None,
    }
}

fn decimal_to_f64(value: &Decimal) -> Option<f64> {
    value.to_string().parse::<f64>().ok()
}

fn normalized_ws_order_status(
    order: &WsOrderMessage,
    fallback_shares: Option<f64>,
) -> TradingOrderStatus {
    let matched = order
        .size_matched
        .as_ref()
        .and_then(decimal_to_f64)
        .unwrap_or(0.0);
    let original = order
        .original_size
        .as_ref()
        .and_then(decimal_to_f64)
        .or(fallback_shares)
        .unwrap_or(0.0);
    if matches!(
        order.msg_type.as_ref(),
        Some(OrderMessageType::Cancellation)
    ) {
        return TradingOrderStatus::Cancelled;
    }
    if original > 0.0 && matched >= original - 1e-6 {
        return TradingOrderStatus::Filled;
    }
    if matched > 0.0 {
        return TradingOrderStatus::PartialFill;
    }
    TradingOrderStatus::Open
}

fn ws_order_message_type_label(msg_type: Option<&OrderMessageType>) -> &str {
    match msg_type {
        Some(OrderMessageType::Placement) => "PLACEMENT",
        Some(OrderMessageType::Update) => "UPDATE",
        Some(OrderMessageType::Cancellation) => "CANCELLATION",
        Some(OrderMessageType::Unknown(value)) => value.as_str(),
        None => "UPDATE",
        _ => "UPDATE",
    }
}

async fn apply_ws_order_update(
    order: &WsOrderMessage,
    event_tx: &mpsc::Sender<TradingEvent>,
    tracked_orders: &mut BTreeMap<String, TrackedOrder>,
    ledger_store: &mut TradingLedgerStore,
) -> bool {
    let Some(tracked) = tracked_orders.get(&order.id).cloned() else {
        return false;
    };
    let mut updated = tracked.entry.clone();
    let mut changed = false;
    let now = now_ms();
    updated.updated_at_ms = now;

    if let Some(price) = decimal_to_f64(&order.price)
        && updated
            .price
            .map(|current| (current - price).abs() > 1e-9)
            .unwrap_or(true)
    {
        updated.price = Some(price);
        changed = true;
    }
    if let Some(original) = order.original_size.as_ref().and_then(decimal_to_f64) {
        let merged = updated
            .shares
            .map_or(original, |current| current.max(original));
        if updated
            .shares
            .map(|current| (current - merged).abs() > 1e-9)
            .unwrap_or(true)
        {
            updated.shares = Some(merged);
            changed = true;
        }
    }

    let status = normalized_ws_order_status(order, updated.shares);
    if status != updated.status {
        updated.status = status;
        changed = true;
    }

    let matched = order
        .size_matched
        .as_ref()
        .and_then(decimal_to_f64)
        .unwrap_or(0.0);
    let detail = format!(
        "WS order {} (matched {:.2}/{:.2})",
        ws_order_message_type_label(order.msg_type.as_ref()),
        matched,
        updated.shares.unwrap_or(0.0)
    );
    if detail != updated.detail {
        updated.detail = detail;
        changed = true;
    }

    if changed {
        persist_and_emit_ledger(event_tx, ledger_store, &updated).await;
    }
    if updated.status.is_terminal() {
        tracked_orders.remove(&order.id);
    } else if let Some(state) = tracked_orders.get_mut(&order.id) {
        state.entry = updated;
    }
    changed
}

fn normalized_ws_trade_status(
    trade: &WsTradeMessage,
    matched: f64,
    total: f64,
) -> TradingOrderStatus {
    match &trade.status {
        TradeMessageStatus::Failed => return TradingOrderStatus::Rejected,
        TradeMessageStatus::Unknown(value) if value.to_ascii_uppercase().contains("CANCEL") => {
            return TradingOrderStatus::Cancelled;
        }
        _ => {}
    }
    if total > 0.0 && matched >= total - 1e-6 {
        return TradingOrderStatus::Filled;
    }
    if matched > 0.0 {
        return TradingOrderStatus::PartialFill;
    }
    TradingOrderStatus::Open
}

async fn apply_ws_trade_update(
    trade: &WsTradeMessage,
    event_tx: &mpsc::Sender<TradingEvent>,
    tracked_orders: &mut BTreeMap<String, TrackedOrder>,
    ledger_store: &mut TradingLedgerStore,
) -> bool {
    let mut order_matches = BTreeMap::<String, f64>::new();
    if let Some(order_id) = &trade.taker_order_id
        && tracked_orders.contains_key(order_id)
    {
        let matched = decimal_to_f64(&trade.size).unwrap_or(0.0);
        order_matches.insert(order_id.clone(), matched);
    }
    for maker in &trade.maker_orders {
        if tracked_orders.contains_key(&maker.order_id) {
            let matched = decimal_to_f64(&maker.matched_amount).unwrap_or(0.0);
            order_matches.insert(maker.order_id.clone(), matched);
        }
    }

    let mut any_changed = false;
    for (order_id, matched) in order_matches {
        let Some(tracked) = tracked_orders.get(&order_id).cloned() else {
            continue;
        };

        let mut updated = tracked.entry.clone();
        let mut changed = false;
        updated.updated_at_ms = now_ms();

        if let Some(price) = decimal_to_f64(&trade.price)
            && updated
                .price
                .map(|current| (current - price).abs() > 1e-9)
                .unwrap_or(true)
        {
            updated.price = Some(price);
            changed = true;
        }

        let total = updated.shares.unwrap_or(0.0);
        let status = normalized_ws_trade_status(trade, matched, total);
        if status != updated.status {
            updated.status = status;
            changed = true;
        }

        let detail = format!(
            "WS trade {:?} (matched {:.2}/{:.2})",
            trade.status, matched, total
        );
        if detail != updated.detail {
            updated.detail = detail;
            changed = true;
        }

        if changed {
            persist_and_emit_ledger(event_tx, ledger_store, &updated).await;
            any_changed = true;
        }
        if updated.status.is_terminal() {
            tracked_orders.remove(&order_id);
        } else if let Some(state) = tracked_orders.get_mut(&order_id) {
            state.entry = updated;
        }
    }

    any_changed
}

async fn reconcile_tracked_orders(
    client: &AuthenticatedClient,
    event_tx: &mpsc::Sender<TradingEvent>,
    tracked_orders: &mut BTreeMap<String, TrackedOrder>,
    ledger_store: &mut TradingLedgerStore,
) {
    let now = now_ms();
    let tracked_ids = tracked_orders.keys().cloned().collect::<Vec<_>>();
    for order_id in tracked_ids {
        let Some(mut tracked) = tracked_orders.get(&order_id).cloned() else {
            continue;
        };
        if tracked
            .cancel_at_ms
            .is_some_and(|cancel_at_ms| now >= cancel_at_ms)
        {
            match client.cancel_order(&order_id).await {
                Ok(response) if response.canceled.iter().any(|id| id == &order_id) => {
                    tracked = mark_cancel_acknowledged(tracked, now_ms());
                    persist_and_emit_ledger(event_tx, ledger_store, &tracked.entry).await;
                    tracked_orders.insert(order_id.clone(), tracked.clone());
                }
                Ok(response) => {
                    warn!(
                        order_id = %order_id,
                        reason = ?response.not_canceled.get(&order_id),
                        "Fast Maker cancellation was not confirmed; reconciling remote status"
                    );
                    tracked.cancel_at_ms = Some(now + 5_000);
                    tracked_orders.insert(order_id.clone(), tracked.clone());
                }
                Err(error) => {
                    warn!(
                        order_id = %order_id,
                        error = %error,
                        "Fast Maker cancellation request failed; will reconcile and retry"
                    );
                    tracked.cancel_at_ms = Some(now + 5_000);
                    tracked_orders.insert(order_id.clone(), tracked.clone());
                }
            }
        }
        match client.order(&order_id).await {
            Ok(order) => {
                let updated = reconcile_entry_from_remote(tracked.entry.clone(), &order, now);
                let changed = updated.status != tracked.entry.status
                    || updated.detail != tracked.entry.detail
                    || updated.price != tracked.entry.price
                    || updated.shares != tracked.entry.shares;

                if changed {
                    persist_and_emit_ledger(event_tx, ledger_store, &updated).await;
                }
                if updated.status.is_terminal() {
                    tracked_orders.remove(&order_id);
                } else if let Some(state) = tracked_orders.get_mut(&order_id) {
                    state.entry = updated;
                }
            }
            Err(error) => {
                let deadline_elapsed = tracked
                    .expires_at_ms
                    .map(|deadline| now > deadline + 1_000)
                    .unwrap_or(false);
                let unresolved = mark_remote_lookup_unresolved(
                    tracked.entry.clone(),
                    deadline_elapsed,
                    &error,
                    now,
                );
                if unresolved.status != tracked.entry.status
                    || unresolved.detail != tracked.entry.detail
                {
                    persist_and_emit_ledger(event_tx, ledger_store, &unresolved).await;
                }
                if let Some(state) = tracked_orders.get_mut(&order_id) {
                    state.entry = unresolved;
                }
            }
        }
    }
}

fn reconcile_entry_from_remote(
    mut entry: TradingLedgerEntry,
    remote: &OpenOrderResponse,
    now: i64,
) -> TradingLedgerEntry {
    let original_size = remote
        .original_size
        .to_string()
        .parse::<f64>()
        .ok()
        .or(entry.shares)
        .unwrap_or(0.0);
    let matched = remote
        .size_matched
        .to_string()
        .parse::<f64>()
        .unwrap_or(0.0);
    entry.order_id = Some(remote.id.clone());
    entry.price = remote.price.to_string().parse::<f64>().ok().or(entry.price);
    entry.shares = Some(original_size.max(entry.shares.unwrap_or(0.0)));
    entry.updated_at_ms = now;
    entry.status = match remote.status {
        OrderStatusType::Canceled => TradingOrderStatus::Cancelled,
        OrderStatusType::Matched if original_size > 0.0 && matched >= original_size - 1e-6 => {
            TradingOrderStatus::Filled
        }
        OrderStatusType::Matched => TradingOrderStatus::PartialFill,
        OrderStatusType::Live
        | OrderStatusType::Delayed
        | OrderStatusType::Unmatched
        | OrderStatusType::Unknown(_) => {
            if matched > 0.0 {
                TradingOrderStatus::PartialFill
            } else {
                TradingOrderStatus::Open
            }
        }
        _ => TradingOrderStatus::Open,
    };
    entry.detail = format!(
        "Remote {:?} (matched {:.2}/{:.2})",
        remote.status, matched, original_size
    );
    entry
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TradeRetryClass {
    None,
    RefreshBook,
    Reauthenticate,
    Unresolved,
}

fn classify_trade_retry(error: &anyhow::Error) -> TradeRetryClass {
    if let Some(class) = classify_structured_clob_retry(error) {
        return class;
    }
    let message = format!("{error:#}").to_ascii_lowercase();
    if message.contains("insufficient")
        || message.contains("min order")
        || message.contains("invalid amount")
        || message.contains("no executable price")
        || message.contains("historical / old session")
    {
        return TradeRetryClass::None;
    }

    // Authentication matching must remain deliberately narrow. In particular,
    // the wrapper text "failed posting signed order" is present for ordinary
    // exchange rejections and must never trigger a full credential/WS rebuild.
    if message.contains("invalid signature")
        || message.contains("signature verification")
        || message.contains("unauthorized")
        || message.contains("authentication")
        || message.contains("invalid api key")
        || message.contains("api key expired")
        || message.contains("invalid credential")
        || message.contains("401")
        || message.contains("403")
    {
        return TradeRetryClass::Reauthenticate;
    }

    // FOK liquidity misses are definitive no-fill outcomes. A single immediate
    // fresh-book retry can still fill, but ambiguous transport/server failures
    // must never create a second financial submission.
    if message.contains("fok_order_not_filled")
        || message.contains("couldn't be fully filled")
        || message.contains("could not be fully filled")
        || message.contains("fully filled or killed")
    {
        return TradeRetryClass::RefreshBook;
    }
    if message.contains("timeout")
        || message.contains("timed out")
        || message.contains("tempor")
        || message.contains("connection")
    {
        return TradeRetryClass::RefreshBook;
    }

    TradeRetryClass::None
}

fn classify_structured_clob_retry(error: &anyhow::Error) -> Option<TradeRetryClass> {
    let post_attempted = error
        .chain()
        .any(|cause| cause.downcast_ref::<OrderPostError>().is_some());
    let sdk_error = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ClobSdkError>())?;
    if sdk_error.kind() == ClobErrorKind::Status {
        let status = sdk_error.downcast_ref::<ClobStatus>()?;
        let code = status.status_code.as_u16();
        if matches!(code, 401 | 403) {
            return Some(TradeRetryClass::Reauthenticate);
        }
        let body = status.message.to_ascii_uppercase();
        if body.contains("FOK_ORDER_NOT_FILLED_ERROR")
            || body.contains("FOK_ORDER_NOT_FILLED")
            || body.contains("COULDN'T BE FULLY FILLED")
            || body.contains("COULD NOT BE FULLY FILLED")
        {
            return Some(TradeRetryClass::RefreshBook);
        }
        if body.contains("INVALID_SIGNATURE")
            || body.contains("INVALID SIGNATURE")
            || body.contains("SIGNATURE_VERIFICATION")
            || body.contains("INVALID_API_KEY")
        {
            return Some(TradeRetryClass::Reauthenticate);
        }
        if code == 408 || code == 429 || code >= 500 {
            return Some(if post_attempted {
                TradeRetryClass::Unresolved
            } else {
                TradeRetryClass::RefreshBook
            });
        }
        // A typed non-auth 4xx response is a permanent validation/liquidity
        // outcome unless an exact retryable exchange code above says otherwise.
        return Some(TradeRetryClass::None);
    }
    if sdk_error.kind() == ClobErrorKind::Validation {
        return Some(TradeRetryClass::None);
    }
    if post_attempted {
        return Some(TradeRetryClass::Unresolved);
    }
    if error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|error| error.is_timeout() || error.is_connect())
    }) {
        return Some(TradeRetryClass::RefreshBook);
    }
    None
}

fn merge_ledger_entry(
    latest: &mut BTreeMap<String, TradingLedgerEntry>,
    entry: TradingLedgerEntry,
) {
    latest
        .entry(entry.local_id.clone())
        .and_modify(|current| {
            if current.updated_at_ms <= entry.updated_at_ms {
                *current = entry.clone();
            }
        })
        .or_insert(entry);
}
async fn resolve_book_snapshot(client: &Client, intent: &TradeIntent) -> Result<BookSnapshot> {
    fetch_book_snapshot(
        client,
        &intent.token_id,
        intent.complement_token_id.as_deref(),
    )
    .await
}
async fn fetch_book_snapshot(
    client: &Client,
    token_id: &str,
    complement_token_id: Option<&str>,
) -> Result<BookSnapshot> {
    let (book, complement) = match complement_token_id {
        Some(other) => {
            let (book, complement) = tokio::try_join!(
                fetch_single_book(client, token_id),
                fetch_single_book(client, other)
            )?;
            (book, Some(complement))
        }
        None => (fetch_single_book(client, token_id).await?, None),
    };
    let direct_bids = parse_book_levels(&book.bids)?;
    let direct_asks = parse_book_levels(&book.asks)?;
    let complement_bids = complement
        .as_ref()
        .map(|book| parse_book_levels(&book.bids))
        .transpose()?
        .unwrap_or_default();
    let complement_asks = complement
        .as_ref()
        .map(|book| parse_book_levels(&book.asks))
        .transpose()?
        .unwrap_or_default();
    let tick_size = book
        .tick_size
        .as_deref()
        .unwrap_or("0.01")
        .parse::<f64>()
        .context("invalid tick size")?;
    let min_order_size = book
        .min_order_size
        .as_deref()
        .unwrap_or("0")
        .parse::<f64>()
        .context("invalid min_order_size")?;
    build_book_snapshot(
        direct_bids,
        direct_asks,
        complement_bids,
        complement_asks,
        tick_size,
        min_order_size,
        book.neg_risk,
    )
}

fn parse_book_levels(levels: &[BookLevel]) -> Result<Vec<LiquidityLevel>> {
    levels
        .iter()
        .map(|level| {
            let price = level
                .price
                .parse::<f64>()
                .with_context(|| format!("invalid book price {}", level.price))?;
            let size = level
                .size
                .parse::<f64>()
                .with_context(|| format!("invalid book size {}", level.size))?;
            if !price.is_finite()
                || !size.is_finite()
                || !(0.0..=1.0).contains(&price)
                || size <= 0.0
            {
                return Err(anyhow!("invalid non-finite or non-positive book level"));
            }
            Ok(LiquidityLevel { price, size })
        })
        .collect()
}

fn build_book_snapshot(
    direct_bids: Vec<LiquidityLevel>,
    direct_asks: Vec<LiquidityLevel>,
    complement_bids: Vec<LiquidityLevel>,
    complement_asks: Vec<LiquidityLevel>,
    tick_size: f64,
    min_order_size: f64,
    neg_risk: bool,
) -> Result<BookSnapshot> {
    if !tick_size.is_finite() || tick_size <= 0.0 {
        return Err(anyhow!("invalid non-positive tick size"));
    }
    let direct_best_bid = direct_bids.iter().map(|level| level.price).reduce(f64::max);
    let direct_best_ask = direct_asks.iter().map(|level| level.price).reduce(f64::min);
    let complement_best_bid = complement_bids
        .iter()
        .map(|level| level.price)
        .reduce(f64::max);
    let complement_best_ask = complement_asks
        .iter()
        .map(|level| level.price)
        .reduce(f64::min);
    let best_bid = [
        direct_best_bid,
        complement_best_ask.map(|price| truncate_to_scale(1.0 - price, 4)),
    ]
    .into_iter()
    .flatten()
    .reduce(f64::max);
    let best_ask = [
        direct_best_ask,
        complement_best_bid.map(|price| truncate_to_scale(1.0 - price, 4)),
    ]
    .into_iter()
    .flatten()
    .reduce(f64::min);

    let mut buy_liquidity = direct_asks;
    buy_liquidity.extend(complement_bids.into_iter().map(|level| LiquidityLevel {
        price: truncate_to_scale(1.0 - level.price, 4),
        size: level.size,
    }));
    buy_liquidity.sort_by(|a, b| a.price.total_cmp(&b.price));
    let mut sell_liquidity = direct_bids;
    sell_liquidity.extend(complement_asks.into_iter().map(|level| LiquidityLevel {
        price: truncate_to_scale(1.0 - level.price, 4),
        size: level.size,
    }));
    sell_liquidity.sort_by(|a, b| b.price.total_cmp(&a.price));

    Ok(BookSnapshot {
        direct_best_bid,
        direct_best_ask,
        complement_best_bid,
        complement_best_ask,
        best_bid,
        best_ask,
        buy_liquidity,
        sell_liquidity,
        tick_size,
        min_order_size,
        neg_risk,
    })
}

async fn fetch_single_book(client: &Client, token_id: &str) -> Result<BookResponse> {
    client
        .get(format!("{CLOB_API_URL}/book"))
        .query(&[("token_id", token_id)])
        .send()
        .await
        .context("failed to fetch order book")?
        .error_for_status()
        .context("order book returned error")?
        .json()
        .await
        .context("failed to decode order book")
}

fn build_execution_plan(intent: &TradeIntent, book: &BookSnapshot) -> Result<ExecutionPlan> {
    let (raw_price, taker_shares) = match (intent.order_side, intent.mechanism) {
        (OrderSide::Buy, TradingMechanism::FastTaker) => {
            let depth_price = depth_worst_buy_price(&book.buy_liquidity, intent.nominal_usd)?;
            (depth_price + TAKER_HOLD_GUARD_TICKS * book.tick_size, None)
        }
        (OrderSide::Sell, TradingMechanism::FastTaker) => {
            let best_bid = book
                .best_bid
                .ok_or_else(|| anyhow!("No bid price available"))?;
            let requested_shares = truncate_to_scale(intent.nominal_usd / best_bid, SHARE_SCALE);
            let depth_price = depth_worst_sell_price(&book.sell_liquidity, requested_shares)?;
            (
                depth_price - TAKER_HOLD_GUARD_TICKS * book.tick_size,
                Some(requested_shares),
            )
        }
        (OrderSide::Buy, TradingMechanism::FastMaker) => (
            book.best_ask
                .ok_or_else(|| anyhow!("No ask price available"))?
                - MAKER_OFFSET,
            None,
        ),
        (OrderSide::Sell, TradingMechanism::FastMaker) => (
            book.best_bid
                .ok_or_else(|| anyhow!("No bid price available"))?
                + MAKER_OFFSET,
            None,
        ),
    };
    let price = match (intent.order_side, intent.mechanism) {
        (OrderSide::Buy, TradingMechanism::FastTaker) => {
            snap_price_to_tick_up(raw_price.clamp(MIN_PRICE, MAX_PRICE), book.tick_size)
        }
        (OrderSide::Sell, TradingMechanism::FastTaker) => {
            snap_price_to_tick_down(raw_price.clamp(MIN_PRICE, MAX_PRICE), book.tick_size)
        }
        _ => snap_price_to_tick(raw_price.clamp(MIN_PRICE, MAX_PRICE), book.tick_size),
    };
    if !(MIN_PRICE..=MAX_PRICE).contains(&price) {
        return Err(anyhow!("Derived price {price:.4} is out of bounds"));
    }

    let shares =
        taker_shares.unwrap_or_else(|| truncate_to_scale(intent.nominal_usd / price, SHARE_SCALE));
    if shares <= 0.0 {
        return Err(anyhow!("Calculated share size is zero"));
    }
    if shares < book.min_order_size {
        return Err(anyhow!(
            "Calculated size {:.3} is below min order size {:.3}",
            shares,
            book.min_order_size
        ));
    }

    let amount = match intent.order_side {
        OrderSide::Buy => Amount::usdc(decimal_from_f64(intent.nominal_usd, 2)?)?,
        OrderSide::Sell => Amount::shares(decimal_from_f64(shares, SHARE_SCALE)?)?,
    };

    let order_type = match intent.mechanism {
        TradingMechanism::FastTaker => ClobOrderType::FOK,
        TradingMechanism::FastMaker => ClobOrderType::GTD,
    };

    Ok(ExecutionPlan {
        price,
        shares,
        order_type,
        amount,
        neg_risk: book.neg_risk,
    })
}

fn depth_worst_buy_price(levels: &[LiquidityLevel], required_quote: f64) -> Result<f64> {
    if !required_quote.is_finite() || required_quote <= 0.0 {
        return Err(anyhow!("invalid buy quote amount"));
    }
    let mut remaining = required_quote;
    for level in levels {
        let quote_capacity = level.price * level.size;
        if quote_capacity + 1e-9 >= remaining {
            return Ok(level.price);
        }
        remaining -= quote_capacity;
    }
    Err(anyhow!(
        "insufficient visible ask depth for ${required_quote:.2} FOK buy"
    ))
}

fn depth_worst_sell_price(levels: &[LiquidityLevel], required_shares: f64) -> Result<f64> {
    if !required_shares.is_finite() || required_shares <= 0.0 {
        return Err(anyhow!("invalid sell share amount"));
    }
    let mut remaining = required_shares;
    for level in levels {
        if level.size + 1e-9 >= remaining {
            return Ok(level.price);
        }
        remaining -= level.size;
    }
    Err(anyhow!(
        "insufficient visible bid depth for {required_shares:.4} share FOK sell"
    ))
}

fn snap_price_to_tick(price: f64, tick_size: f64) -> f64 {
    let steps = (price / tick_size).round();
    truncate_to_scale((steps * tick_size).clamp(MIN_PRICE, MAX_PRICE), 4)
}

fn snap_price_to_tick_up(price: f64, tick_size: f64) -> f64 {
    let steps = ((price - tick_size * 1e-9) / tick_size).ceil();
    truncate_to_scale((steps * tick_size).clamp(MIN_PRICE, MAX_PRICE), 4)
}

fn snap_price_to_tick_down(price: f64, tick_size: f64) -> f64 {
    let steps = ((price + tick_size * 1e-9) / tick_size).floor();
    truncate_to_scale((steps * tick_size).clamp(MIN_PRICE, MAX_PRICE), 4)
}

fn truncate_to_scale(value: f64, scale: u32) -> f64 {
    let factor = 10_f64.powi(scale as i32);
    (value * factor).floor() / factor
}

fn decimal_from_f64(value: f64, scale: u32) -> Result<Decimal> {
    Decimal::from_str(&format!("{:.*}", scale as usize, value))
        .with_context(|| format!("failed to convert {value} into Decimal"))
}

fn price_decimal_for_tick(value: f64, tick_size: f64) -> Result<Decimal> {
    let tick = decimal_from_f64(tick_size, 4)?.normalize();
    decimal_from_f64(value, tick.scale())
}

async fn fetch_buy_preflight(client: &AuthenticatedClient) -> Result<BuyPreflightSnapshot> {
    let collateral = client
        .balance_allowance(BalanceAllowanceRequest::default())
        .await
        .context("failed to fetch collateral balance")?;
    Ok(BuyPreflightSnapshot {
        balance: parse_collateral_pusd(&collateral.balance.to_string())
            .context("invalid collateral balance")?,
        regular_allowance: parse_collateral_allowance_pusd(&collateral.allowances, false)
            .context("invalid regular collateral allowance")?,
        neg_risk_allowance: parse_collateral_allowance_pusd(&collateral.allowances, true)
            .context("invalid neg-risk collateral allowance")?,
    })
}

async fn fetch_buy_preflight_cached(
    client: &AuthenticatedClient,
    cache: &SharedBuyPreflightCache,
) -> Result<BuyPreflightSnapshot> {
    if let Some(cached) = *cache.read().await {
        let age_ms = now_ms().saturating_sub(cached.fetched_at_ms);
        if (0..=BUY_PREFLIGHT_CACHE_MAX_AGE_MS).contains(&age_ms) {
            info!(
                age_ms,
                "Using guarded cached trading balance/allowance preflight"
            );
            return Ok(cached.snapshot);
        }
    }
    let snapshot = fetch_buy_preflight(client).await?;
    *cache.write().await = Some(TimedBuyPreflightSnapshot {
        snapshot,
        fetched_at_ms: now_ms(),
    });
    Ok(snapshot)
}

async fn reserve_buy_preflight_balance(cache: &SharedBuyPreflightCache, nominal_usd: f64) {
    let mut guard = cache.write().await;
    if let Some(cached) = guard.as_mut() {
        cached.snapshot.balance = (cached.snapshot.balance - nominal_usd).max(0.0);
    }
}

fn validate_buy_preflight(
    collateral: &BuyPreflightSnapshot,
    intent: &TradeIntent,
    plan: &ExecutionPlan,
) -> Result<()> {
    if collateral.balance + 1e-9 < intent.nominal_usd {
        return Err(anyhow!(
            "insufficient collateral balance {:.4} for ${:.2} buy",
            collateral.balance,
            intent.nominal_usd
        ));
    }
    let allowance = if plan.neg_risk {
        collateral.neg_risk_allowance
    } else {
        collateral.regular_allowance
    };
    if allowance + 1e-9 < intent.nominal_usd {
        return Err(anyhow!(
            "Approval required: pUSD allowance ${allowance:.2} is below the ${:.2} order size; approve the correct CLOB V2 exchange for this wallet first",
            intent.nominal_usd
        ));
    }
    Ok(())
}

async fn preflight_sell(
    client: &AuthenticatedClient,
    public_http: &Client,
    funder_address: &str,
    signature_type_id: u8,
    intent: &TradeIntent,
    plan: &ExecutionPlan,
) -> Result<()> {
    let token_id = U256::from_str(intent.token_id.trim())
        .with_context(|| format!("invalid CLOB token ID {}", intent.token_id))?;
    let signature_type = signature_type_from_id(signature_type_id)?;
    let conditional_future = client.balance_allowance(
        BalanceAllowanceRequest::builder()
            .asset_type(AssetType::Conditional)
            .token_id(token_id)
            .signature_type(signature_type)
            .build(),
    );
    let position_future = fetch_position_size(public_http, funder_address, &intent.token_id);
    let (conditional, available_shares) = tokio::join!(conditional_future, position_future);
    conditional.context("failed to fetch conditional balance/allowance")?;
    let available_shares = available_shares?;
    if available_shares + 1e-6 < plan.shares {
        return Err(anyhow!(
            "insufficient position {:.4} shares for requested sell {:.4} shares",
            available_shares,
            plan.shares
        ));
    }

    Ok(())
}

fn parse_collateral_pusd(raw: &str) -> Result<f64> {
    let parsed = raw
        .trim()
        .parse::<f64>()
        .with_context(|| format!("invalid collateral balance {raw}"))?;
    // Polymarket V2 pUSD collateral balances use 6-decimal base units.
    Ok(parsed / 1_000_000.0)
}

fn parse_collateral_allowance_pusd(
    allowances: &HashMap<Address, String>,
    neg_risk: bool,
) -> Result<f64> {
    let spender = if neg_risk {
        POLYMARKET_NEG_RISK_CTF_EXCHANGE_V2_SPENDER
    } else {
        POLYMARKET_CTF_EXCHANGE_V2_SPENDER
    };
    let exchange =
        Address::from_str(spender).context("invalid CLOB V2 exchange spender address")?;
    if let Some(raw) = allowances.get(&exchange) {
        return parse_collateral_pusd(raw);
    }
    // Do not accept an allowance granted to a legacy or different exchange.
    Ok(0.0)
}

async fn fetch_position_size(client: &Client, user: &str, token_id: &str) -> Result<f64> {
    let positions: Vec<PositionResponse> = client
        .get(format!("{DATA_API_URL}/positions"))
        .query(&[("user", user), ("sizeThreshold", "0.01")])
        .send()
        .await
        .context("failed fetching user positions")?
        .error_for_status()
        .context("positions endpoint returned error")?
        .json()
        .await
        .context("failed decoding user positions")?;

    Ok(positions
        .into_iter()
        .find(|position| position.asset == token_id)
        .map(|position| position.size)
        .unwrap_or(0.0))
}

fn parse_order_side(raw: &str) -> Result<OrderSide> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "buy" => Ok(OrderSide::Buy),
        "sell" => Ok(OrderSide::Sell),
        other => Err(anyhow!("unsupported side {other}; expected buy or sell")),
    }
}

pub fn parse_mechanism(raw: &str) -> Result<TradingMechanism> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "fast-taker" | "taker" => Ok(TradingMechanism::FastTaker),
        "fast-maker" | "maker" => Ok(TradingMechanism::FastMaker),
        other => Err(anyhow!(
            "unsupported mechanism {other}; expected fast-taker or fast-maker"
        )),
    }
}

async fn resolve_outcome_token(
    client: &Client,
    slug: &str,
    market_query: Option<&str>,
    outcome: &str,
) -> Result<ResolvedOutcome> {
    let event: EventBySlug = client
        .get(format!(
            "https://gamma-api.polymarket.com/events/slug/{slug}"
        ))
        .send()
        .await
        .with_context(|| format!("failed fetching event for slug {slug}"))?
        .error_for_status()
        .with_context(|| format!("gamma returned error for slug {slug}"))?
        .json()
        .await
        .with_context(|| format!("failed decoding event for slug {slug}"))?;
    let normalized_outcome = normalize_outcome_label(outcome);

    let normalized_market_query = market_query.map(|value| value.trim().to_ascii_lowercase());
    let mut matches = Vec::new();

    for market in event.markets {
        let question = market.question.unwrap_or_else(|| event.title.clone());
        if let Some(query) = normalized_market_query.as_deref()
            && !question.to_ascii_lowercase().contains(query)
        {
            continue;
        }

        let outcomes = parse_json_string_array(market.outcomes.as_deref())?;
        let token_ids = parse_json_string_array(market.clob_token_ids.as_deref())?;
        for (candidate, token_id) in outcomes.iter().zip(token_ids.iter()) {
            if normalize_outcome_label(candidate) == normalized_outcome {
                matches.push(ResolvedOutcome {
                    title: question.clone(),
                    outcome_label: candidate.clone(),
                    token_id: token_id.clone(),
                    complement_token_id: outcomes.iter().zip(token_ids.iter()).find_map(
                        |(other_outcome, other_token_id)| {
                            if normalize_outcome_label(other_outcome) != normalized_outcome {
                                Some(other_token_id.clone())
                            } else {
                                None
                            }
                        },
                    ),
                });
            }
        }
    }

    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => Err(anyhow!(
            "could not resolve outcome {outcome} for event {slug}{}",
            normalized_market_query
                .as_deref()
                .map(|query| format!(" using market query '{query}'"))
                .unwrap_or_default()
        )),
        _ => Err(anyhow!(
            "outcome {outcome} matched multiple markets in event {slug}; pass --market-query to disambiguate"
        )),
    }
}

fn normalize_outcome_label(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "yes" | "up" => "yes".to_string(),
        "no" | "down" => "no".to_string(),
        other => other.to_string(),
    }
}

fn parse_json_string_array(raw: Option<&str>) -> Result<Vec<String>> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    serde_json::from_str(raw).context("failed parsing JSON string array")
}

#[derive(Debug)]
struct ResolvedOutcome {
    title: String,
    outcome_label: String,
    token_id: String,
    complement_token_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample_ledger_entry(local_id: &str, updated_at_ms: i64) -> TradingLedgerEntry {
        TradingLedgerEntry {
            local_id: local_id.to_string(),
            order_id: Some(format!("order-{local_id}")),
            fingerprint: "fp".to_string(),
            session_slug: "btc-updown-5m-123".to_string(),
            market_label: "Bitcoin Up or Down - March 24, 9:40AM-9:45AM ET DOWN".to_string(),
            trade_side: TradeSide::BuyDown,
            order_side: OrderSide::Buy,
            mechanism: TradingMechanism::FastTaker,
            token_id: "token-1".to_string(),
            price: Some(0.42),
            shares: Some(2.5),
            nominal_usd: 1.05,
            status: TradingOrderStatus::Filled,
            detail: "ok".to_string(),
            created_at_ms: updated_at_ms - 500,
            updated_at_ms,
        }
    }

    fn sample_trade_intent() -> TradeIntent {
        TradeIntent {
            local_id: "local-1".to_string(),
            fingerprint: "intent-fp".to_string(),
            token_id: "token-1".to_string(),
            complement_token_id: Some("token-complement".to_string()),
            trade_side: TradeSide::BuyUp,
            order_side: OrderSide::Buy,
            nominal_usd: 1.0,
            mechanism: TradingMechanism::FastTaker,
            market_slug: "btc-updown-5m-test".to_string(),
            market_label: "Bitcoin Up or Down test UP".to_string(),
            session_end_ms: now_ms() + 60_000,
        }
    }

    #[test]
    fn fok_liquidity_miss_retries_without_reauthentication() {
        let error = anyhow!(
            "failed posting signed order: ApiError(code=FOK_ORDER_NOT_FILLED, message=order couldn't be fully filled)"
        );
        assert_eq!(classify_trade_retry(&error), TradeRetryClass::RefreshBook);
    }

    #[test]
    fn typed_clob_status_drives_retry_without_wrapper_text() {
        let fok = anyhow::Error::new(ClobSdkError::status(
            polymarket_client_sdk_v2::error::StatusCode::BAD_REQUEST,
            polymarket_client_sdk_v2::error::Method::POST,
            "/order".to_string(),
            r#"{"error":"FOK_ORDER_NOT_FILLED_ERROR"}"#,
        ))
        .context("failed posting signed order");
        assert_eq!(classify_trade_retry(&fok), TradeRetryClass::RefreshBook);

        let unauthorized = anyhow::Error::new(ClobSdkError::status(
            polymarket_client_sdk_v2::error::StatusCode::UNAUTHORIZED,
            polymarket_client_sdk_v2::error::Method::POST,
            "/order".to_string(),
            "unauthorized",
        ))
        .context("failed posting signed order");
        assert_eq!(
            classify_trade_retry(&unauthorized),
            TradeRetryClass::Reauthenticate
        );

        let server_error = anyhow::Error::new(OrderPostError {
            source: ClobSdkError::status(
                polymarket_client_sdk_v2::error::StatusCode::INTERNAL_SERVER_ERROR,
                polymarket_client_sdk_v2::error::Method::POST,
                "/order".to_string(),
                "response unavailable after submit",
            ),
        });
        assert_eq!(
            classify_trade_retry(&server_error),
            TradeRetryClass::Unresolved
        );

        let pre_submit_server_error = anyhow::Error::new(ClobSdkError::status(
            polymarket_client_sdk_v2::error::StatusCode::INTERNAL_SERVER_ERROR,
            polymarket_client_sdk_v2::error::Method::GET,
            "/book".to_string(),
            "temporary server failure",
        ));
        assert_eq!(
            classify_trade_retry(&pre_submit_server_error),
            TradeRetryClass::RefreshBook
        );
    }

    #[test]
    fn pre_submit_transport_failure_can_retry_without_post_marker() {
        let error = anyhow!("book request timed out waiting for response");
        assert_eq!(classify_trade_retry(&error), TradeRetryClass::RefreshBook);
    }

    #[test]
    fn explicit_signature_failure_reauthenticates() {
        let error = anyhow!("CLOB rejected request: invalid signature");
        assert_eq!(
            classify_trade_retry(&error),
            TradeRetryClass::Reauthenticate
        );
    }

    #[test]
    fn permanent_order_validation_error_does_not_retry() {
        let error = anyhow!("invalid amount for a marketable BUY order");
        assert_eq!(classify_trade_retry(&error), TradeRetryClass::None);
    }

    #[test]
    fn session_deadline_is_enforced_at_the_exact_boundary() {
        ensure_session_open_at(10_000, 9_999, "test").expect("session remains open");
        let error = ensure_session_open_at(10_000, 10_000, "test")
            .expect_err("exact deadline must fail closed");
        assert!(error.to_string().contains("already ended"));
    }

    #[test]
    fn maker_expiration_never_crosses_the_session_deadline() {
        let now =
            DateTime::<Utc>::from_timestamp_millis(1_700_000_000_000).expect("fixed timestamp");
        let minimum_deadline =
            (now + TimeDelta::seconds(MAKER_GTD_EXPIRATION_LEAD_SECS)).timestamp_millis();
        let expiration =
            maker_expiration_for_deadline(now, minimum_deadline).expect("valid maker lifetime");
        assert_eq!(expiration.timestamp_millis(), minimum_deadline);
        assert!(
            maker_expiration_for_deadline(now, minimum_deadline - 1)
                .expect_err("short session must reject maker mode")
                .to_string()
                .contains("insufficient session lifetime")
        );
    }

    #[test]
    fn healthy_local_clock_avoids_per_request_time_fetch() {
        assert!(!clock_requires_server_time(
            1_700_000_000,
            1_699_999_999_900,
            1_700_000_000_100,
            MAX_LOCAL_AUTH_CLOCK_DRIFT_SECS,
        ));
    }

    #[test]
    fn drifted_or_invalid_clock_keeps_server_time_safety_mode() {
        assert!(clock_requires_server_time(
            1_700_000_004,
            1_699_999_999_900,
            1_700_000_000_100,
            MAX_LOCAL_AUTH_CLOCK_DRIFT_SECS,
        ));
        assert!(clock_requires_server_time(1_700_000_000, 2_000, 1_000, 1));
    }

    #[test]
    fn calculate_fast_taker_buy_uses_depth_cap_and_hold_guard() {
        let plan = build_execution_plan(
            &TradeIntent {
                local_id: "local".to_string(),
                fingerprint: "fp".to_string(),
                token_id: "1".to_string(),
                complement_token_id: Some("2".to_string()),
                trade_side: TradeSide::BuyUp,
                order_side: OrderSide::Buy,
                nominal_usd: 1.0,
                mechanism: TradingMechanism::FastTaker,
                market_slug: "x".to_string(),
                market_label: "buy".to_string(),
                session_end_ms: 60_000,
            },
            &BookSnapshot {
                direct_best_bid: Some(0.45),
                direct_best_ask: Some(0.48),
                complement_best_bid: Some(0.52),
                complement_best_ask: Some(0.55),
                best_bid: Some(0.45),
                best_ask: Some(0.48),
                buy_liquidity: vec![LiquidityLevel {
                    price: 0.48,
                    size: 100.0,
                }],
                sell_liquidity: vec![LiquidityLevel {
                    price: 0.45,
                    size: 100.0,
                }],
                tick_size: 0.01,
                min_order_size: 0.001,
                neg_risk: false,
            },
        )
        .unwrap();
        assert!((plan.price - 0.49).abs() < f64::EPSILON);
    }

    #[test]
    fn order_price_decimal_scale_matches_market_tick() {
        for (tick_size, price, expected_scale, expected) in [
            (0.1, 0.5, 1, "0.5"),
            (0.01, 0.5, 2, "0.50"),
            (0.001, 0.505, 3, "0.505"),
            (0.0001, 0.5051, 4, "0.5051"),
        ] {
            let decimal = price_decimal_for_tick(price, tick_size).expect("valid tick price");
            assert_eq!(decimal.scale(), expected_scale);
            assert_eq!(decimal.to_string(), expected);
        }
    }

    #[test]
    fn fast_taker_walks_visible_depth_before_applying_guard() {
        let mut intent = sample_trade_intent();
        intent.nominal_usd = 5.0;
        let book = BookSnapshot {
            direct_best_bid: Some(0.39),
            direct_best_ask: Some(0.40),
            complement_best_bid: None,
            complement_best_ask: None,
            best_bid: Some(0.39),
            best_ask: Some(0.40),
            buy_liquidity: vec![
                LiquidityLevel {
                    price: 0.40,
                    size: 5.0,
                },
                LiquidityLevel {
                    price: 0.42,
                    size: 10.0,
                },
            ],
            sell_liquidity: vec![LiquidityLevel {
                price: 0.39,
                size: 100.0,
            }],
            tick_size: 0.01,
            min_order_size: 0.001,
            neg_risk: false,
        };
        let plan = build_execution_plan(&intent, &book).expect("depth-aware buy plan");
        assert!((plan.price - 0.43).abs() < f64::EPSILON);
    }

    #[test]
    fn fast_taker_sell_keeps_share_amount_and_guards_one_tick() {
        let mut intent = sample_trade_intent();
        intent.trade_side = TradeSide::SellUp;
        intent.order_side = OrderSide::Sell;
        intent.nominal_usd = 5.0;
        let book = BookSnapshot {
            direct_best_bid: Some(0.50),
            direct_best_ask: Some(0.51),
            complement_best_bid: None,
            complement_best_ask: None,
            best_bid: Some(0.50),
            best_ask: Some(0.51),
            buy_liquidity: vec![LiquidityLevel {
                price: 0.51,
                size: 100.0,
            }],
            sell_liquidity: vec![
                LiquidityLevel {
                    price: 0.50,
                    size: 5.0,
                },
                LiquidityLevel {
                    price: 0.48,
                    size: 5.0,
                },
            ],
            tick_size: 0.01,
            min_order_size: 0.001,
            neg_risk: false,
        };
        let plan = build_execution_plan(&intent, &book).expect("depth-aware sell plan");
        assert!((plan.price - 0.47).abs() < f64::EPSILON);
        assert!((plan.shares - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn response_amounts_report_actual_fill_not_worst_price_cap() {
        assert_eq!(
            derive_execution_fill(OrderSide::Buy, 1.0, 2.0),
            Some((0.5, 2.0))
        );
        assert_eq!(
            derive_execution_fill(OrderSide::Sell, 2.0, 1.0),
            Some((0.5, 2.0))
        );
        assert_eq!(derive_execution_fill(OrderSide::Buy, 0.0, 0.0), None);
    }

    #[test]
    fn calculate_fast_maker_sell_moves_above_bid() {
        let plan = build_execution_plan(
            &TradeIntent {
                local_id: "local".to_string(),
                fingerprint: "fp".to_string(),
                token_id: "1".to_string(),
                complement_token_id: Some("2".to_string()),
                trade_side: TradeSide::SellUp,
                order_side: OrderSide::Sell,
                nominal_usd: 1.0,
                mechanism: TradingMechanism::FastMaker,
                market_slug: "x".to_string(),
                market_label: "sell".to_string(),
                session_end_ms: 60_000,
            },
            &BookSnapshot {
                direct_best_bid: Some(0.40),
                direct_best_ask: Some(0.43),
                complement_best_bid: Some(0.57),
                complement_best_ask: Some(0.60),
                best_bid: Some(0.40),
                best_ask: Some(0.43),
                buy_liquidity: vec![LiquidityLevel {
                    price: 0.43,
                    size: 100.0,
                }],
                sell_liquidity: vec![LiquidityLevel {
                    price: 0.40,
                    size: 100.0,
                }],
                tick_size: 0.01,
                min_order_size: 0.001,
                neg_risk: false,
            },
        )
        .unwrap();
        assert!((plan.price - 0.43).abs() < f64::EPSILON);
    }

    #[test]
    fn size_must_respect_min_order_size() {
        let error = build_execution_plan(
            &TradeIntent {
                local_id: "local".to_string(),
                fingerprint: "fp".to_string(),
                token_id: "1".to_string(),
                complement_token_id: Some("2".to_string()),
                trade_side: TradeSide::SellUp,
                order_side: OrderSide::Sell,
                nominal_usd: 0.5,
                mechanism: TradingMechanism::FastTaker,
                market_slug: "x".to_string(),
                market_label: "sell".to_string(),
                session_end_ms: 60_000,
            },
            &BookSnapshot {
                direct_best_bid: Some(0.95),
                direct_best_ask: Some(0.96),
                complement_best_bid: Some(0.04),
                complement_best_ask: Some(0.05),
                best_bid: Some(0.95),
                best_ask: Some(0.96),
                buy_liquidity: vec![LiquidityLevel {
                    price: 0.96,
                    size: 100.0,
                }],
                sell_liquidity: vec![LiquidityLevel {
                    price: 0.95,
                    size: 100.0,
                }],
                tick_size: 0.01,
                min_order_size: 1.0,
                neg_risk: false,
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("below min order size"));
    }

    #[test]
    fn complementary_binary_book_supplies_missing_best_ask() {
        let plan = build_execution_plan(
            &TradeIntent {
                local_id: "local".to_string(),
                fingerprint: "fp".to_string(),
                token_id: "1".to_string(),
                complement_token_id: Some("2".to_string()),
                trade_side: TradeSide::BuyUp,
                order_side: OrderSide::Buy,
                nominal_usd: 5.0,
                mechanism: TradingMechanism::FastTaker,
                market_slug: "x".to_string(),
                market_label: "buy".to_string(),
                session_end_ms: 60_000,
            },
            &BookSnapshot {
                direct_best_bid: Some(0.99),
                direct_best_ask: None,
                complement_best_bid: Some(0.16),
                complement_best_ask: Some(0.17),
                best_bid: Some(0.83),
                best_ask: Some(0.84),
                buy_liquidity: vec![LiquidityLevel {
                    price: 0.84,
                    size: 100.0,
                }],
                sell_liquidity: vec![LiquidityLevel {
                    price: 0.83,
                    size: 100.0,
                }],
                tick_size: 0.01,
                min_order_size: 5.0,
                neg_risk: true,
            },
        )
        .unwrap();
        assert!((plan.price - 0.85).abs() < f64::EPSILON);
        assert!(plan.shares >= 5.0);
    }

    #[test]
    fn trading_state_ready_requires_configuration() {
        let mut state = TradingState::default();
        state.toggle_enabled();
        state.set_side(TradeSide::BuyUp);
        assert!(!state.is_ready());
        state.set_configured(true);
        assert!(state.is_ready());
    }

    #[test]
    fn reset_for_new_session_restores_safe_defaults() {
        let mut state = TradingState::default();
        state.set_configured(true);
        state.toggle_enabled();
        state.set_nominal(5.0);
        state.set_mechanism(TradingMechanism::FastMaker);
        state.set_side(TradeSide::SellDown);
        state.last_error = Some("boom".to_string());
        state.active_order_id = Some("order-1".to_string());

        state.reset_for_new_session();

        assert!(!state.enabled);
        assert_eq!(state.selected_nominal, 1.0);
        assert_eq!(state.selected_mechanism, TradingMechanism::FastTaker);
        assert_eq!(state.selected_side, None);
        assert!(!state.ready_to_trade);
        assert_eq!(state.order_status, "Disabled");
        assert_eq!(state.last_error, None);
        assert_eq!(state.active_order_id, None);
    }

    #[test]
    fn unresolved_submission_lock_survives_disable_until_terminal() {
        let mut state = TradingState::default();
        let mut entry = sample_ledger_entry("uncertain", 1_000);
        entry.status = TradingOrderStatus::Unresolved;
        entry.detail = "Waiting for authoritative order status".to_string();
        let session_slug = entry.session_slug.clone();
        let local_id = entry.local_id.clone();
        state.replace_ledger(vec![entry.clone()]);
        state.reconcile_submission_lock(&session_slug, 2_000);
        assert_eq!(
            state
                .in_flight_intent
                .as_ref()
                .map(|intent| &intent.local_id),
            Some(&local_id)
        );

        state.enabled = true;
        state.toggle_enabled();
        assert!(state.in_flight_intent.is_some());

        entry.status = TradingOrderStatus::Failed;
        entry.updated_at_ms = 3_000;
        state.upsert_ledger_entry(entry);
        state.reconcile_submission_lock(&session_slug, 3_000);
        assert!(state.in_flight_intent.is_none());
    }

    #[test]
    fn normalize_outcome_aliases() {
        assert_eq!(normalize_outcome_label("YES"), "yes");
        assert_eq!(normalize_outcome_label("Up"), "yes");
        assert_eq!(normalize_outcome_label("no"), "no");
        assert_eq!(normalize_outcome_label("Down"), "no");
    }

    #[test]
    fn market_query_must_disambiguate_shared_yes_no_event_markets() {
        let event = EventBySlug {
            title: "2026 FIFA World Cup Winner".to_string(),
            markets: vec![
                EventMarket {
                    question: Some("Will Spain win the 2026 FIFA World Cup?".to_string()),
                    outcomes: Some("[\"Yes\",\"No\"]".to_string()),
                    clob_token_ids: Some("[\"1\",\"2\"]".to_string()),
                },
                EventMarket {
                    question: Some("Will England win the 2026 FIFA World Cup?".to_string()),
                    outcomes: Some("[\"Yes\",\"No\"]".to_string()),
                    clob_token_ids: Some("[\"3\",\"4\"]".to_string()),
                },
            ],
        };

        let normalized_outcome = normalize_outcome_label("yes");
        let mut matches = Vec::new();
        for market in event.markets {
            let question = market.question.unwrap();
            if !question.to_ascii_lowercase().contains("england") {
                continue;
            }
            let outcomes = parse_json_string_array(market.outcomes.as_deref()).unwrap();
            let token_ids = parse_json_string_array(market.clob_token_ids.as_deref()).unwrap();
            for (candidate, token_id) in outcomes.iter().zip(token_ids.iter()) {
                if normalize_outcome_label(candidate) == normalized_outcome {
                    matches.push((question.clone(), token_id.clone()));
                }
            }
        }

        assert_eq!(
            matches,
            vec![(
                "Will England win the 2026 FIFA World Cup?".to_string(),
                "3".to_string()
            )]
        );
    }

    #[test]
    fn duplicate_fingerprint_guard_is_session_scoped() {
        let mut state = TradingState::default();
        state.mark_intent_in_flight(
            "local-1".to_string(),
            "btc-updown-5m-123:Buy UP:2.00:Fast Taker".to_string(),
            "btc-updown-5m-123".to_string(),
            10_000,
        );

        assert!(state.duplicate_fingerprint_locked(
            "btc-updown-5m-123:Buy UP:2.00:Fast Taker",
            "btc-updown-5m-123"
        ));
        assert!(!state.duplicate_fingerprint_locked(
            "btc-updown-5m-123:Buy UP:2.00:Fast Taker",
            "btc-updown-5m-124"
        ));
        assert!(state.in_flight_intent.is_some());

        state.clear_in_flight_if("local-1");
        assert!(state.in_flight_intent.is_none());
    }

    #[test]
    fn parse_collateral_balance_uses_pusd_base_units() {
        assert!((parse_collateral_pusd("5114883").unwrap() - 5.114883).abs() < 1e-9);
        assert!((parse_collateral_pusd("1000000").unwrap() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn v2_allowance_requires_the_exact_market_exchange() {
        let regular = Address::from_str(POLYMARKET_CTF_EXCHANGE_V2_SPENDER).unwrap();
        let neg_risk = Address::from_str(POLYMARKET_NEG_RISK_CTF_EXCHANGE_V2_SPENDER).unwrap();
        let mut allowances = HashMap::new();
        allowances.insert(regular, "5000000".to_string());
        allowances.insert(neg_risk, "2000000".to_string());

        assert_eq!(
            parse_collateral_allowance_pusd(&allowances, false).unwrap(),
            5.0
        );
        assert_eq!(
            parse_collateral_allowance_pusd(&allowances, true).unwrap(),
            2.0
        );

        allowances.remove(&regular);
        assert_eq!(
            parse_collateral_allowance_pusd(&allowances, false).unwrap(),
            0.0
        );
    }

    #[test]
    fn buy_preflight_selects_allowance_for_market_risk_mode() {
        let intent = sample_trade_intent();
        let mut plan = ExecutionPlan {
            price: 0.5,
            shares: 2.0,
            order_type: ClobOrderType::FOK,
            amount: Amount::usdc(Decimal::from_str("1.00").unwrap()).unwrap(),
            neg_risk: false,
        };
        let collateral = BuyPreflightSnapshot {
            balance: 5.0,
            regular_allowance: 2.0,
            neg_risk_allowance: 0.5,
        };
        validate_buy_preflight(&collateral, &intent, &plan).expect("regular allowance valid");
        plan.neg_risk = true;
        let error = validate_buy_preflight(&collateral, &intent, &plan)
            .expect_err("neg-risk allowance must be selected");
        assert!(error.to_string().contains("Approval required"));
    }

    #[test]
    fn signature_type_ids_cover_all_clob_v2_wallet_modes() {
        assert_eq!(signature_type_from_id(0).unwrap(), SignatureType::Eoa);
        assert_eq!(signature_type_from_id(1).unwrap(), SignatureType::Proxy);
        assert_eq!(
            signature_type_from_id(2).unwrap(),
            SignatureType::GnosisSafe
        );
        assert_eq!(signature_type_from_id(3).unwrap(), SignatureType::Poly1271);
        assert!(signature_type_from_id(4).is_err());
    }

    #[test]
    fn runtime_config_requires_the_complete_v2_credential_set() {
        let mut empty = TradingArgs {
            signer_address: None,
            funder_address: None,
            private_key: None,
            signature_type: None,
        };
        assert!(runtime_config_from_args(&mut empty).unwrap().is_none());

        let mut partial = TradingArgs {
            signer_address: Some("0x1".to_string()),
            funder_address: Some("0x2".to_string()),
            private_key: Some("secret".to_string()),
            signature_type: None,
        };
        assert!(runtime_config_from_args(&mut partial).is_err());
        assert!(partial.private_key.is_none());

        let mut complete = TradingArgs {
            signer_address: Some("0x1".to_string()),
            funder_address: Some("0x2".to_string()),
            private_key: Some("secret".to_string()),
            signature_type: Some(3),
        };
        let config = runtime_config_from_args(&mut complete)
            .unwrap()
            .expect("complete credential set");
        assert_eq!(config.signature_type, 3);
        assert!(complete.private_key.is_none());
    }

    #[test]
    fn cancellation_acknowledgement_keeps_reconciliation_owner() {
        let mut entry = sample_ledger_entry("maker", 1_000);
        entry.status = TradingOrderStatus::Open;
        entry.mechanism = TradingMechanism::FastMaker;
        let tracked = TrackedOrder {
            entry,
            cancel_at_ms: Some(1_500),
            expires_at_ms: Some(10_000),
        };
        let pending = mark_cancel_acknowledged(tracked, 2_000);
        assert_eq!(pending.entry.status, TradingOrderStatus::Unresolved);
        assert!(!pending.entry.status.is_terminal());
        assert!(pending.entry.detail.contains("final matched size"));
        assert_eq!(pending.cancel_at_ms, None);
        assert_eq!(pending.expires_at_ms, Some(10_000));
    }

    #[test]
    fn lookup_error_preserves_uncertainty_instead_of_expired() {
        let mut entry = sample_ledger_entry("remote", 1_000);
        entry.status = TradingOrderStatus::Open;
        let unresolved = mark_remote_lookup_unresolved(entry, true, &"timeout", 20_000);
        assert_eq!(unresolved.status, TradingOrderStatus::Unresolved);
        assert!(!unresolved.status.is_terminal());
        assert!(unresolved.detail.contains("after the local deadline"));
    }

    #[test]
    fn restart_recovers_known_nonterminal_orders_only() {
        let mut open_maker = sample_ledger_entry("open-maker", 1_000);
        open_maker.status = TradingOrderStatus::Open;
        open_maker.mechanism = TradingMechanism::FastMaker;

        let mut unresolved = sample_ledger_entry("unresolved", 1_100);
        unresolved.status = TradingOrderStatus::Unresolved;

        let terminal = sample_ledger_entry("filled", 1_200);
        let recovered = recover_tracked_orders(&[open_maker, unresolved, terminal], 5_000);
        assert_eq!(recovered.len(), 2);
        assert_eq!(
            recovered
                .get("order-open-maker")
                .and_then(|tracked| tracked.cancel_at_ms),
            Some(5_000)
        );
        assert_eq!(
            recovered
                .get("order-unresolved")
                .and_then(|tracked| tracked.cancel_at_ms),
            None
        );
    }

    #[tokio::test]
    async fn trade_history_reloads_the_latest_record() {
        let directory = tempdir().expect("temporary directory");
        let mut store = TradingLedgerStore::new(directory.path().to_path_buf());
        let first = sample_ledger_entry("local-1", 1_000);
        let mut updated = first.clone();
        updated.status = TradingOrderStatus::Cancelled;
        updated.updated_at_ms = 2_000;
        store.append(&first).await.expect("append trade");
        store.append(&updated).await.expect("append trade update");

        let mut reopened = TradingLedgerStore::new(directory.path().to_path_buf());
        let entries = reopened.load_today().await.expect("reload trades");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, TradingOrderStatus::Cancelled);
    }
}

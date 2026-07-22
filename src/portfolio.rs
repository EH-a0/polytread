use std::collections::BTreeMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use alloy::primitives::{Address, B256, U256};
use alloy::providers::{Provider as _, ProviderBuilder};
use alloy::signers::Signer as _;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc};
use tracing::{info, warn};

use crate::config::DATA_API_URL;
use crate::state::{AppEvent, now_ms};
use crate::trading::TradingRuntimeConfig;

const PORTFOLIO_POLL_SECONDS: u64 = 20;
const PORTFOLIO_PERSIST_SECONDS: i64 = 60;
const PORTFOLIO_SNAPSHOT_SCHEMA_VERSION: u8 = 1;
const MIN_OPEN_POSITION_SIZE: f64 = 0.01;
const MAX_PORTFOLIO_ROWS: usize = 500;
const MAX_POSITION_PAGES: usize = 4;
const MAX_CLOSED_PAGES: usize = 10;
const MAX_CLAIMABLE_POSITIONS: usize = 100;
const MAX_CLAIM_HISTORY: usize = 100;
const CONDITIONAL_TOKENS_ADDRESS: &str = "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045";
const PUSD_ADDRESS: &str = "0xC011a7E12a19f7B1f670d46F03B03f3342E82DFB";
const CTF_COLLATERAL_ADAPTER_ADDRESS: &str = "0xAdA100Db00Ca00073811820692005400218FcE1f";
const NEG_RISK_CTF_COLLATERAL_ADAPTER_ADDRESS: &str = "0xadA2005600Dec949baf300f4C6120000bDB6eAab";

sol! {
    #[sol(rpc)]
    interface IConditionalTokens {
        function isApprovedForAll(address account, address operator) external view returns (bool);
        function setApprovalForAll(address operator, bool approved) external;
    }

    #[sol(rpc)]
    interface ICtfCollateralAdapter {
        function COLLATERAL_TOKEN() external view returns (address);
        function CONDITIONAL_TOKENS() external view returns (address);
        function redeemPositions(
            address collateralToken,
            bytes32 parentCollectionId,
            bytes32 conditionId,
            uint256[] calldata indexSets
        ) external;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClaimablePosition {
    pub condition_id: String,
    pub title: String,
    pub slug: String,
    pub outcomes: String,
    pub shares: f64,
    pub redeemable_value_usd: f64,
    pub cash_pnl_usd: f64,
    pub negative_risk: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClaimRecord {
    pub condition_id: String,
    pub title: String,
    pub transaction_hash: String,
    pub claimed_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PortfolioState {
    pub current_open_pnl_usd: f64,
    pub today_realized_pnl_usd: f64,
    pub current_value_usd: f64,
    pub open_positions: usize,
    pub claimable_positions: Vec<ClaimablePosition>,
    pub claim_history: Vec<ClaimRecord>,
    pub updated_at_ms: Option<i64>,
    pub last_error: Option<String>,
    pub claim_status: String,
    pub claim_in_flight_condition_id: Option<String>,
    pub direct_claim_supported: bool,
    pub manual_claim_only: bool,
    pub wallet_type: String,
}

impl PortfolioState {
    pub fn configured(signature_type: u8) -> Self {
        Self {
            current_open_pnl_usd: 0.0,
            today_realized_pnl_usd: 0.0,
            current_value_usd: 0.0,
            open_positions: 0,
            claimable_positions: Vec::new(),
            claim_history: Vec::new(),
            updated_at_ms: None,
            last_error: None,
            claim_status: "No claim submitted".to_string(),
            claim_in_flight_condition_id: None,
            direct_claim_supported: signature_type == 0,
            manual_claim_only: true,
            wallet_type: wallet_type_label(signature_type).to_string(),
        }
    }

    fn apply_refresh(&mut self, refresh: PortfolioRefresh) {
        self.current_open_pnl_usd = refresh.current_open_pnl_usd;
        self.today_realized_pnl_usd = refresh.today_realized_pnl_usd;
        self.current_value_usd = refresh.current_value_usd;
        self.open_positions = refresh.open_positions;
        self.claimable_positions = refresh.claimable_positions;
        self.updated_at_ms = Some(now_ms());
        self.last_error = None;
    }
}

impl Default for PortfolioState {
    fn default() -> Self {
        Self::configured(255)
    }
}

#[derive(Debug, Clone)]
pub struct ClaimIntent {
    pub condition_id: String,
    pub expected_redeemable_value_usd: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PositionResponse {
    condition_id: String,
    #[serde(default)]
    size: f64,
    #[serde(default)]
    current_value: f64,
    #[serde(default)]
    cash_pnl: f64,
    #[serde(default)]
    redeemable: bool,
    #[serde(default)]
    title: String,
    #[serde(default)]
    slug: String,
    #[serde(default)]
    outcome: String,
    #[serde(default)]
    negative_risk: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClosedPositionResponse {
    #[serde(default)]
    realized_pnl: f64,
    timestamp: i64,
}

#[derive(Debug)]
struct PortfolioRefresh {
    current_open_pnl_usd: f64,
    today_realized_pnl_usd: f64,
    current_value_usd: f64,
    open_positions: usize,
    claimable_positions: Vec<ClaimablePosition>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct PortfolioSnapshotRecord {
    #[serde(default)]
    schema_version: u8,
    current_open_pnl_usd: f64,
    today_realized_pnl_usd: f64,
    current_value_usd: f64,
    open_positions: usize,
    updated_at_ms: Option<i64>,
}

impl From<&PortfolioState> for PortfolioSnapshotRecord {
    fn from(state: &PortfolioState) -> Self {
        Self {
            schema_version: PORTFOLIO_SNAPSHOT_SCHEMA_VERSION,
            current_open_pnl_usd: state.current_open_pnl_usd,
            today_realized_pnl_usd: state.today_realized_pnl_usd,
            current_value_usd: state.current_value_usd,
            open_positions: state.open_positions,
            updated_at_ms: state.updated_at_ms,
        }
    }
}

struct PortfolioStore {
    path: PathBuf,
    claims_path: PathBuf,
    last_persisted_at_ms: Option<i64>,
}

impl PortfolioStore {
    async fn open(
        data_dir: PathBuf,
    ) -> Result<(Self, Option<PortfolioSnapshotRecord>, Vec<ClaimRecord>)> {
        fs::create_dir_all(&data_dir).await.with_context(|| {
            format!(
                "failed to create portfolio history directory {}",
                data_dir.display()
            )
        })?;
        let path = data_dir.join("portfolio.ndjson");
        let claims_path = data_dir.join("claims.ndjson");
        let last = load_last_valid::<PortfolioSnapshotRecord>(&path)
            .await?
            .filter(|snapshot| snapshot.schema_version == PORTFOLIO_SNAPSHOT_SCHEMA_VERSION);
        let claims = load_recent_valid::<ClaimRecord>(&claims_path, MAX_CLAIM_HISTORY).await?;
        let last_persisted_at_ms = last.as_ref().and_then(|state| state.updated_at_ms);
        Ok((
            Self {
                path,
                claims_path,
                last_persisted_at_ms,
            },
            last,
            claims,
        ))
    }

    async fn persist_if_due(&mut self, state: &PortfolioState, force: bool) -> Result<()> {
        let now = now_ms();
        if !force
            && self.last_persisted_at_ms.is_some_and(|previous| {
                now.saturating_sub(previous) < PORTFOLIO_PERSIST_SECONDS * 1_000
            })
        {
            return Ok(());
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await
            .with_context(|| format!("failed opening {}", self.path.display()))?;
        let mut row = serde_json::to_vec(&PortfolioSnapshotRecord::from(state))
            .context("failed serializing portfolio history")?;
        row.push(b'\n');
        file.write_all(&row).await?;
        file.flush().await?;
        self.last_persisted_at_ms = Some(now);
        Ok(())
    }

    async fn record_claim(&self, claim: &ClaimRecord) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.claims_path)
            .await
            .with_context(|| format!("failed opening {}", self.claims_path.display()))?;
        let mut row = serde_json::to_vec(claim).context("failed serializing claim history")?;
        row.push(b'\n');
        file.write_all(&row).await?;
        file.flush().await?;
        Ok(())
    }
}

pub async fn run_portfolio_task(
    config: TradingRuntimeConfig,
    event_tx: mpsc::Sender<AppEvent>,
    mut shutdown: broadcast::Receiver<()>,
    mut claim_rx: mpsc::Receiver<ClaimIntent>,
    data_dir: PathBuf,
    polygon_rpc_url: String,
) {
    let client = match Client::builder().timeout(Duration::from_secs(12)).build() {
        Ok(client) => client,
        Err(error) => {
            warn!(%error, "failed to create portfolio HTTP client");
            return;
        }
    };
    let (mut store, persisted, claims) = match PortfolioStore::open(data_dir).await {
        Ok(value) => value,
        Err(error) => {
            warn!(%error, "portfolio history unavailable");
            return;
        }
    };
    let mut state = PortfolioState::configured(config.signature_type);
    if let Some(persisted) = persisted {
        state.current_open_pnl_usd = persisted.current_open_pnl_usd;
        state.today_realized_pnl_usd = persisted.today_realized_pnl_usd;
        state.current_value_usd = persisted.current_value_usd;
        state.open_positions = persisted.open_positions;
        state.updated_at_ms = persisted.updated_at_ms;
    }
    state.claim_history = claims.into_iter().rev().collect();
    state.direct_claim_supported = config.signature_type == 0;
    state.manual_claim_only = true;
    state.wallet_type = wallet_type_label(config.signature_type).to_string();
    state.claim_in_flight_condition_id = None;
    let _ = event_tx.send(AppEvent::Portfolio(state.clone())).await;

    let mut refresh_tick = tokio::time::interval(Duration::from_secs(PORTFOLIO_POLL_SECONDS));
    refresh_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown.recv() => return,
            _ = refresh_tick.tick() => {
                refresh_and_publish(&client, &config.funder_address, &mut state, &event_tx, &mut store).await;
            }
            Some(intent) = claim_rx.recv() => {
                handle_claim(
                    &client,
                    &config,
                    &polygon_rpc_url,
                    intent,
                    &mut state,
                    &event_tx,
                    &mut store,
                ).await;
            }
        }
    }
}

async fn refresh_and_publish(
    client: &Client,
    funder_address: &str,
    state: &mut PortfolioState,
    event_tx: &mpsc::Sender<AppEvent>,
    store: &mut PortfolioStore,
) {
    match fetch_portfolio(client, funder_address).await {
        Ok(refresh) => state.apply_refresh(refresh),
        Err(error) => {
            state.last_error = Some(format!("Portfolio refresh failed: {error}"));
            warn!(%error, "portfolio refresh failed; preserving last snapshot");
        }
    }
    let _ = event_tx.send(AppEvent::Portfolio(state.clone())).await;
    if let Err(error) = store.persist_if_due(state, false).await {
        warn!(%error, "failed persisting portfolio snapshot");
    }
}

async fn handle_claim(
    client: &Client,
    config: &TradingRuntimeConfig,
    polygon_rpc_url: &str,
    intent: ClaimIntent,
    state: &mut PortfolioState,
    event_tx: &mpsc::Sender<AppEvent>,
    store: &mut PortfolioStore,
) {
    let position = state
        .claimable_positions
        .iter()
        .find(|position| {
            position
                .condition_id
                .eq_ignore_ascii_case(&intent.condition_id)
        })
        .cloned();
    let mut confirmed_record = None;
    let result = async {
        let position = position.ok_or_else(|| anyhow!("the position is no longer claimable"))?;
        if (position.redeemable_value_usd - intent.expected_redeemable_value_usd).abs() > 0.01 {
            bail!("the claim value changed; refresh and confirm it again");
        }
        if config.signature_type != 0 {
            bail!(
                "direct local claims are unavailable for {}; use the manual Polymarket Portfolio link",
                wallet_type_label(config.signature_type)
            );
        }
        state.claim_in_flight_condition_id = Some(position.condition_id.clone());
        state.claim_status = format!("Submitting manual claim for {}", position.title);
        state.last_error = None;
        let _ = event_tx.send(AppEvent::Portfolio(state.clone())).await;

        let transaction_hash = execute_eoa_claim(config, polygon_rpc_url, &position).await?;
        let record = ClaimRecord {
            condition_id: position.condition_id.clone(),
            title: position.title.clone(),
            transaction_hash: transaction_hash.clone(),
            claimed_at_ms: now_ms(),
        };
        state.claim_history.insert(0, record.clone());
        state.claim_history.truncate(MAX_CLAIM_HISTORY);
        confirmed_record = Some(record);
        state.claim_status = format!("Manual claim confirmed: {transaction_hash}");
        info!(condition_id = %position.condition_id, %transaction_hash, "manual claim confirmed");
        Result::<()>::Ok(())
    }
    .await;

    state.claim_in_flight_condition_id = None;
    if let Err(error) = result {
        state.claim_status = format!("Claim rejected: {error}");
        state.last_error = Some(error.to_string());
    }
    if let Some(record) = confirmed_record.as_ref()
        && let Err(error) = store.record_claim(record).await
    {
        warn!(%error, "failed persisting confirmed claim history");
    }
    let _ = event_tx.send(AppEvent::Portfolio(state.clone())).await;
    if let Err(error) = store.persist_if_due(state, true).await {
        warn!(%error, "failed persisting claim result");
    }
    refresh_and_publish(client, &config.funder_address, state, event_tx, store).await;
}

async fn fetch_portfolio(client: &Client, funder_address: &str) -> Result<PortfolioRefresh> {
    let positions = fetch_positions(client, funder_address).await?;
    let today_start_seconds = Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight is valid")
        .and_utc()
        .timestamp();
    let closed = fetch_today_closed_positions(client, funder_address, today_start_seconds).await?;

    Ok(portfolio_refresh_from_rows(&positions, &closed))
}

fn portfolio_refresh_from_rows(
    positions: &[PositionResponse],
    closed: &[ClosedPositionResponse],
) -> PortfolioRefresh {
    let active_positions = positions
        .iter()
        .filter(|position| {
            !position.redeemable
                && position.size.is_finite()
                && position.size > MIN_OPEN_POSITION_SIZE
        })
        .collect::<Vec<_>>();
    let current_open_pnl_usd =
        finite_sum(active_positions.iter().map(|position| position.cash_pnl));
    let current_value_usd = finite_sum(
        active_positions
            .iter()
            .map(|position| position.current_value),
    );
    let today_realized_pnl_usd = finite_sum(closed.iter().map(|position| position.realized_pnl));
    let claimable_positions = aggregate_claimable_positions(positions);
    PortfolioRefresh {
        current_open_pnl_usd,
        today_realized_pnl_usd,
        current_value_usd,
        open_positions: active_positions.len(),
        claimable_positions,
    }
}

async fn fetch_positions(client: &Client, funder_address: &str) -> Result<Vec<PositionResponse>> {
    let mut all = Vec::new();
    for page in 0..MAX_POSITION_PAGES {
        let offset = page * MAX_PORTFOLIO_ROWS;
        let response = client
            .get(format!("{DATA_API_URL}/positions"))
            .query(&[
                ("user", funder_address.to_string()),
                ("sizeThreshold", "0.01".to_string()),
                ("limit", MAX_PORTFOLIO_ROWS.to_string()),
                ("offset", offset.to_string()),
            ])
            .send()
            .await
            .context("current positions request failed")?
            .error_for_status()
            .context("current positions request was rejected")?
            .json::<Vec<PositionResponse>>()
            .await
            .context("current positions response was invalid")?;
        let count = response.len();
        all.extend(response);
        if count < MAX_PORTFOLIO_ROWS {
            break;
        }
    }
    Ok(all)
}

async fn fetch_today_closed_positions(
    client: &Client,
    funder_address: &str,
    today_start_seconds: i64,
) -> Result<Vec<ClosedPositionResponse>> {
    let mut all = Vec::new();
    for page in 0..MAX_CLOSED_PAGES {
        let offset = page * 50;
        let response = client
            .get(format!("{DATA_API_URL}/closed-positions"))
            .query(&[
                ("user", funder_address.to_string()),
                ("limit", "50".to_string()),
                ("offset", offset.to_string()),
                ("sortBy", "TIMESTAMP".to_string()),
                ("sortDirection", "DESC".to_string()),
            ])
            .send()
            .await
            .context("closed positions request failed")?
            .error_for_status()
            .context("closed positions request was rejected")?
            .json::<Vec<ClosedPositionResponse>>()
            .await
            .context("closed positions response was invalid")?;
        let count = response.len();
        let mut reached_older_row = false;
        for row in response {
            let timestamp_seconds = normalize_timestamp_seconds(row.timestamp);
            if timestamp_seconds < today_start_seconds {
                reached_older_row = true;
                continue;
            }
            all.push(row);
        }
        if count < 50 || reached_older_row {
            break;
        }
    }
    Ok(all)
}

fn aggregate_claimable_positions(positions: &[PositionResponse]) -> Vec<ClaimablePosition> {
    let mut grouped = BTreeMap::<String, ClaimablePosition>::new();
    for position in positions.iter().filter(|position| {
        position.redeemable
            && position.size.is_finite()
            && position.size > 0.0
            && position.current_value.is_finite()
            && position.current_value > 0.0
    }) {
        let entry = grouped
            .entry(position.condition_id.to_ascii_lowercase())
            .or_insert_with(|| ClaimablePosition {
                condition_id: position.condition_id.clone(),
                title: position.title.clone(),
                slug: position.slug.clone(),
                outcomes: String::new(),
                shares: 0.0,
                redeemable_value_usd: 0.0,
                cash_pnl_usd: 0.0,
                negative_risk: position.negative_risk,
            });
        if !position.outcome.trim().is_empty() {
            if !entry.outcomes.is_empty() {
                entry.outcomes.push_str(" + ");
            }
            entry.outcomes.push_str(position.outcome.trim());
        }
        entry.shares += position.size.max(0.0);
        entry.redeemable_value_usd += position.current_value.max(0.0);
        if position.cash_pnl.is_finite() {
            entry.cash_pnl_usd += position.cash_pnl;
        }
        entry.negative_risk |= position.negative_risk;
    }
    let mut values = grouped.into_values().collect::<Vec<_>>();
    values.sort_by(|left, right| {
        right
            .redeemable_value_usd
            .total_cmp(&left.redeemable_value_usd)
            .then_with(|| left.condition_id.cmp(&right.condition_id))
    });
    values.truncate(MAX_CLAIMABLE_POSITIONS);
    values
}

fn finite_sum(values: impl Iterator<Item = f64>) -> f64 {
    let total = values.filter(|value| value.is_finite()).sum::<f64>();
    if total.abs() < 1e-9 { 0.0 } else { total }
}

fn normalize_timestamp_seconds(value: i64) -> i64 {
    if value.abs() > 10_000_000_000 {
        value.div_euclid(1_000)
    } else {
        value
    }
}

async fn execute_eoa_claim(
    config: &TradingRuntimeConfig,
    polygon_rpc_url: &str,
    position: &ClaimablePosition,
) -> Result<String> {
    let signer = PrivateKeySigner::from_str(config.private_key.trim())
        .context("invalid EOA private key")?
        .with_chain_id(Some(polymarket_client_sdk_v2::POLYGON));
    let configured_signer =
        Address::from_str(&config.signer_address).context("invalid configured signer address")?;
    let funder =
        Address::from_str(&config.funder_address).context("invalid configured funder address")?;
    if signer.address() != configured_signer || signer.address() != funder {
        bail!("EOA claim requires the signer and funding wallet to be the same address");
    }

    let provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .connect(polygon_rpc_url)
        .await
        .context("failed connecting to the Polygon RPC")?;
    let chain_id = provider
        .get_chain_id()
        .await
        .context("failed checking the Polygon RPC chain ID")?;
    if chain_id != polymarket_client_sdk_v2::POLYGON {
        bail!("the configured claim RPC returned chain ID {chain_id}, expected Polygon 137");
    }
    let gas_balance = provider
        .get_balance(signer.address())
        .await
        .context("failed checking the Polygon POL gas balance")?;
    if gas_balance == U256::ZERO {
        bail!("the EOA has no POL for Polygon transaction gas");
    }

    let conditional_tokens = Address::from_str(CONDITIONAL_TOKENS_ADDRESS)?;
    let adapter = Address::from_str(if position.negative_risk {
        NEG_RISK_CTF_COLLATERAL_ADAPTER_ADDRESS
    } else {
        CTF_COLLATERAL_ADAPTER_ADDRESS
    })?;
    let pusd = Address::from_str(PUSD_ADDRESS)?;
    let contract = ICtfCollateralAdapter::new(adapter, provider.clone());
    let adapter_collateral = contract
        .COLLATERAL_TOKEN()
        .call()
        .await
        .context("claim adapter collateral check failed")?;
    let adapter_ctf = contract
        .CONDITIONAL_TOKENS()
        .call()
        .await
        .context("claim adapter CTF check failed")?;
    if adapter_collateral != pusd || adapter_ctf != conditional_tokens {
        bail!("claim adapter contract configuration does not match current pUSD/CTF addresses");
    }
    let ctf = IConditionalTokens::new(conditional_tokens, provider.clone());
    let approved = ctf
        .isApprovedForAll(signer.address(), adapter)
        .call()
        .await
        .context("failed checking the claim adapter approval")?;
    if !approved {
        let pending = ctf
            .setApprovalForAll(adapter, true)
            .send()
            .await
            .context("failed submitting the one-time claim adapter approval")?;
        let receipt = pending
            .get_receipt()
            .await
            .context("claim adapter approval was not confirmed")?;
        if !receipt.status() {
            bail!("the claim adapter approval transaction reverted");
        }
    }

    let condition_id = B256::from_str(&position.condition_id)
        .context("claim position has an invalid condition ID")?;
    let pending = contract
        .redeemPositions(
            pusd,
            B256::ZERO,
            condition_id,
            vec![U256::from(1), U256::from(2)],
        )
        .send()
        .await
        .context("failed submitting the manual claim transaction")?;
    let transaction_hash = *pending.tx_hash();
    let receipt = pending
        .get_receipt()
        .await
        .context("manual claim transaction was not confirmed")?;
    if !receipt.status() {
        bail!("the manual claim transaction reverted");
    }
    Ok(format!("{transaction_hash:#x}"))
}

async fn load_last_valid<T>(path: &PathBuf) -> Result<Option<T>>
where
    T: for<'de> Deserialize<'de>,
{
    let file = match fs::File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed opening {}", path.display()));
        }
    };
    let mut lines = BufReader::new(file).lines();
    let mut last = None;
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<T>(&line) {
            Ok(value) => last = Some(value),
            Err(error) => warn!(path = %path.display(), %error, "skipping malformed portfolio row"),
        }
    }
    Ok(last)
}

async fn load_recent_valid<T>(path: &PathBuf, cap: usize) -> Result<Vec<T>>
where
    T: for<'de> Deserialize<'de>,
{
    let file = match fs::File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed opening {}", path.display()));
        }
    };
    let mut lines = BufReader::new(file).lines();
    let mut recent = std::collections::VecDeque::with_capacity(cap);
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<T>(&line) {
            Ok(value) => {
                if recent.len() == cap {
                    recent.pop_front();
                }
                recent.push_back(value);
            }
            Err(error) => warn!(path = %path.display(), %error, "skipping malformed history row"),
        }
    }
    Ok(recent.into())
}

fn wallet_type_label(value: u8) -> &'static str {
    match value {
        0 => "EOA",
        1 => "legacy proxy",
        2 => "Gnosis Safe",
        3 => "deposit wallet",
        _ => "unconfigured",
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn position(
        condition_id: &str,
        outcome: &str,
        size: f64,
        current_value: f64,
        pnl: f64,
        redeemable: bool,
    ) -> PositionResponse {
        PositionResponse {
            condition_id: condition_id.to_string(),
            size,
            current_value,
            cash_pnl: pnl,
            redeemable,
            title: "Resolved market".to_string(),
            slug: "resolved-market".to_string(),
            outcome: outcome.to_string(),
            negative_risk: false,
        }
    }

    #[test]
    fn claimable_rows_group_by_condition_without_losing_value() {
        let rows = aggregate_claimable_positions(&[
            position("0xabc", "Yes", 2.0, 2.0, 1.0, true),
            position("0xABC", "No", 3.0, 3.0, -1.0, true),
        ]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].redeemable_value_usd, 5.0);
        assert_eq!(rows[0].cash_pnl_usd, 0.0);
        assert_eq!(rows[0].outcomes, "Yes + No");
    }

    #[test]
    fn resolved_zero_value_rows_are_not_active_or_claimable() {
        let refresh =
            portfolio_refresh_from_rows(&[position("0xabc", "No", 8.0, 0.0, -2.0, true)], &[]);
        assert_eq!(refresh.open_positions, 0);
        assert_eq!(refresh.current_value_usd, 0.0);
        assert_eq!(refresh.current_open_pnl_usd, 0.0);
        assert!(refresh.claimable_positions.is_empty());
    }

    #[test]
    fn only_active_rows_drive_open_value_and_pnl() {
        let refresh = portfolio_refresh_from_rows(
            &[
                position("0xactive", "Yes", 3.0, 1.5, 0.5, false),
                position("0xresolved", "Yes", 2.0, 2.0, 1.0, true),
            ],
            &[],
        );
        assert_eq!(refresh.open_positions, 1);
        assert_eq!(refresh.current_value_usd, 1.5);
        assert_eq!(refresh.current_open_pnl_usd, 0.5);
        assert_eq!(refresh.claimable_positions.len(), 1);
        assert_eq!(refresh.claimable_positions[0].condition_id, "0xresolved");
    }

    #[test]
    fn displayed_totals_normalize_negative_zero() {
        assert_eq!(finite_sum([-0.0].into_iter()), 0.0);
        assert!(!finite_sum([-0.0].into_iter()).is_sign_negative());
    }

    #[test]
    fn millisecond_and_second_timestamps_normalize() {
        assert_eq!(normalize_timestamp_seconds(1_700_000_000), 1_700_000_000);
        assert_eq!(
            normalize_timestamp_seconds(1_700_000_000_123),
            1_700_000_000
        );
    }

    #[tokio::test]
    async fn portfolio_store_reloads_the_last_valid_snapshot() {
        let directory = tempdir().expect("tempdir");
        let (mut store, initial, claims) = PortfolioStore::open(directory.path().to_path_buf())
            .await
            .expect("open");
        assert!(initial.is_none());
        assert!(claims.is_empty());
        let mut state = PortfolioState::configured(0);
        state.current_open_pnl_usd = 12.5;
        state.updated_at_ms = Some(2_000);
        store.persist_if_due(&state, true).await.expect("persist");
        let (_, loaded, _) = PortfolioStore::open(directory.path().to_path_buf())
            .await
            .expect("reopen");
        assert_eq!(loaded.expect("snapshot").current_open_pnl_usd, 12.5);
    }

    #[tokio::test]
    async fn portfolio_store_ignores_an_earlier_calculation_schema() {
        let directory = tempdir().expect("tempdir");
        tokio::fs::write(
            directory.path().join("portfolio.ndjson"),
            b"{\"current_open_pnl_usd\":-62.0,\"today_realized_pnl_usd\":0.0,\"current_value_usd\":0.0,\"open_positions\":8,\"updated_at_ms\":2000}\n",
        )
        .await
        .expect("write legacy snapshot");
        let (_, loaded, _) = PortfolioStore::open(directory.path().to_path_buf())
            .await
            .expect("open");
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn claim_history_is_stored_once_in_a_separate_ledger() {
        let directory = tempdir().expect("tempdir");
        let (store, _, _) = PortfolioStore::open(directory.path().to_path_buf())
            .await
            .expect("open");
        let claim = ClaimRecord {
            condition_id: "0xabc".to_string(),
            title: "Claimed market".to_string(),
            transaction_hash: format!("0x{}", "1".repeat(64)),
            claimed_at_ms: 2_000,
        };
        store.record_claim(&claim).await.expect("record claim");
        let (_, _, claims) = PortfolioStore::open(directory.path().to_path_buf())
            .await
            .expect("reopen");
        assert_eq!(claims, vec![claim]);
    }

    #[test]
    fn portfolio_is_explicitly_manual_claim_only() {
        let state = PortfolioState::configured(0);
        assert!(state.manual_claim_only);
        assert!(state.direct_claim_supported);
        assert!(!PortfolioState::configured(2).direct_claim_supported);
    }
}

# Architecture

PolyTread is one Tokio process with a small embedded browser page. The browser and backend share
one origin; no relay or hosted control plane is required.

## Runtime flow

1. `discovery.rs` selects the current and next BTC five-minute sessions.
2. `feeds/` streams Binance BTC, Chainlink RTDS, and the two active outcome tokens.
3. `state.rs` reduces events into a bounded dashboard snapshot.
4. `consumer.rs` owns first-run setup, OS-vault access, default launch, status, and authenticated
   local shutdown.
5. `ws_dashboard.rs` serves the page, SSE snapshots, health check, and narrow typed command/control
   channels.
6. `trading.rs` authenticates, validates, builds, signs, submits, and reconciles explicit orders.
7. `portfolio.rs` refreshes source-backed PnL and claimable positions, persists bounded snapshots,
   and executes only explicitly queued EOA claims.
8. `history.rs` appends session and one-second price records.

The browser receives a compact snapshot containing current prices, feed status, sessions, trading
state, and recent local history. It never receives a private key.

## Source map

| Path | Responsibility |
| --- | --- |
| `src/main.rs` | CLI entry point |
| `src/app.rs` | Runtime orchestration and browser-command validation |
| `src/config.rs` | CLI and environment configuration |
| `src/consumer.rs` | Secure setup, local config/vault, launch, status, and shutdown |
| `src/discovery.rs` | Public session discovery |
| `src/feeds/` | Public price and outcome-token streams |
| `src/state.rs` | Minimal application state and snapshots |
| `src/trading.rs` | Authenticated execution, reconciliation, and trade history |
| `src/portfolio.rs` | Data-API PnL, claim discovery/history, and manual EOA claims |
| `src/history.rs` | Session and one-second price persistence |
| `src/local_control.rs` | Same-user local shutdown channel for the background consumer |
| `src/ws_dashboard.rs` | Loopback HTTP, SSE, and WebSocket server |
| `web/dashboard.html` | Embedded browser interface |

## Trading boundary

The browser sends only three command families: arm/disarm, submit one atomic order intent, and
request one manual claim. A rotating localhost bootstrap link establishes an HttpOnly browser
session before either command transport can reach the queue. The backend also validates the request
ID, selected nominal, active session, armed state, configured credentials, balance or allowance,
current position for sells, and a bounded execution price. Browser command origins must match the
service host.
Market depth is fetched on demand to calculate that price and is discarded after planning.

Taker and maker orders share the same authenticated client. Ambiguous POST outcomes are retained as
unresolved rather than replayed. A maker order uses a short client-side cancel target plus a
protocol-valid GTD expiration that remains inside the confirmed session. User WebSocket events
update trade status, with remote polling as reconciliation fallback; cancellation and lookup errors
retain reconciliation ownership until remote evidence establishes a terminal result.

## Claim boundary

Portfolio refresh is read-only. No timer can enqueue a claim. The only claim path begins with the
dashboard button and confirmation, revalidates the condition ID and expected value against current
claimable state, and enters a single bounded queue.

EOA claims use the current V2 pUSD collateral adapters on Polygon and wait for transaction
confirmation. Safe, proxy, and deposit wallets require Polymarket's builder-authenticated relayer;
the open-source consumer build presents those positions but hands the operator to the official
Portfolio page. It does not restore the retired private relayer integration.

## Persistence

The configured data directory has five append-only NDJSON files:

- `sessions.ndjson`
- `trades.ndjson`
- `prices-1s.ndjson`
- `portfolio.ndjson`
- `claims.ndjson`

Startup retains bounded recent price/session rows and reduces repeated trade updates by local ID.
Portfolio rows are compact and written at most once per minute; claim results have a separate
append-only ledger so they are not duplicated inside every PnL row. Consumer secrets live in the
operating-system credential vault, while compact local JSONL ledgers retain the consumer-visible
history and portfolio state.

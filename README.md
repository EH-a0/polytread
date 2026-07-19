# PolyTread

PolyTread is a lightweight, self-hosted Rust service for manually trading Polymarket BTC
five-minute markets from a local browser. It discovers live sessions, streams public prices,
submits explicitly confirmed orders, presents current and daily PnL, keeps local trade history,
and shows positions that are ready to claim.

The distribution is one native binary with an embedded browser dashboard and bounded local
storage, designed to run directly on a consumer machine.

## One-command start

After the first release and NPM package are published, install and start PolyTread with:

```sh
npm install --global polytread && polytread
```

The first `polytread` launch opens a terminal setup wizard. Private-key input is hidden. The wizard
derives the signer, discovers the funding wallet, detects its wallet type, authenticates with the
Polymarket CLOB, checks pUSD balance and allowances, and shows progress for every network step.

The private key and local shutdown token are stored in the operating-system credential vault. They
are never placed in NPM, JavaScript, a command-line argument, the browser, the config file, or the
history directory. The non-secret config and history remain local to the current OS user.

When setup completes, the terminal prints a rotating loopback dashboard access link. Open that
exact link: its URL fragment is exchanged for an HttpOnly same-site session cookie and immediately
removed from the address bar. The link changes whenever PolyTread restarts and should not be
shared. From a second terminal:

```sh
polytread status
polytread shutdown
```

`shutdown` uses a vault-backed bearer token and asks the Rust service to stop gracefully.

## Trading and claims

PolyTread can place real orders. It is not a strategy, signal, or promise of profitability.

- The consumer service is fixed to a localhost listener.
- Browser trading stays disabled unless the user types `ENABLE` during setup.
- The dashboard starts disarmed and confirms every individual order.
- Backend gates verify the active session, request ID, wallet balance/allowance, position size, and
  bounded execution price.
- Claimable positions are refreshed and displayed, but a claim is never submitted automatically.
- EOA positions have an explicit manual Claim button and require Polygon POL for gas. A one-time
  adapter approval may be submitted before the confirmed claim.
- Safe, legacy proxy, and deposit-wallet claims require Polymarket's authenticated relayer. The
  dashboard therefore provides a manual link to the official Portfolio page instead of collecting
  or shipping builder credentials.

Use a dedicated wallet and comply with the rules that apply to your account and location.

## Build from source

Install Rust 1.97, then run the safe public-data service without credentials:

```powershell
cargo run --locked -- serve
```

To exercise the consumer wizard from source:

```powershell
cargo run --locked --
```

Advanced operators may still supply `PM_SIGNER_ADDRESS`, `PM_FUNDER_ADDRESS`, `PM_PRIVATE_KEY`,
and `PM_SIGNATURE_TYPE` through the environment for `serve`. Private keys are intentionally not
accepted as CLI arguments.

## Local persistence

Consumer mode uses the operating system's per-user app-data directory. Advanced `serve` mode uses
`./data` unless changed. The lightweight NDJSON files are:

- `sessions.ndjson`: observed market sessions;
- `trades.ndjson`: append-only trade status changes, reduced to the latest state on load;
- `prices-1s.ndjson`: one price sample per second;
- `portfolio.ndjson`: one compact source-backed PnL summary per minute;
- `claims.ndjson`: one row per successfully confirmed manual claim.

No credential, raw feed frame, complete order book, or full market-depth snapshot is persisted.

## Verify

Run the complete local publishing gate:

```powershell
./scripts/verify.ps1 -FixFormat
```

On Linux or macOS:

```bash
bash scripts/verify.sh
```

## Documentation

- [Getting Started](documents/GETTING_STARTED.md)
- [Architecture](documents/ARCHITECTURE.md)
- [Configuration](documents/CONFIGURATION.md)
- [Operations](documents/OPERATIONS.md)

## License

Licensed under either Apache-2.0 or MIT, at your option. See `LICENSE-APACHE` and `LICENSE-MIT`.

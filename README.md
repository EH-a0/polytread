# PolyTread

[![Latest release](https://img.shields.io/github/v/release/EH-a0/polytread?style=flat-square)](https://github.com/EH-a0/polytread/releases/latest) [![npm](https://img.shields.io/npm/v/polytread?style=flat-square)](https://www.npmjs.com/package/polytread) [![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue?style=flat-square)](#license)

**A local browser dashboard for manually trading Polymarket BTC five-minute markets.**

PolyTread discovers the active markets, streams public prices, submits only explicitly confirmed
orders, shows current and daily PnL, keeps local trade history, and identifies positions that are
ready to claim.

It runs as one native Rust service on your own computer. The dashboard stays on `localhost`; no
VPS, Docker container, hosted relay, or public web server is required. Internet access is still
required for market data, authentication, trading, and claims.

> [!WARNING]
> PolyTread can submit real-money orders and explicit EOA claim transactions. It is not an
> automated strategy, trading signal, or promise of profitability. Use a dedicated wallet, review
> every confirmation, and follow the rules that apply to your Polymarket account and location.

## Install and start

### Requirements

- [Node.js](https://nodejs.org/en/download) 18 or newer, including npm;
- a current browser;
- Windows x64, Linux x64/arm64, or macOS Intel/Apple Silicon;
- for first-time setup, access to the private key of a dedicated Polymarket signing wallet.

You do **not** need to install Rust, Python, Docker, or a database for the recommended setup.

Check Node.js before continuing:

```sh
node --version
npm --version
```

`node --version` must report `v18` or newer.

### 1. Install PolyTread

```sh
npm install --global polytread
```

The npm installer selects the native binary for your operating system, downloads it from the
matching GitHub release, and verifies its SHA-256 checksum. It does not request or receive trading
credentials.

### 2. Start PolyTread

```sh
polytread
```

The first launch opens an interactive terminal wizard. It:

1. opens a full-screen setup menu; select **Setup configs** and press <kbd>Enter</kbd>;
2. reads the private key through a dedicated masked-input screen;
3. shows an animated checklist while it derives the signer and checks real Polymarket connectivity;
4. discovers the funding wallet, detects its type, and authenticates with the CLOB;
5. reports the pUSD collateral balance and both trading allowances;
6. asks for an explicit <kbd>Y</kbd> or <kbd>N</kbd> browser-trading choice;
7. stores secrets in the operating-system credential vault and starts the local dashboard.

The **signer** is the address derived from the private key. The **funding wallet** is the account or
contract that holds the Polymarket funds; it may be the signer itself, a proxy, or a Safe. PolyTread
normally discovers both the funding wallet and wallet type automatically and asks rather than
guessing if detection is inconclusive.

If DNS filtering blocks the required endpoints, setup may offer an operating-system DNS change.
The approval screen explains the change in plain language; press <kbd>I</kbd> there to open the
full diagnostic and rollback details. Nothing changes unless you type `YES`. PolyTread saves a
local rollback record, and the original settings can be restored with `polytread restore-dns`.
This changes DNS resolution only; it does not change your public IP or determine whether trading
is permitted in your location.

### 3. Open the exact dashboard link

After the listener starts, the runtime screen shows a link similar to:

```text
PolyTread dashboard: http://127.0.0.1:9878/#access=...
```

Press <kbd>C</kbd> to copy the complete private URL without selecting its wrapped text, then open
it in your browser. Its temporary URL fragment establishes
an HttpOnly local browser session and is then removed from the address bar. The access link rotates
whenever PolyTread restarts, so an old or partial link will not work and should not be shared.

Press <kbd>Esc</kbd>, <kbd>Q</kbd>, or <kbd>Ctrl</kbd>+<kbd>C</kbd> on the returning-user runtime
screen to close only that view. PolyTread verifies a no-console background worker, returns the
terminal prompt, and prints the complete URL again. The service continues until you run
`polytread shutdown`.

Setup is successful when the dashboard opens, shows connection status, and begins discovering the
current BTC five-minute session. The dashboard starts disarmed even if browser trading was enabled
during setup.

## Everyday commands

Run these from a terminal after installation:

```sh
polytread                  # start the service; run setup first when needed
polytread status           # check whether the configured local service is running
polytread shutdown         # request an authenticated graceful shutdown
polytread diagnose         # test Polymarket REST, WebSocket, and DNS connectivity
polytread setup --force    # validate and replace the saved wallet setup
polytread restore-dns      # restore DNS saved by an approved setup remediation
```

In advanced `polytread serve` mode, <kbd>Ctrl</kbd>+<kbd>C</kbd> still stops that foreground process.
On the normal consumer runtime screen it closes the view and leaves the service running.

## What PolyTread provides

- current and next Polymarket BTC five-minute sessions;
- live public BTC reference and UP/DOWN outcome prices;
- explicit manual buy and sell controls with order-status reconciliation;
- current open PnL and today's realized PnL in UTC;
- local session, price, trade, portfolio, and claim history;
- claimable-position visibility and explicit manual claim handling.

PolyTread does not run a strategy, schedule trades, automatically arm the dashboard, or
automatically claim positions.

## Trading and claim safeguards

- Consumer mode is fixed to a loopback listener and does not expose the dashboard publicly.
- Browser orders remain disabled unless you explicitly press <kbd>Y</kbd> on the final setup screen.
- The dashboard starts disarmed and asks for confirmation before every order.
- The backend rechecks the active session, request ID, wallet balance and allowance, position size,
  and bounded execution price.
- Claimable positions are refreshed automatically, but a claim is never submitted automatically.
- EOA claims require an explicit confirmation and Polygon POL for gas. A one-time collateral
  adapter approval may also require confirmation.
- Safe, legacy proxy, and deposit-wallet claims require Polymarket's authenticated relayer, so
  PolyTread sends those users to the official Portfolio page instead of collecting builder
  credentials.

## Credentials and local data

Private-key input is hidden. The private key and local shutdown token are stored in the
operating-system credential vault; there is no plaintext-secret fallback. They are never placed in
npm, JavaScript, a command-line argument, the browser, the config file, or the history directory.

Non-secret configuration and bounded NDJSON history remain in the current operating-system user's
application-data directories. PolyTread does not persist raw feed frames, complete order books, or
full market-depth snapshots. See [Configuration](documents/CONFIGURATION.md) for the exact settings
and [Operations](documents/OPERATIONS.md) for backup and upgrade guidance.

## Troubleshooting and help

| Problem | What to try |
| --- | --- |
| `node` or `npm` is not recognized | Install Node.js 18 or newer, then close and reopen the terminal. |
| `polytread` is not recognized after installation | Reopen the terminal and confirm npm's global executable directory is on `PATH`. |
| The native binary is missing | Run `npm install --global polytread` again; installation fails closed if download or checksum verification fails. |
| Setup cannot reach Polymarket | Run `polytread diagnose`. Approve a proposed DNS change only after reading the prompt; use `polytread restore-dns` to restore saved settings. |
| The dashboard link is rejected or expired | Return to the running terminal and open the newest complete `#access=...` link. |
| You need to replace the configured wallet | Run `polytread setup --force` and complete the confirmations again. |

For more detail, start with [Getting Started](documents/GETTING_STARTED.md), then search the
[existing GitHub issues](https://github.com/EH-a0/polytread/issues). When reporting a problem,
include the operating system, Node/npm versions, the exact command, the complete error text, and
the output of `polytread diagnose` when relevant.

Never post a private key, seed phrase, dashboard `#access` fragment, credential-vault secret, or
authenticated RPC URL in an issue, screenshot, or log.

## Native downloads

The recommended npm installation supplies the global `polytread` command and automatically checks
the downloaded binary. Raw native binaries and their `.sha256` files are also available on the
[latest GitHub release](https://github.com/EH-a0/polytread/releases/latest) for Windows x64, Linux
x64/arm64, and macOS Intel/Apple Silicon. These are command-line binaries, not graphical installers;
choose only the asset that matches your operating system and CPU architecture.

## Build from source

Install Rust 1.97, then run the credential-free, view-only public-data service:

```sh
cargo run --locked -- serve
```

To run the same consumer setup wizard from source:

```sh
cargo run --locked --
```

Advanced operators may provide `PM_SIGNER_ADDRESS`, `PM_FUNDER_ADDRESS`, `PM_PRIVATE_KEY`, and
`PM_SIGNATURE_TYPE` through the environment for `serve`. Private keys are intentionally not
accepted as command-line arguments. See [Configuration](documents/CONFIGURATION.md) before using
advanced mode.

## Documentation

- [Getting Started](documents/GETTING_STARTED.md) — installation, first launch, commands, and claims
- [Configuration](documents/CONFIGURATION.md) — local files, environment settings, and trading gates
- [Operations](documents/OPERATIONS.md) — releases, lifecycle, backups, upgrades, and service use
- [Architecture](documents/ARCHITECTURE.md) — runtime components and security boundaries

## Verify a source checkout

Run the complete local publishing gate:

```powershell
./scripts/verify.ps1 -FixFormat
```

On Linux or macOS:

```bash
bash scripts/verify.sh
```

## License

Licensed under either [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.

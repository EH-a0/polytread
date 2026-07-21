# Getting Started

## Consumer install

Requirements are Node.js 18 or newer, a current browser, and a supported native platform:
Windows x64, Linux x64/arm64, or macOS x64/arm64.

Install the published package and start PolyTread:

```sh
npm install --global polytread && polytread
```

The NPM installer downloads the native Rust binary for the current platform and verifies its
SHA-256 checksum. NPM and the JavaScript launcher never receive trading credentials.

## First launch

The full-screen terminal wizard performs these steps:

1. Opens a single **Setup configs** selection; press <kbd>Enter</kbd> to continue.
2. Reads the private key on a masked-input screen.
3. Derives the Polygon signer address and checks the real Polymarket endpoints.
4. Discovers the Polymarket funding wallet from the public profile API.
5. Detects EOA, legacy proxy, Gnosis Safe, or deposit-wallet mode. If public metadata is
   inconclusive, it asks for the wallet type rather than guessing.
6. Authenticates with the Polymarket CLOB and checks pUSD balance and both V2 allowances.
7. Requests an explicit <kbd>Y</kbd> or <kbd>N</kbd> browser-trading choice.
8. Stores the private key and a random shutdown token in the OS credential vault.
9. Stores only non-secret addresses, wallet type, localhost bind, and the trading opt-in in the
   per-user config file.

The progress screen keeps every completed result visible and animates the active row. A valid
zero-balance wallet is accepted with a funding warning; backend balance gates still prevent an
unfunded buy.

Browser orders remain view-only unless you press <kbd>Y</kbd> on the final safety choice. Even after
that opt-in, the dashboard starts disarmed and asks for confirmation before every order.

## Normal commands

```sh
polytread                 # start; runs setup first when needed
polytread status          # check the configured localhost service
polytread shutdown        # authenticated graceful shutdown
polytread setup --force   # replace the saved local setup
```

The dashboard URL is printed after the listener has successfully opened. It is normally
[http://127.0.0.1:9878](http://127.0.0.1:9878).

## Claims

Claimable positions, current open PnL, and today's realized PnL in UTC are presented in the
dashboard. Claims are manual only; there is no automatic claim scheduler.

- EOA: press Claim, review the browser confirmation, and explicitly submit the Polygon
  transaction. POL is required for gas, and a one-time V2 collateral-adapter approval may be sent.
- Safe, legacy proxy, or deposit wallet: use the official Portfolio link. Those wallets require
  Polymarket's separately authenticated relayer, which this consumer package intentionally does
  not impersonate or configure.

## Run from source

Rust 1.97 is selected by `rust-toolchain.toml`.

```powershell
cargo run --locked --
```

For a credential-free, view-only development server:

```powershell
cargo run --locked -- serve
```

Use a different local port or data directory in advanced mode when needed:

```powershell
cargo run --locked -- serve --bind 127.0.0.1:9988 --data-dir D:\polytread-data
```

## Deliberate live smoke command

`trade-smoke` can submit a real order and is never called by tests:

```powershell
$env:PM_SIGNER_ADDRESS = "0x..."
$env:PM_FUNDER_ADDRESS = "0x..."
$env:PM_PRIVATE_KEY = "0x..."
$env:PM_SIGNATURE_TYPE = "0"
cargo run --locked -- trade-smoke --slug <market-slug> --outcome yes --side buy --nominal-usd 1
```

Do not run it with a funded wallet unless you intend to trade.

<div align="center">

# PolyTread

**A local, private dashboard for manually trading Polymarket BTC five-minute markets.**

Watch live markets, review your position, and submit only the orders you explicitly confirm—without
running a public web server.

[![Latest release](https://img.shields.io/github/v/release/EH-a0/polytread?style=flat-square)](https://github.com/EH-a0/polytread/releases/latest)
[![npm](https://img.shields.io/npm/v/polytread?style=flat-square&logo=npm)](https://www.npmjs.com/package/polytread)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue?style=flat-square)](#license)

[![Start with the Setup Guide](https://img.shields.io/badge/START-SETUP_GUIDE-F59E0B?style=for-the-badge&logo=readthedocs&logoColor=white)](documents/GETTING_STARTED.md)
[![Learn the Dashboard](https://img.shields.io/badge/LEARN-DASHBOARD_GUIDE-2563EB?style=for-the-badge&logo=bookstack&logoColor=white)](documents/DASHBOARD_GUIDE.md)

</div>

> [!WARNING]
> PolyTread can submit **real-money orders** and explicit claim transactions. It is not a trading
> strategy, signal, or promise of profit. Use a dedicated wallet, check every confirmation, and
> follow the rules that apply to your Polymarket account and location.

[![PolyTread dashboard showing a live example market with trading disarmed](documents/assets/dashboard/cover-dashboard.jpg)](documents/DASHBOARD_GUIDE.md)

<p align="center"><sub>Dashboard preview with synthetic example data. Trading is disarmed.</sub></p>

## At a glance

| | PolyTread behavior |
| --- | --- |
| **Where it runs** | One native service on your computer |
| **Where it opens** | Your browser on `localhost`—not a public website |
| **How orders happen** | Manually, after your explicit confirmation |
| **Default safety state** | Every dashboard session starts disarmed |
| **What you do not need** | Rust, Python, Docker, a database, a VPS, or a hosted relay |
| **What still needs internet** | Market data, authentication, trading, and claims |

## Total npm downloads

[![Total npm downloads](https://img.shields.io/npm/dt/polytread?style=for-the-badge&logo=npm&logoColor=white&label=total%20npm%20downloads&color=CB3837)](https://www.npmjs.com/package/polytread)

This badge reads the public npm download count through Shields.io and refreshes automatically. Click
it to open the package page and see the currently published version.

## Start here

The recommended setup uses npm. If this is your first time using a terminal, follow the
**[visual Setup Guide](documents/GETTING_STARTED.md)**; it shows every command and the screen you
should expect next.

### 1. Check the requirements

You need:

- [Node.js](https://nodejs.org/en/download) 18 or newer, with npm;
- a current web browser;
- Windows x64, Linux x64/arm64, or macOS Intel/Apple Silicon;
- for wallet setup, the private key of a **dedicated Polymarket signing wallet**.

Check Node.js and npm:

```sh
node --version
npm --version
```

Continue when `node --version` reports `v18` or newer.

### 2. Install PolyTread

```sh
npm install --global polytread
```

The installer selects the native binary for your computer, downloads it from the matching GitHub
release, and verifies its SHA-256 checksum. npm does not ask for or receive your trading key.

### 3. Start PolyTread

```sh
polytread
```

On the first launch, complete the secure terminal setup. On later launches, PolyTread reuses your
saved settings, checks the live services, and shows the local dashboard address.

### 4. Open the complete dashboard link

The runtime screen shows a private link similar to:

```text
PolyTread dashboard: http://127.0.0.1:9878/#access=...
```

Press <kbd>C</kbd> to copy the **complete** link, then paste it into your browser. A successful start
means the dashboard opens, reports its connections, and begins finding the current BTC five-minute
market. It always starts with trading turned off, even when browser trading was allowed during
setup.

> [!NOTE]
> The `#access=...` part is private and changes whenever PolyTread restarts. Do not share it, and do
> not reuse an old or incomplete link. The browser uses it once to create a protected local session,
> then removes it from the address bar.

## What first launch looks like

<table>
  <tr>
    <td width="50%">
      <a href="documents/assets/setup/states/23-complete-enabled.png">
        <img src="documents/assets/setup/states/23-complete-enabled.png" alt="PolyTread secure setup complete screen">
      </a>
    </td>
    <td width="50%">
      <a href="documents/assets/runtime/states/07-runtime-active.png">
        <img src="documents/assets/runtime/states/07-runtime-active.png" alt="PolyTread active local runtime screen">
      </a>
    </td>
  </tr>
  <tr>
    <td align="center"><strong>1. Secure setup</strong><br>Complete the checks and choose whether browser trading is allowed.</td>
    <td align="center"><strong>2. Local runtime</strong><br>Copy the private URL and leave the service running.</td>
  </tr>
</table>

The setup wizard guides you through these steps:

1. Select **Setup configs**.
2. Enter the dedicated wallet key on a masked-input screen.
3. Let PolyTread derive the signer and check real Polymarket connectivity.
4. Confirm the detected funding wallet and wallet type, or enter them when detection is unclear.
5. Review authentication, pUSD balance, and trading allowances.
6. Press <kbd>Y</kbd> or <kbd>N</kbd> to allow or disable browser trading.
7. Save the settings in the operating-system credential vault and continue to the runtime screen.

The **signer** is the address produced from your private key. The **funding wallet** is the account
or contract that holds the Polymarket funds. They can be the same address, but a proxy or Safe setup
may use different addresses. PolyTread normally detects this and asks rather than guessing when it
cannot be certain.

Setup can also finish successfully in view-only mode. If authentication works but no pUSD is
available, you can still open the dashboard, but trading remains unavailable until the wallet is
funded.

### If setup reports a DNS problem

Some networks block the services PolyTread needs. If setup offers a DNS change, read the explanation
first; press <kbd>I</kbd> for the technical and rollback details. Nothing changes unless you type
`YES`. PolyTread records the previous settings so you can restore them later:

```sh
polytread restore-dns
```

Changing DNS only changes how service names are resolved. It does not change your public IP or
decide whether trading is permitted in your location.

## Learn the dashboard safely

The **[Dashboard Guide](documents/DASHBOARD_GUIDE.md)** explains the page from top to bottom with
screenshots: connection states, the price chart, order controls, confirmation steps, activity and
history, positions, and manual claims.

[![Open the Dashboard Guide](https://img.shields.io/badge/OPEN-DASHBOARD_GUIDE-2563EB?style=for-the-badge&logo=bookstack&logoColor=white)](documents/DASHBOARD_GUIDE.md)

Before placing an order, remember:

- **Live** describes the data connection; it does not mean trading is armed.
- Enabling browser trading during setup only makes the controls available.
- You must arm the current dashboard session separately.
- Every order still has a final confirmation step.
- Claimable positions can appear automatically, but PolyTread never claims them automatically.

## What PolyTread does—and does not do

| PolyTread does | PolyTread does not |
| --- | --- |
| Find the current and next BTC five-minute markets | Run an automated trading strategy |
| Stream public BTC and UP/DOWN prices | Produce trading signals or predict a winner |
| Submit explicitly confirmed manual buys and sells | Arm the dashboard automatically |
| Reconcile order status after submission | Submit an order without confirmation |
| Show open PnL and today's realized PnL in UTC | Promise execution, returns, or profitability |
| Keep bounded local market, trade, and portfolio history | Publish your dashboard to the internet |
| Find claimable positions and support manual claims | Claim a position automatically |

## Everyday commands

| Command | What it does |
| --- | --- |
| `polytread` | Start or reconnect to the normal local service |
| `polytread status` | Check whether the configured service is running |
| `polytread shutdown` | Ask the service to stop safely |
| `polytread diagnose` | Check Polymarket REST, WebSocket, and DNS connectivity |
| `polytread setup --force` | Replace the saved wallet setup after stopping the running service |
| `polytread restore-dns` | Restore DNS settings saved during an approved setup change |

On the normal runtime screen, <kbd>Esc</kbd>, <kbd>Q</kbd>, or
<kbd>Ctrl</kbd>+<kbd>C</kbd> closes that terminal view after confirming the background worker. The
service keeps running and the terminal prints the complete dashboard URL again. Use
`polytread shutdown` when you want to stop the service itself.

In advanced `polytread serve` mode, <kbd>Ctrl</kbd>+<kbd>C</kbd> stops the foreground process.

## Safety and privacy

### Your key stays out of the browser

Private-key input is hidden. The private key and local shutdown token are stored in the
operating-system credential vault, with no plaintext-secret fallback. They are never placed in npm,
JavaScript, a command-line argument, the browser, the configuration file, or the history directory.

Non-secret configuration and bounded NDJSON history stay in the current operating-system user's
application-data directories. PolyTread does not store raw feed frames, complete order books, or
full market-depth snapshots. See [Configuration](documents/CONFIGURATION.md) for the exact files and
settings.

### Orders and claims stay explicit

- Consumer mode listens only on the local loopback interface.
- Browser orders remain unavailable unless you explicitly allow them during setup.
- Every dashboard session begins disarmed and every order requires confirmation.
- The backend rechecks the active session, request ID, wallet funds and allowance, position size,
  and bounded execution price.
- EOA claims require explicit confirmation and Polygon POL for gas. A one-time collateral adapter
  approval can also require confirmation.
- Safe, legacy proxy, and deposit-wallet claims use Polymarket's authenticated relayer, so PolyTread
  directs those users to the official Portfolio page instead of collecting builder credentials.

## Troubleshooting

| What you see | What to do |
| --- | --- |
| `node` or `npm` is not recognized | Install Node.js 18 or newer, then close and reopen the terminal. |
| `polytread` is not recognized | Reopen the terminal and confirm npm's global executable directory is on `PATH`. |
| The native binary is missing | Install again. The installer stops if the download or checksum cannot be verified. |
| The terminal says it is too small | Resize it to at least 80 columns by 24 rows, then continue. |
| Connectivity is degraded | Wait for the continuing checks or run `polytread diagnose`; a degraded check is not always a setup failure. |
| Setup cannot reach required services | Run `polytread diagnose`. Only approve a proposed DNS change after reading its prompt. |
| Setup is valid but there is no pUSD | Continue in view-only mode; trading is unavailable until funds are present. |
| The dashboard link is rejected or expired | Return to the runtime screen and copy the newest complete `#access=...` link. |
| You need to replace the configured wallet | Run `polytread shutdown`, then `polytread setup --force`. |
| An approved DNS change must be undone | Run `polytread restore-dns`. |

Still stuck? Read [Getting Started](documents/GETTING_STARTED.md), then search the
[existing GitHub issues](https://github.com/EH-a0/polytread/issues). A useful bug report includes
your operating system, Node/npm versions, the exact command, the complete error text, and
`polytread diagnose` output when relevant.

**Never post** a private key, seed phrase, dashboard `#access` fragment, credential-vault secret,
or authenticated RPC URL in an issue, screenshot, or log.

## Other ways to install or run

### Native downloads

The npm installation is recommended because it supplies the global `polytread` command and checks
the downloaded binary automatically. Raw binaries and their `.sha256` files are also available on
the [latest GitHub release](https://github.com/EH-a0/polytread/releases/latest) for Windows x64,
Linux x64/arm64, and macOS Intel/Apple Silicon.

These files are command-line binaries, not graphical installers. Download only the asset that
matches your operating system and CPU architecture.

### Build from source

Install Rust 1.97, then start the credential-free, view-only public-data service:

```sh
cargo run --locked -- serve
```

To use the same consumer setup wizard from source:

```sh
cargo run --locked --
```

Advanced operators can provide `PM_SIGNER_ADDRESS`, `PM_FUNDER_ADDRESS`, `PM_PRIVATE_KEY`, and
`PM_SIGNATURE_TYPE` through the environment for `serve`. Private keys are intentionally not
accepted as command-line arguments. Read [Configuration](documents/CONFIGURATION.md) before using
advanced mode.

## Documentation

| Guide | Start here when you want to... |
| --- | --- |
| **[Getting Started](documents/GETTING_STARTED.md)** | Install from npm and reach the first dashboard with screenshots |
| **[Dashboard Guide](documents/DASHBOARD_GUIDE.md)** | Understand every status, control, order step, activity tab, position, and claim path |
| [Configuration](documents/CONFIGURATION.md) | Learn where local files live and how settings and trading gates work |
| [Operations](documents/OPERATIONS.md) | Back up, upgrade, diagnose, and manage the service lifecycle |
| [Architecture](documents/ARCHITECTURE.md) | Understand runtime components and security boundaries |

<div align="center">

[![Start with the Setup Guide](https://img.shields.io/badge/START-SETUP_GUIDE-F59E0B?style=for-the-badge&logo=readthedocs&logoColor=white)](documents/GETTING_STARTED.md)
[![Learn the Dashboard](https://img.shields.io/badge/LEARN-DASHBOARD_GUIDE-2563EB?style=for-the-badge&logo=bookstack&logoColor=white)](documents/DASHBOARD_GUIDE.md)

</div>

## Verify a source checkout

Run the complete local publishing gate on Windows:

```powershell
./scripts/verify.ps1 -FixFormat
```

On Linux or macOS:

```bash
bash scripts/verify.sh
```

## License

Licensed under either [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.

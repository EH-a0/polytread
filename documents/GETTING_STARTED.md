# Getting Started with PolyTread

This guide takes you from an empty terminal to the local PolyTread dashboard. You do not need to
know Rust, run Docker, or manage a server.

> [!WARNING]
> PolyTread can submit real-money orders after you explicitly enable and arm browser trading. Use
> a dedicated Polymarket wallet, read every confirmation, and never share a private key, seed
> phrase, or dashboard access link.

> [!NOTE]
> Every screenshot below was generated from the current source with disposable example data. No
> real wallet or order appears in the images. The latest npm release can briefly lag behind the
> source between releases, so an installed screen may look slightly different while keeping the
> same safety rules and flow.

## What you will do

1. Install Node.js and npm.
2. Install the `polytread` command.
3. Complete the secure terminal setup.
4. Start the local service.
5. Open its private dashboard link in your browser.

PolyTread runs on your own computer and opens only on `localhost`. It still needs internet access
to read market data, authenticate, place an order, or submit a supported claim.

## Before you start

Have these ready:

- Windows x64, Linux x64/arm64, or macOS Intel/Apple Silicon;
- [Node.js](https://nodejs.org/en/download) 18 or newer, including npm;
- a current web browser;
- the private key for a **dedicated** Polymarket signing wallet; and
- enough terminal space for at least 80 columns by 24 rows.

For a first tour, choose **view-only mode** when setup asks about browser trading. You can inspect
the full dashboard without enabling browser orders.

### Three useful terms

| Term | Plain meaning |
| --- | --- |
| **Signer** | The wallet address created from the private key you enter. It signs requests. |
| **Funding wallet** | The Polymarket account or contract that actually holds the funds. It can be the signer, a proxy, or a Safe. |
| **pUSD** | The collateral balance PolyTread can use for Polymarket orders. A wallet can be valid but still have no pUSD available. |

PolyTread normally discovers the funding wallet and wallet type. If the public information is not
clear enough, it asks you rather than guessing.

## 1. Check Node.js and npm

Open PowerShell, Terminal, or another command prompt and run:

```sh
node --version
npm --version
```

The first command must show `v18` or a newer major version. If either command is not recognized,
install Node.js, close the terminal, open a new terminal, and try again.

## 2. Install PolyTread

Run:

```sh
npm install --global polytread
```

The installer selects the native PolyTread binary for your operating system and processor,
downloads it from the matching GitHub release, and checks its SHA-256 checksum. npm and the small
JavaScript launcher never ask for or receive your trading credentials.

If `polytread` is not recognized after installation, close and reopen the terminal. If it still
fails, check that npm's global executable directory is on your `PATH`.

## 3. Start the first-time setup

Run:

```sh
polytread
```

The opening menu has one safe first-run choice. Leave **Setup configs** selected and press
<kbd>Enter</kbd>.

![PolyTread setup selection](assets/setup/states/01-welcome.png)

The setup screen keeps completed checks visible, highlights the current step, and shows the keys
you can press at the bottom. If the whole screen does not fit, enlarge the terminal before
continuing.

## 4. Enter the signing key

PolyTread next asks for the private key of the dedicated signing wallet.

![Private-key input screen](assets/setup/states/02-private-key-empty.png)

Paste or type the key and press <kbd>Enter</kbd>. The value is masked on screen:

![Masked private-key input](assets/setup/states/03-private-key-masked.png)

This key is used only by the native PolyTread process. After validation it is stored in your
operating-system credential vault. It is not stored in npm, JavaScript, the browser, a command-line
argument, or a plaintext config file.

Never put the key in a screenshot, GitHub issue, chat message, shell-history example, or log.

## 5. Let PolyTread check the connection and wallet

PolyTread now:

1. derives the signer address;
2. checks the required Polymarket REST, WebSocket, and DNS paths;
3. discovers the funding wallet and wallet type;
4. authenticates with the Polymarket CLOB; and
5. checks the pUSD balance and trading approvals.

![Connectivity checks in progress](assets/setup/states/07-connectivity-running.png)

A completed line stays visible. A warning explains what is missing and whether setup can safely
continue. For example, a valid wallet with no available pUSD can finish setup, but the backend
will block buys until the balance and approval are sufficient.

![Valid setup with no available pUSD](assets/setup/states/20-browser-choice-no-funds.png)

### If DNS is blocked

Some filtered networks cannot resolve a required endpoint. PolyTread may offer an operating-system
DNS change and first shows a plain-language approval screen:

![DNS change approval](assets/setup/states/09-dns-acknowledgement-empty.png)

- Press <kbd>I</kbd> to read the exact technical and rollback details.
- Type `YES` only if you approve the described operating-system change.
- Any other answer leaves DNS unchanged.
- Restore saved DNS settings later with `polytread restore-dns`.

![DNS technical and rollback details](assets/setup/states/10-dns-change-details.png)

This changes DNS resolution only. It does not change your public IP, create a VPN, or determine
whether trading is permitted for your account or location.

## 6. Choose whether the browser may place orders

After the checks pass, PolyTread asks for one explicit choice:

![Browser-trading choice](assets/setup/states/21-browser-choice-funded.png)

- Press <kbd>N</kbd> for **view-only mode**. This is the recommended first tour.
- Press <kbd>Y</kbd> to make manual order controls available.

Choosing <kbd>Y</kbd> does not place an order and does not leave trading armed. Every dashboard
session starts disarmed, and every order still needs a separate browser confirmation.

## 7. Finish setup and start the runtime

PolyTread saves the secret values in the operating-system credential vault and non-secret settings
in the current user's application-data directory. You will see one of these successful endings:

| If you chose | Completion screen |
| --- | --- |
| Browser trading enabled | [Open the enabled completion screen](assets/setup/states/23-complete-enabled.png) |
| View-only mode | [Open the view-only completion screen](assets/setup/states/24-complete-view-only.png) |

The enabled completion screen is shown here because it also summarizes the remaining safety gates:

![Setup complete with browser trading available](assets/setup/states/23-complete-enabled.png)

Press <kbd>Enter</kbd>. The returning-user runtime repeats the startup checks, skips permissions
that are already granted, and waits until the local listener is ready.

![Returning-user checks](assets/runtime/states/01-checks-starting.png)

When startup is complete, the runtime screen shows the dashboard address and recent bounded logs:

![Active local runtime](assets/runtime/states/07-runtime-active.png)

## 8. Open the exact dashboard link

The runtime prints a link similar to:

```text
PolyTread dashboard: http://127.0.0.1:9878/#access=...
```

Press <kbd>C</kbd> to copy the **complete** link, including `#access=...`, and open it in your
browser. Do not manually copy only the visible first line if the terminal wraps the address.

The access fragment establishes an HttpOnly session for that local browser and is then removed
from the address bar. It rotates whenever PolyTread restarts, so an old, partial, bookmarked, or
shared access link will not work.

When the service has connected and found a market, the dashboard looks like this:

![PolyTread live dashboard](assets/dashboard/cover-dashboard.jpg)

Success means that:

- the page opens without an access warning;
- the top-right status changes from connecting to `LIVE` or a clearly explained degraded state;
- the current BTC five-minute market appears; and
- the dashboard remains disarmed until you deliberately arm it.

If no five-minute market is open, waiting is normal. PolyTread keeps looking for the current or
next session.

## 9. Leave it running or stop it

On the runtime screen:

- <kbd>C</kbd> copies the newest complete dashboard URL;
- <kbd>Esc</kbd>, <kbd>Q</kbd>, or <kbd>Ctrl</kbd>+<kbd>C</kbd> closes the screen and leaves a verified
  no-console PolyTread worker running; and
- `polytread shutdown` explicitly stops that background service.

Useful commands for later:

```sh
polytread
polytread status
polytread diagnose
polytread shutdown
polytread setup --force
polytread restore-dns
```

Do not run `polytread setup --force` while the current service is running. Stop it first, because a
forced setup replaces saved settings and rotates the local control secret.

## Troubleshooting

| What you see | What it means | What to do |
| --- | --- | --- |
| Terminal too small | The full safety text cannot fit. | Resize to at least 80x24 and continue. [See screen](assets/setup/states/27-terminal-too-small.png). |
| Connectivity warning, setup continues | One path is degraded but the remaining checks can continue. | Read the warning; use `polytread diagnose` if it persists. [See screen](assets/setup/states/08-connectivity-degraded.png). |
| Connectivity failure | A required endpoint cannot be reached safely. | Run `polytread diagnose`; review DNS or network restrictions. [See screen](assets/setup/states/15-connectivity-failure.png). |
| Funding-wallet or wallet-type question | Public discovery was inconclusive. This is not automatically an error. | Enter the known funding address or choose the wallet type carefully. [Funding screen](assets/setup/states/16-funding-wallet-empty.png) · [wallet-type screen](assets/setup/states/18-wallet-type-selection.png). |
| No available pUSD | Authentication succeeded, but buys cannot be funded or approved. | Add collateral and approval before trying to trade; view-only use still works. [See screen](assets/setup/states/20-browser-choice-no-funds.png). |
| Authentication or wallet mismatch | The signer, funding wallet, wallet type, or API authentication does not agree. | Check the chosen account details; do not guess repeatedly. [See screen](assets/setup/states/25-authentication-failure.png). |
| DNS rollback failed | PolyTread could not automatically restore the recorded settings. | Run the exact displayed `polytread restore-dns` recovery command with the required OS permissions. [See screen](assets/setup/states/26-dns-rollback-failure.png). |
| Dashboard access required | The URL is old, incomplete, or was never exchanged for a session. | Return to the runtime and press <kbd>C</kbd> for the newest complete link. [See screen](assets/dashboard/01-access-required.jpg). |

When asking for help, include your operating system, Node/npm versions, the exact command, the
complete error text, and `polytread diagnose` output when relevant. Remove private keys, seed
phrases, dashboard access fragments, credential-vault secrets, and authenticated RPC URLs first.

## Complete first-run screen reference

The happy path above stays short on purpose. This reference preserves every deterministic setup
screen so an instruction or support reply can point to an exact state.

| # | State | Screenshot |
| ---: | --- | --- |
| 01 | Setup selection | [View](assets/setup/states/01-welcome.png) |
| 02 | Empty private-key input | [View](assets/setup/states/02-private-key-empty.png) |
| 03 | Masked private key | [View](assets/setup/states/03-private-key-masked.png) |
| 04 | Empty-key validation | [View](assets/setup/states/04-private-key-empty-error.png) |
| 05 | Invalid signing key | [View](assets/setup/states/05-private-key-invalid-error.png) |
| 06 | Deriving signer | [View](assets/setup/states/06-derive-signer-running.png) |
| 07 | Connectivity checks | [View](assets/setup/states/07-connectivity-running.png) |
| 08 | Degraded connectivity that can continue | [View](assets/setup/states/08-connectivity-degraded.png) |
| 09 | DNS acknowledgement | [View](assets/setup/states/09-dns-acknowledgement-empty.png) |
| 10 | DNS technical details | [View](assets/setup/states/10-dns-change-details.png) |
| 11 | Exact DNS approval entered | [View](assets/setup/states/11-dns-acknowledgement-yes.png) |
| 12 | Invalid DNS approval response | [View](assets/setup/states/12-dns-acknowledgement-invalid.png) |
| 13 | Approved DNS change in progress | [View](assets/setup/states/13-dns-change-locked.png) |
| 14 | Required operating-system acknowledgement | [View](assets/setup/states/14-dns-operating-system-step.png) |
| 15 | Blocking connectivity failure | [View](assets/setup/states/15-connectivity-failure.png) |
| 16 | Manual funding-wallet fallback | [View](assets/setup/states/16-funding-wallet-empty.png) |
| 17 | Invalid funding-wallet address | [View](assets/setup/states/17-funding-wallet-invalid.png) |
| 18 | Manual wallet-type selection | [View](assets/setup/states/18-wallet-type-selection.png) |
| 19 | Authentication and funds checks | [View](assets/setup/states/19-credentials-authenticating.png) |
| 20 | Valid wallet with no available pUSD | [View](assets/setup/states/20-browser-choice-no-funds.png) |
| 21 | Funded browser-trading choice | [View](assets/setup/states/21-browser-choice-funded.png) |
| 22 | Saving credentials and settings | [View](assets/setup/states/22-saving-configuration.png) |
| 23 | Complete with browser trading available | [View](assets/setup/states/23-complete-enabled.png) |
| 24 | Complete in view-only mode | [View](assets/setup/states/24-complete-view-only.png) |
| 25 | Authentication or wallet mismatch | [View](assets/setup/states/25-authentication-failure.png) |
| 26 | DNS rollback failure and recovery command | [View](assets/setup/states/26-dns-rollback-failure.png) |
| 27 | Terminal too small | [View](assets/setup/states/27-terminal-too-small.png) |

<details>
<summary>Open the 27-screen setup contact sheet</summary>

![All PolyTread setup screens](assets/setup/polytread-setup-all-states.png)

</details>

## Next: learn the dashboard

Continue to the [Dashboard Guide](DASHBOARD_GUIDE.md) for a plain-language tour of every control,
status, order step, activity tab, and claim path.

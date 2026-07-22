# PolyTread

PolyTread is a local browser dashboard for manually trading Polymarket BTC five-minute markets.
This package installs its native open-source Rust binary and exposes the global `polytread`
command.

## Requirements

- Node.js 18 or newer;
- a current browser;
- Windows x64, Linux x64/arm64, or macOS Intel/Apple Silicon.

## Install and start

```sh
npm install --global polytread
polytread
```

The installer downloads the matching GitHub release binary and verifies its SHA-256 checksum.
It never requests or receives trading credentials.

On first launch, PolyTread opens a full-screen terminal setup wizard with a single setup selection,
masked private-key input, and an animated validation checklist. The private key is stored in the
operating-system credential vault, not in npm, JavaScript, the dashboard, a command-line argument,
or a plaintext config file.

When setup completes, the service shows a rotating localhost dashboard link containing
`#access=...`. On later launches, press `C` in the runtime screen to copy that complete private
link. Press `Esc` to close the screen while PolyTread continues in a no-console background worker;
stop the service later with `polytread shutdown`. Browser trading remains opt-in, the dashboard
starts disarmed, and every order requires confirmation.

PolyTread can place real-money orders. It is not a trading strategy or promise of profitability;
use a dedicated wallet and follow the rules that apply to your account and location.

Useful commands:

```sh
polytread status
polytread diagnose
polytread shutdown
```

See the [source repository](https://github.com/EH-a0/polytread) for the full getting-started guide,
troubleshooting, security boundaries, source builds, and licenses.

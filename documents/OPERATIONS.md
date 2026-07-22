# Operations

## Release build

```bash
cargo build --locked --release
```

The only executable is `target/release/polytread` (or `polytread.exe` on Windows).

Tagged GitHub releases run `.github/workflows/release.yml`, build the five supported native
targets, and attach each binary plus its SHA-256 file. The version in `npm/package.json` must match
the Git tag before the NPM package is published. The installer fails if an asset is absent or its
checksum differs.

The package metadata and installer target `EH-a0/polytread`. If the repository ever moves, update
both locations together before publishing another release.

## Local service

The advanced safe default is view-only and loopback-bound:

```bash
./target/release/polytread serve
```

Health is `GET /healthz`; the dashboard is `GET /`. The process writes logs to standard output and
shuts down on Ctrl+C or process termination.

Consumer mode prints the URL only after binding succeeds. Its normal lifecycle is:

```bash
polytread
polytread status
polytread shutdown
```

The returning-user runtime screen uses <kbd>C</kbd> to copy the complete private dashboard URL.
<kbd>Esc</kbd>, <kbd>Q</kbd>, and <kbd>Ctrl</kbd>+<kbd>C</kbd> close that screen and hand the runtime
to a verified no-console worker; they do not stop the consumer service. `polytread shutdown` uses
the same-user local control channel and remains the explicit stop command. Advanced `serve` mode
continues to treat <kbd>Ctrl</kbd>+<kbd>C</kbd> as foreground shutdown.

The HTTP shutdown endpoint is absent from advanced `serve` mode. In consumer mode it accepts only
a loopback peer with the random bearer token stored in the OS credential vault.

## Linux service example

The tracked `scripts/polytread.service` is a generic advanced-mode systemd example. It assumes:

- binary at `/opt/polytread/polytread`;
- service user and group named `polytread`;
- history directory `/var/lib/polytread`;
- optional environment file `/etc/polytread.env`.

Review and adapt every path. Keep the environment file outside the repository, owned by the
service user or root, and not world-readable. Headless service deployments may not provide an OS
desktop credential vault, so the NPM consumer wizard deliberately fails closed there; use the
advanced environment-driven service only when you accept and secure that operational model.

## Browser access

The service has no built-in public deployment or remote-access layer. Keep it on loopback for local
use. A separate public deployment must supply its own TLS, authentication, request limits,
firewall, and origin controls. A plain public bind is unsafe for an order-capable service.

## History care

Back up the data directory while the process is stopped for a consistent copy. Files are plain
NDJSON. Stop the process before rotating, archiving, or renaming them; do not edit a live file in
place.

Malformed history rows are skipped with a warning so an interrupted append does not block restart.
History contains market, order, PnL, and claim metadata but never the signing key or control token.

## Upgrade procedure

1. Stop consumer mode with `polytread shutdown`; stop advanced foreground `serve` mode with Ctrl+C.
2. Back up the per-user data directory or advanced data directory.
3. Run `scripts/verify.sh` or `scripts/verify.ps1` on the candidate source.
4. Install the checksum-verified package/binary.
5. Start and confirm `/healthz`, feeds, sessions, persisted trades, PnL, and claim visibility.
6. Re-enable browser trading only after validating the build.

No migration service is required for the five append-only files.

## NPM package validation

Check packaging code without downloading a release:

```bash
npm --prefix npm run check
npm pack --dry-run ./npm
```

For a local end-to-end launcher test, build Rust and install the package with
`POLYTREAD_BINARY_PATH` set to that local binary. That override is for packaging tests; normal
installs download and verify the matching GitHub release asset.

## Documentation screenshot galleries

The setup and returning-user runtime screenshots are rendered from the real Ratatui view with
fixed, synthetic state. They do not read the credential vault or contact a trading endpoint.

```powershell
./scripts/render-setup-gallery.ps1 -Gallery Setup -OutputDir C:\temp\polytread-setup-gallery
./scripts/render-setup-gallery.ps1 -Gallery Runtime -OutputDir C:\temp\polytread-runtime-gallery
```

Each run exports individual `states/*.png` files, category sheets, one all-states contact sheet,
JSON render data, and a local HTML index. Copy only the reviewed PNG deliverables into
`documents/assets`; the large JSON and generated HTML are inspection artifacts.

The browser-dashboard guide uses a localhost-only synthetic fixture around the production
`web/dashboard.html`:

```powershell
python scripts/dashboard-guide-fixture.py --port 8765
```

Open `http://127.0.0.1:8765/web/dashboard.html?state=live`. Supported `state` groups are:

- access and transport: `access-required`, `waiting`, `local`, and `reconnecting`;
- normal controls: `view-only`, `live`, and `armed`;
- connectivity: `degraded`, `dns-filtering`, `endpoint-restricted`, and `unreachable`;
- market data: `stale-book`, `empty-book`, `closing`, `missing-target`, `missing-chainlink`,
  `chainlink-below-target`, and `chainlink-at-target`;
- trading readiness: `no-funds`, `balance-stale`, `balance-error`, `balance-shortfall`,
  `approval-shortfall`, `minimum-too-high`, `maker-cutoff`, `in-flight-order`, and `order-error`;
- portfolio and claims: `portfolio-stale`, `negative-pnl`, `claiming`, `claim-error`,
  `claim-success`, `proxy-claims`, and `empty-claims`; and
- local history: `empty-history`.

The fixture freezes browser time at its example snapshot, disables motion, rejects unknown states,
returns no-op command acknowledgements, and contains no wallet, credential, live endpoint, or real
order path. Stop it with <kbd>Ctrl</kbd>+<kbd>C</kbd> after the screenshots are captured.

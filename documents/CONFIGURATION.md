# Configuration

Run `polytread --help` and subcommand help for the authoritative CLI.

## Consumer mode

Running `polytread` without a subcommand uses the per-user consumer config and OS credential vault.
The config contains only:

- signer and funding-wallet public addresses;
- detected signature type;
- the fixed loopback listener;
- the user's explicit browser-trading opt-in;
- a config schema version.

The private key and local shutdown token are separate vault entries. There is no plaintext-secret
fallback: setup fails closed when the platform credential vault is unavailable.

An operator may set `POLYTREAD_POLYGON_RPC_URL` to replace the public Polygon RPC used only for an
explicit EOA claim. Keep authenticated RPC URLs out of shell history and never commit them.

Use `polytread setup --force` to validate and replace a configuration. It never prints the private
key. `polytread status` and `polytread shutdown` use the listener stored in this config.

## Advanced service settings

These settings apply to `polytread serve`:

| CLI or environment | Default | Purpose |
| --- | --- | --- |
| `--bind` / `POLYTREAD_BIND` | `127.0.0.1:9878` | HTTP, SSE, and WebSocket listener |
| `--data-dir` / `POLYTREAD_DATA_DIR` | `./data` | Local NDJSON history directory |
| `--allow-web-trading` / `POLYTREAD_ALLOW_WEB_TRADING` | `false` | Accept browser order commands |
| `--history-seconds` | `600` | In-memory one-second samples sent to the browser |
| `--discovery-poll-seconds` | `15` | Public market discovery interval |
| `--websocket-heartbeat-seconds` | `5` | Feed health heartbeat |
| `--duration-seconds` | unset | Optional bounded runtime for validation |
| `--log-filter` / `POLYTREAD_LOG` | `info` | Tracing filter |

Consumer mode keeps the listener and data directory fixed to a predictable per-user store plus
localhost-only access.

## Advanced trading credentials

All four environment values are required together. Private keys are not accepted on the command
line because process arguments may be visible to other local users or tools.

| Environment | Meaning |
| --- | --- |
| `PM_SIGNER_ADDRESS` | Polygon address derived from the signing key |
| `PM_FUNDER_ADDRESS` | Address or contract wallet that holds funds |
| `PM_PRIVATE_KEY` | Signing key, read from the environment only in advanced mode |
| `PM_SIGNATURE_TYPE` | `0` EOA, `1` legacy proxy, `2` Gnosis Safe, or `3` deposit wallet |

Use `scripts/polytread.env.example` only as a shape reference. Never commit a populated copy.

## Trading gates

Credentials alone do not enable orders. The server also requires the browser-trading opt-in, and
the dashboard must be explicitly armed. Every submit includes the expected session slug and a
unique request ID.

Only the browser's nominal values are accepted: `0.5`, `1`, `2`, `3`, `4`, and `5` USD. Backend
preflight checks use current balance, the correct V2 standard/negative-risk allowance, and current
position size.

## PnL definitions

- Current open PnL is the sum of `cashPnl` for current positions returned by Polymarket's Data API.
- Today realized PnL is the sum of closed-position `realizedPnl` rows whose timestamps fall within
  the current UTC day.

These labels intentionally state their comparator. The values are not projections and are not
calculated from locally sampled prices.

## Network ownership

PolyTread contains no public hostname, certificate automation, proxy credential, or built-in relay.
The open-source consumer path remains on localhost. Any separate public deployment, TLS, access
control, firewall, or service manager is operator-owned and outside this package.

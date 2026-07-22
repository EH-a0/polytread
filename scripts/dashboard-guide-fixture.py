#!/usr/bin/env python3
"""Serve the production dashboard with synthetic data for documentation screenshots.

This helper binds to localhost only. It never reads PolyTread configuration, credentials,
history, or live endpoints, and command requests are acknowledged without taking action.
"""

from __future__ import annotations

import argparse
import copy
import json
import math
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse


REPOSITORY_ROOT = Path(__file__).resolve().parents[1]
DASHBOARD_PATH = REPOSITORY_ROOT / "web" / "dashboard.html"
EXAMPLE_NOW_MS = 1_800_000_000_000
VALID_STATES = {
    "access-required",
    "approval-shortfall",
    "armed",
    "balance-error",
    "balance-shortfall",
    "balance-stale",
    "chainlink-at-target",
    "chainlink-below-target",
    "claim-error",
    "claim-success",
    "claiming",
    "closing",
    "degraded",
    "dns-filtering",
    "empty-book",
    "empty-claims",
    "empty-history",
    "endpoint-restricted",
    "in-flight-order",
    "live",
    "local",
    "maker-cutoff",
    "minimum-too-high",
    "missing-chainlink",
    "missing-target",
    "negative-pnl",
    "no-funds",
    "order-error",
    "portfolio-stale",
    "proxy-claims",
    "reconnecting",
    "stale-book",
    "unreachable",
    "view-only",
    "waiting",
}


def _fixture_head() -> str:
    """Freeze time and motion without changing the production dashboard file."""

    return f"""
<style>
  *, *::before, *::after {{ animation: none !important; transition: none !important; }}
</style>
<script>
(() => {{
  const fixedNow = {EXAMPLE_NOW_MS};
  const RealDate = Date;
  class FixtureDate extends RealDate {{
    constructor(...args) {{ super(...(args.length ? args : [fixedNow])); }}
    static now() {{ return fixedNow; }}
  }}
  FixtureDate.parse = RealDate.parse;
  FixtureDate.UTC = RealDate.UTC;
  window.Date = FixtureDate;
}})();
</script>
"""


def _session(slug: str, title: str, start_ms: int) -> dict:
    return {
        "slug": slug,
        "title": title,
        "start_ms": start_ms,
        "end_ms": start_ms + 300_000,
        "price_to_beat": 104_250.00,
        "up_token_id": "example-up-token",
        "down_token_id": "example-down-token",
        "active": True,
        "closed": False,
    }


def _price_history() -> list[dict]:
    rows = []
    for index in range(180):
        timestamp = EXAMPLE_NOW_MS - (179 - index) * 1_000
        wave = math.sin(index / 13) * 18
        rows.append(
            {
                "timestamp_ms": timestamp,
                "session_slug": "btc-updown-5m-example",
                "binance_btc_usd": 104_240 + wave + index * 0.08,
                "chainlink_btc_usd": 104_238 + wave * 0.82 + index * 0.07,
                "up_price": 0.51,
                "down_price": 0.49,
            }
        )
    return rows


def _book(bid_start: float, ask_start: float) -> dict:
    return {
        "bids": [
            {"price": round(bid_start - index * 0.01, 2), "size": 90 + index * 17}
            for index in range(8)
        ],
        "asks": [
            {"price": round(ask_start + index * 0.01, 2), "size": 78 + index * 14}
            for index in range(8)
        ],
        "updated_at_ms": EXAMPLE_NOW_MS - 850,
    }


def _ledger() -> list[dict]:
    rows = [
        ("BuyUp", "FastTaker", 0.52, 3.84, "filled", "Filled completely"),
        ("BuyDown", "FastMaker", 0.47, 4.25, "open", "Resting on the order book"),
        ("BuyUp", "FastTaker", 0.55, 1.81, "partial_fill", "Partially filled"),
    ]
    return [
        {
            "local_id": f"example-order-{index}",
            "order_id": f"example-clob-{index}",
            "fingerprint": f"example-fingerprint-{index}",
            "session_slug": "btc-updown-5m-example",
            "market_label": "BTC Up or Down — example 5-minute market",
            "trade_side": side,
            "order_side": "BUY",
            "mechanism": mechanism,
            "token_id": "example-token",
            "price": price,
            "shares": shares,
            "nominal_usd": round(price * shares, 2),
            "status": status,
            "detail": detail,
            "created_at_ms": EXAMPLE_NOW_MS - (index + 2) * 80_000,
            "updated_at_ms": EXAMPLE_NOW_MS - (index + 1) * 42_000,
        }
        for index, (side, mechanism, price, shares, status, detail) in enumerate(rows)
    ]


def _portfolio(*, direct_claim_supported: bool = True) -> dict:
    return {
        "current_open_pnl_usd": 1.42,
        "today_realized_pnl_usd": 3.18,
        "current_value_usd": 18.73,
        "open_positions": 2,
        "claimable_positions": [
            {
                "condition_id": "0xexample-condition-one",
                "title": "BTC Up or Down — resolved example",
                "slug": "btc-updown-5m-resolved-example",
                "outcomes": "UP",
                "shares": 7.25,
                "redeemable_value_usd": 7.25,
                "cash_pnl_usd": 2.11,
                "negative_risk": False,
            },
            {
                "condition_id": "0xexample-condition-two",
                "title": "BTC Up or Down — second resolved example",
                "slug": "btc-updown-5m-resolved-example-two",
                "outcomes": "DOWN",
                "shares": 4.50,
                "redeemable_value_usd": 4.50,
                "cash_pnl_usd": -0.36,
                "negative_risk": False,
            },
        ],
        "claim_history": [
            {
                "condition_id": "0xexample-old-condition",
                "title": "BTC Up or Down — earlier example",
                "transaction_hash": "0x" + "12" * 32,
                "claimed_at_ms": EXAMPLE_NOW_MS - 86_400_000,
            }
        ],
        "updated_at_ms": EXAMPLE_NOW_MS - 1_100,
        "last_error": None,
        "claim_status": "No claim submitted",
        "claim_in_flight_condition_id": None,
        "direct_claim_supported": direct_claim_supported,
        "manual_claim_only": True,
        "wallet_type": "EOA" if direct_claim_supported else "Gnosis Safe",
    }


def _base_snapshot() -> dict:
    current = _session(
        "btc-updown-5m-example",
        "Bitcoin Up or Down — example 5-minute market",
        EXAMPLE_NOW_MS - 60_000,
    )
    next_session = _session(
        "btc-updown-5m-example-next",
        "Bitcoin Up or Down — next example market",
        EXAMPLE_NOW_MS + 240_000,
    )
    past = []
    for index in range(4):
        start = EXAMPLE_NOW_MS - (index + 1) * 300_000
        past.append(
            {
                "observed_at_ms": start,
                "session": _session(
                    f"btc-updown-5m-past-{index + 1}",
                    f"Bitcoin Up or Down — past example {index + 1}",
                    start,
                ),
            }
        )
    return {
        "now_ms": EXAMPLE_NOW_MS,
        "web_trading_allowed": True,
        "minimum_buy_order_usd": 1.0,
        "minimum_maker_remaining_ms": 183_000,
        "maximum_trading_balance_age_ms": 10_000,
        "current_session": current,
        "next_session": next_session,
        "past_sessions": past,
        "binance_btc_usd": 104_267.43,
        "chainlink_btc_usd": 104_263.18,
        "up": {
            "last_trade": 0.52,
            "best_bid": 0.51,
            "best_ask": 0.52,
            "updated_at_ms": EXAMPLE_NOW_MS - 700,
            "minimum_order_size": 5.0,
            "tick_size": 0.01,
        },
        "down": {
            "last_trade": 0.49,
            "best_bid": 0.48,
            "best_ask": 0.49,
            "updated_at_ms": EXAMPLE_NOW_MS - 700,
            "minimum_order_size": 5.0,
            "tick_size": 0.01,
        },
        "orderbook": {"up": _book(0.51, 0.52), "down": _book(0.48, 0.49)},
        "price_history": _price_history(),
        "feeds": [
            {
                "feed": "binance_spot",
                "connected": True,
                "reconnects": 0,
                "last_message_ms": EXAMPLE_NOW_MS - 350,
                "last_error": None,
                "status": "live",
            },
            {
                "feed": "chainlink_rtds",
                "connected": True,
                "reconnects": 0,
                "last_message_ms": EXAMPLE_NOW_MS - 420,
                "last_error": None,
                "status": "live",
            },
            {
                "feed": "market",
                "connected": True,
                "reconnects": 0,
                "last_message_ms": EXAMPLE_NOW_MS - 290,
                "last_error": None,
                "status": "live",
            },
        ],
        "connectivity": {
            "kind": "available",
            "headline": "Polymarket connectivity is available",
            "detail": "Required REST and WebSocket endpoints are responding.",
            "checked_at_ms": EXAMPLE_NOW_MS - 2_000,
            "clob_rest_ok": True,
            "market_rest_ok": True,
            "market_websocket_ok": True,
            "setup_ready": True,
        },
        "trading": {
            "enabled": False,
            "configured": True,
            "selected_nominal": 1.0,
            "selected_mechanism": "FastTaker",
            "selected_side": None,
            "order_status": "Ready for a manually confirmed order.",
            "ready_to_trade": True,
            "last_error": None,
            "active_order_id": None,
            "orders_placed": 3,
            "orders_cancelled": 0,
            "available_usdc": 25.0,
            "allowance_usdc": 25.0,
            "balance_updated_ms": EXAMPLE_NOW_MS - 600,
            "balance_error": None,
            "ledger": _ledger(),
            "in_flight_intent": None,
            "last_submitted_fingerprint": None,
            "last_submitted_session_slug": None,
        },
        "portfolio": _portfolio(),
    }


def snapshot_for(state: str) -> dict:
    if state not in VALID_STATES:
        raise ValueError(f"unknown documentation fixture state: {state}")
    snapshot = copy.deepcopy(_base_snapshot())
    if state == "waiting":
        snapshot["current_session"] = None
        snapshot["next_session"] = None
        snapshot["price_history"] = []
        snapshot["binance_btc_usd"] = None
        snapshot["chainlink_btc_usd"] = None
        snapshot["up"] = {}
        snapshot["down"] = {}
        snapshot["orderbook"] = {"up": {}, "down": {}}
        snapshot["feeds"] = []
        snapshot["connectivity"] = {
            "kind": "checking",
            "headline": "Checking Polymarket connectivity",
            "detail": "Testing the required live endpoints.",
            "checked_at_ms": EXAMPLE_NOW_MS,
            "clob_rest_ok": False,
            "market_rest_ok": False,
            "market_websocket_ok": False,
            "setup_ready": False,
        }
        snapshot["trading"]["configured"] = False
        snapshot["trading"]["available_usdc"] = None
        snapshot["trading"]["allowance_usdc"] = None
        snapshot["trading"]["balance_updated_ms"] = None
        snapshot["trading"]["ledger"] = []
        snapshot["portfolio"]["updated_at_ms"] = None
        snapshot["portfolio"]["claimable_positions"] = []
    elif state == "view-only":
        snapshot["web_trading_allowed"] = False
    elif state == "armed":
        snapshot["trading"]["enabled"] = True
    elif state == "no-funds":
        snapshot["trading"]["available_usdc"] = 0.0
        snapshot["trading"]["allowance_usdc"] = 0.0
    elif state == "balance-stale":
        snapshot["trading"]["balance_updated_ms"] = EXAMPLE_NOW_MS - 60_000
    elif state == "balance-error":
        snapshot["trading"]["balance_updated_ms"] = EXAMPLE_NOW_MS - 60_000
        snapshot["trading"]["balance_error"] = "Example balance refresh failed"
    elif state == "balance-shortfall":
        snapshot["trading"]["available_usdc"] = 0.50
    elif state == "approval-shortfall":
        snapshot["trading"]["allowance_usdc"] = 0.50
    elif state == "minimum-too-high":
        snapshot["trading"]["available_usdc"] = 4.00
        snapshot["trading"]["allowance_usdc"] = 4.00
        snapshot["up"]["minimum_order_size"] = 8.0
        snapshot["down"]["minimum_order_size"] = 9.0
    elif state == "maker-cutoff":
        snapshot["current_session"]["end_ms"] = EXAMPLE_NOW_MS + 120_000
    elif state == "in-flight-order":
        snapshot["trading"]["enabled"] = True
        snapshot["trading"]["in_flight_intent"] = {"request_id": "example-in-flight"}
        snapshot["trading"]["order_status"] = "Submitting the manually confirmed order…"
    elif state == "order-error":
        snapshot["trading"]["order_status"] = "Error: example exchange rejection; no fill was recorded."
    elif state == "degraded":
        snapshot["connectivity"].update(
            {
                "kind": "degraded",
                "headline": "One live feed is reconnecting",
                "detail": "Price display continues, but trading waits for a fresh market order book.",
                "market_websocket_ok": False,
            }
        )
        snapshot["feeds"][2].update(
            {
                "connected": False,
                "reconnects": 2,
                "last_error": "Example reconnect",
                "status": "reconnecting",
            }
        )
    elif state in {"dns-filtering", "endpoint-restricted", "unreachable"}:
        messages = {
            "dns-filtering": (
                "DNS filtering is blocking a required endpoint",
                "Review the setup diagnostic before approving any operating-system DNS change.",
            ),
            "endpoint-restricted": (
                "A required Polymarket endpoint is restricted",
                "The service cannot safely enable trading on the current connection.",
            ),
            "unreachable": (
                "Polymarket connectivity is unavailable",
                "Required REST and WebSocket checks cannot be completed.",
            ),
        }
        headline, detail = messages[state]
        snapshot["connectivity"].update(
            {
                "kind": state.replace("-", "_"),
                "headline": headline,
                "detail": detail,
                "clob_rest_ok": False,
                "market_rest_ok": False,
                "market_websocket_ok": False,
                "setup_ready": False,
            }
        )
        for feed in snapshot["feeds"]:
            feed.update(
                {
                    "connected": False,
                    "last_error": f"Example {state} diagnostic",
                    "status": "offline",
                }
            )
    elif state == "stale-book":
        snapshot["orderbook"]["up"]["updated_at_ms"] = EXAMPLE_NOW_MS - 60_000
        snapshot["orderbook"]["down"]["updated_at_ms"] = EXAMPLE_NOW_MS - 60_000
    elif state == "empty-book":
        snapshot["orderbook"]["up"]["bids"] = []
        snapshot["orderbook"]["up"]["asks"] = []
        snapshot["orderbook"]["down"]["bids"] = []
        snapshot["orderbook"]["down"]["asks"] = []
    elif state == "closing":
        snapshot["current_session"]["end_ms"] = EXAMPLE_NOW_MS - 1_000
        snapshot["current_session"]["active"] = False
        snapshot["current_session"]["closed"] = True
    elif state == "missing-target":
        snapshot["current_session"]["price_to_beat"] = None
    elif state == "missing-chainlink":
        snapshot["chainlink_btc_usd"] = None
        snapshot["feeds"][1].update(
            {
                "connected": False,
                "last_error": "Example Chainlink reconnect",
                "status": "reconnecting",
            }
        )
    elif state == "chainlink-below-target":
        snapshot["chainlink_btc_usd"] = 104_236.82
    elif state == "chainlink-at-target":
        snapshot["chainlink_btc_usd"] = 104_250.00
    elif state == "portfolio-stale":
        snapshot["portfolio"]["updated_at_ms"] = EXAMPLE_NOW_MS - 180_000
    elif state == "negative-pnl":
        snapshot["portfolio"]["current_open_pnl_usd"] = -1.42
        snapshot["portfolio"]["today_realized_pnl_usd"] = -3.18
        snapshot["portfolio"]["current_value_usd"] = 12.31
    elif state == "claiming":
        first = snapshot["portfolio"]["claimable_positions"][0]
        snapshot["portfolio"]["claim_in_flight_condition_id"] = first["condition_id"]
        snapshot["portfolio"]["claim_status"] = "Submitting the manually confirmed claim…"
    elif state == "claim-error":
        snapshot["portfolio"]["last_error"] = "Example claim rejection; no transaction was submitted."
    elif state == "claim-success":
        snapshot["portfolio"]["claim_status"] = "Claim submitted successfully."
        snapshot["portfolio"]["claim_history"].insert(
            0,
            {
                "condition_id": "0xexample-recent-condition",
                "title": "BTC Up or Down — newly claimed example",
                "transaction_hash": "0x" + "34" * 32,
                "claimed_at_ms": EXAMPLE_NOW_MS - 12_000,
            },
        )
    elif state == "proxy-claims":
        snapshot["portfolio"] = _portfolio(direct_claim_supported=False)
    elif state == "empty-claims":
        snapshot["portfolio"]["claimable_positions"] = []
        snapshot["portfolio"]["claim_history"] = []
    elif state == "empty-history":
        snapshot["trading"]["ledger"] = []
        snapshot["past_sessions"] = []
        snapshot["portfolio"]["claim_history"] = []
    return snapshot


class DocumentationHandler(BaseHTTPRequestHandler):
    server_version = "PolyTreadDocumentationFixture/1.0"

    def _selected_state(self) -> str:
        referer = self.headers.get("Referer", "")
        query = parse_qs(urlparse(referer).query)
        return query.get("state", ["live"])[0]

    def _empty(self, status: int) -> None:
        self.send_response(status)
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Length", "0")
        self.end_headers()

    def _json(self, value: dict, status: int = 200) -> None:
        payload = json.dumps(value, separators=(",", ":")).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def _authenticate(self) -> None:
        state = self._selected_state()
        if state not in VALID_STATES:
            self._empty(404)
        elif state == "access-required":
            self._empty(401)
        else:
            self._empty(204)

    def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
        request = urlparse(self.path)
        if request.path in {"/", "/web/dashboard.html"}:
            html = DASHBOARD_PATH.read_text(encoding="utf-8")
            payload = html.replace("</head>", f"{_fixture_head()}\n</head>", 1).encode(
                "utf-8"
            )
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return
        if request.path == "/_auth/session":
            self._authenticate()
            return
        if request.path == "/events":
            state = self._selected_state()
            if state not in VALID_STATES:
                self._empty(404)
                return
            if state == "reconnecting":
                self._empty(503)
                return
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Connection", "close")
            self.end_headers()
            if state != "local":
                payload = json.dumps(snapshot_for(state), separators=(",", ":"))
                self.wfile.write(f"data: {payload}\n\n".encode("utf-8"))
            self.wfile.flush()
            try:
                for _ in range(120):
                    time.sleep(0.5)
                    self.wfile.write(b": documentation fixture keepalive\n\n")
                    self.wfile.flush()
            except (BrokenPipeError, ConnectionAbortedError, ConnectionResetError):
                pass
            return
        if request.path in {"/favicon.ico", "/command-ws"}:
            self._empty(204)
            return
        self._empty(404)

    def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
        request = urlparse(self.path)
        if request.path == "/_auth/session":
            self._authenticate()
            return
        if request.path == "/cmd":
            length = int(self.headers.get("Content-Length", "0"))
            if length:
                self.rfile.read(length)
            self._json({"ok": True, "documentation_fixture": True})
            return
        self._empty(404)

    def log_message(self, _format: str, *_args: object) -> None:
        return


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=8765)
    args = parser.parse_args()
    server = ThreadingHTTPServer(("127.0.0.1", args.port), DocumentationHandler)
    print(f"Synthetic dashboard available at http://127.0.0.1:{args.port}/web/dashboard.html")
    print("No credentials, live endpoints, or order submission are used.")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


if __name__ == "__main__":
    main()

# openEther service contract

Both the Rust core and the TypeScript services depend on this file. Change it first,
then the implementation.

Base URL for the core: `http://localhost:8080` (override with `CORE_URL`).

## REST

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/healthz` | `{ "status": "ok", "live_trading": false, "version": "0.1.0" }` |
| GET | `/metrics` | Prometheus text exposition (no trailing slash) |
| GET | `/api/config` | Effective runtime config (redacted) |
| GET | `/api/portfolio` | `PortfolioSnapshot` |
| GET | `/api/positions` | `Position[]` |
| GET | `/api/trades?limit=200` | `TradeRecord[]`, newest first |
| GET | `/api/logs?limit=200&source=agent` | `LogEntry[]`, newest first |
| GET | `/api/ticks?limit=200` | `MarketTick[]`, newest first |
| POST | `/api/logs` | Body `LogEntry` (source/level/message/context) — the AI agent pushes its reasoning here |
| GET | `/api/risk` | `RiskState` (see below) |
| GET | `/api/strategy` | `{ "name", "loaded_at_ms", "wasm_sha256", "ticks_processed", "fuel_used" }` |
| POST | `/api/strategy/reload` | Body `{ "path": "/abs/path/strategy.wasm" }` → reloads the sandboxed strategy |
| POST | `/api/orders` | Body `OrderRequest` → runs the risk layer, then submits |
| GET | `/api/orders` | `TrackedOrder[]`, newest first — order state and venue-confirmed fills |
| DELETE | `/api/orders/{order_id}` | Cancel a resting order; the venue must confirm |
| POST | `/api/breaker/reset` | Body `{ "confirm": true }` → manual breaker reset |
| GET | `/api/venues` | `[{ "venue": "paper", "healthy": true, "live": false }]` |
| GET | `/api/wallet` | `WalletMetadata` — public wallet metadata, never a secret |
| GET | `/api/wallet/holdings` | `WalletHoldings` — on-chain balances, valued but never traded (see below) |
| POST | `/api/wallet/sign-dry-run` | Body `{ "passphrase", "envelope", "account"? }` → `DryRunSignature` |
| GET | `/api/live` | Live-trading arming state (see below) |
| GET | `/api/risk/rules` | `RiskContract` — the limits, check order and reason codes the engine runs with (see below) |

### `POST /api/orders`

```json
{
  "venue": "paper",
  "symbol": "SOL/USDT",
  "side": "buy",
  "qty": 1.5,
  "order_type": "market",
  "limit_price": null,
  "reduce_only": false,
  "rationale": "momentum breakout on 5m, risk 0.4% of equity"
}
```

Response `200`:
```json
{ "approved": true, "order_id": "…", "status": "filled", "fill_price": 142.31, "fee": 0.02, "risk": { … } }
```

Response `403` (rejected by the risk layer — never overridable):
```json
{ "approved": false, "reason": "position_size_exceeds_max_pct", "detail": "…", "breaker_state": "closed" }
```

### `GET /api/orders` and `DELETE /api/orders/{order_id}`

An order is not a trade. `POST /api/orders` answers with the risk verdict and the
venue's initial acknowledgement; what actually filled is only known later, from
the venue's own trade history. `GET /api/orders` is where that truth is reported.

```json
[
  {
    "venue": "binance",
    "order_id": "1234567890",
    "client_id": "oe-…",
    "symbol": "SOL/USDT",
    "side": "buy",
    "order_type": "limit",
    "limit_price": 130.0,
    "qty": 2.0,
    "status": "partially_filled",
    "rationale": "momentum breakout on 5m",
    "submitted_ts_ms": 1700000000000,
    "updated_ts_ms": 1700000004000,
    "fills": [
      { "venue": "binance", "order_id": "1234567890", "symbol": "SOL/USDT",
        "side": "buy", "qty": 0.8, "price": 130.0, "fee": 0.0104,
        "ts_ms": 1700000003000 }
    ],
    "missing_since_ms": null
  }
]
```

Rules the engine holds itself to:

- **Only venue-reported fills are booked.** A fill is matched by `order_id` and
  deduped by `(ts_ms, qty, price)`, so reconciliation can re-read an overlapping
  window without double-counting. There is no synthetic-fill fallback: an empty
  answer means nothing filled.
- **`status` is an inference, `fills` is a fact.** A fill the venue confirms is
  booked even if the engine had already labelled the order cancelled or filled.
  Dropping it would under-report the exposure the risk layer measures.
- **Leaving the open list is not proof of cancellation.** Trade history lags the
  open list, so an absent order gets a 30 s grace window (`missing_since_ms`)
  before it is retired, and fills are still read during that window.
- **A venue that cannot report fills keeps its orders unreconciled** rather than
  invented. Hyperliquid refuses order placement outright, so this never arises
  there.
- Commission is converted honestly: quote asset → USD directly, base asset →
  `commission × price`, anything else (e.g. BNB) → `0` **with a warning**. It is
  never guessed.

`DELETE` cancels a resting order. The venue must confirm before the engine marks
it cancelled, so a failed cancel leaves the order live and still reconciling.
Cancelling only reduces exposure, so it does not pass through the risk layer; a
fill that landed just before the cancel is still booked.

Response `200`: `{ "ok": true }`

Response `400` (plain text): `unknown order`, or
`order … is filled: only a resting order can be cancelled`.

### `POST /api/wallet/sign-dry-run`

Signs an envelope locally. The transaction is **never** broadcast: the answer
always carries `"dry_run": true` and `"broadcast": false`.

```json
{
  "passphrase": "…",
  "envelope": {
    "chain": "evm",
    "nonce": 7,
    "timestamp_ms": 1700000000000,
    "to": "0x…",
    "amount": 1.5,
    "amount_raw": 1500000,
    "fee": 21000,
    "memo": ""
  },
  "account": { "chain": "solana", "index": 2 }
}
```

`account` is optional. Omitted means the primary account. `chain` is `"evm"` or
`"solana"`, `index` is the account index of an HD keystore. An account the
keystore does not hold is refused with `account_not_found`.

Response `200`:
```json
{
  "dry_run": true,
  "broadcast": false,
  "chain": "evm",
  "address": "0x…",
  "public_key": "0x…",
  "tx_hash": "0x…",
  "signature": "0x…",
  "envelope": { … },
  "account": { "chain": "evm", "index": 0, "path": "m/44'/60'/0'/0/0", "address": "0x…", "public_key": "0x…" },
  "signed_at_ms": 1700000000000,
  "note": "dry run only: …"
}
```

Error `4xx`:
```json
{ "ok": false, "error": "…", "code": "invalid_passphrase", "broadcast": false, "dry_run": true }
```

Codes: `wallet_not_configured`, `wallet_locked`, `wallet_lease_expired`,
`invalid_passphrase`, `keystore_decrypt_failed`, `live_broadcast_unsupported`,
`chain_mismatch`, `account_not_found`, `invalid_key`, `invalid_envelope`,
`signature_invalid`, `unsupported_keystore_format`, `keystore_io_error`.

### `GET /api/wallet/holdings`

The on-chain wallet, read from the chain and priced from the DEX feed. The
engine refreshes it every 30 seconds and never unlocks the keystore to do it:
only public addresses are used.

**These balances are not trading equity.** There is no broadcast path, so the
risk layer keeps measuring equity, drawdown and position caps on the trading
account alone. `PortfolioSnapshot.onchain_value_usd` reports the same total for
convenience and is likewise excluded from every risk check.

Response `200`:
```json
{
  "configured": true,
  "chains": [
    {
      "chain": "ethereum",
      "address": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
      "native_symbol": "ETH",
      "native_balance": 0.8083523704434918,
      "native_price_usd": 1931.68,
      "native_value_usd": 1561.47,
      "tokens": [
        {
          "chain": "ethereum",
          "asset": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
          "symbol": "USDC",
          "balance": 52363.119552,
          "decimals": 6,
          "price_usd": 1.0008,
          "value_usd": 52405.01,
          "priced": true
        }
      ],
      "tokens_skipped": 0,
      "value_usd": 53966.48,
      "fetched_at_ms": 1731200000000
    }
  ],
  "value_usd": 53966.48,
  "fetched_at_ms": 1731200000000
}
```

- `configured` is `false` when no keystore is set; every balance is then zero.
- `price_usd` is `null` and `priced` is `false` when no pool quoted the token.
  An unpriced token contributes nothing to `value_usd` — it is never guessed at.
- `tokens_skipped` counts holdings left unpriced on purpose: a busy address can
  hold thousands of dust tokens, and the read is bounded (300 tokens, 30 per
  feed request) so the free feed's rate limit survives.
- Only the first Solana and first EVM account of the keystore are read; each
  extra account is another full set of RPC calls.
- EVM tokens come from `OPENETHER_EVM_TOKEN_WATCHLIST` (comma-separated contract
  addresses). Solana tokens come from the address's own token accounts.
- `OPENETHER_PRICE_FEED_URL` overrides the feed (default
  `https://api.dexscreener.com`).

### `GET /api/live`

Reports whether the engine is allowed to place real orders, and why not when
it is not. Live trading needs **both** `OPENETHER_LIVE_TRADING=1` and a valid
arming record at `OPENETHER_LIVE_ARMING_FILE`. The record is written by the
`openether-live` CLI; there is no HTTP endpoint that can arm the engine.

Response `200`:
```json
{
  "live_trading": false,
  "venue": "paper",
  "arming": {
    "live_enabled": false,
    "reason": "live_not_requested",
    "detail": "live trading is off: OPENETHER_LIVE_TRADING is not set. The arming record is valid, but live trading was not requested.",
    "effective_max_order_notional_usd": 5000.0,
    "venue": "binance",
    "armed_at_ms": 1700000000000,
    "expires_at_ms": 1700014400000,
    "max_order_notional_usd": 500.0,
    "operator_note": "morning session"
  }
}
```

`reason` is `null` when live trading is on. Otherwise it is one of
`arming_file_unset`, `arming_file_missing`, `arming_file_unreadable`,
`arming_version_unsupported`, `arming_venue_mismatch`,
`arming_confirmation_invalid`, `arming_expired`, `arming_config_changed`,
`live_not_requested`.

`effective_max_order_notional_usd` is the per-order cap the risk layer
enforces: the smaller of the configured cap and the armed cap. While live
trading is on it is what an oversized order is measured against
(`notional_too_large`).

`GET /api/config` carries the same object under `live_arming`.

### `GET /api/risk/rules`

The canonical risk contract. The Rust risk layer owns it: the limits, the
order the checks run in and the reason codes are defined once, here. The
dashboard reads this endpoint and runs the same checks, so the browser
simulator refuses exactly what the engine refuses, for the same reason.

Response `200`:
```json
{
  "version": 1,
  "limits": {
    "max_position_pct_of_equity": 20.0,
    "max_gross_exposure_pct": 60.0,
    "max_daily_loss_pct": 3.0,
    "max_drawdown_pct": 10.0,
    "max_open_positions": 5,
    "max_order_notional_usd": 5000.0,
    "min_order_notional_usd": 10.0,
    "breaker_cooldown_ms": 300000,
    "hard_halt_on_oversized_order": true
  },
  "allowlists": {
    "symbols": ["BTC/USDT", "ETH/USDT", "SOL/USDT"],
    "venues": ["paper"],
    "contracts": []
  },
  "check_order": [
    "breaker_open",
    "invalid_quantity",
    "invalid_price",
    "invalid_equity",
    "invalid_symbol",
    "symbol_not_allowed",
    "venue_not_allowed",
    "contract_not_allowed",
    "notional_too_small",
    "notional_too_large",
    "reduce_only_violation",
    "position_size_exceeds_max_pct",
    "gross_exposure_exceeded",
    "max_open_positions",
    "half_open_probe_too_large"
  ],
  "venue_checks": ["no_position", "reduce_only", "insufficient_cash"],
  "reasons": [
    "breaker_open",
    "invalid_quantity",
    "invalid_price",
    "invalid_equity",
    "invalid_symbol",
    "symbol_not_allowed",
    "venue_not_allowed",
    "contract_not_allowed",
    "notional_too_small",
    "notional_too_large",
    "reduce_only_violation",
    "position_size_exceeds_max_pct",
    "gross_exposure_exceeded",
    "max_open_positions",
    "half_open_probe_too_large",
    "no_position",
    "reduce_only",
    "insufficient_cash",
    "approved"
  ]
}
```

`check_order` is the order the checks run in: the first one that fails is the
reason the order is refused. `venue_checks` are applied by the venue after the
risk layer has passed the order. `reasons` is every code an order can end up
with, `approved` included.

The same contract, with empty allowlists, is generated for the browser by
`cargo run -p openether-core --bin openether-rules` and written to
`ts/packages/ui/src/lib/risk-rules.json`. That file is generated, not
hand-written, and a Rust test fails the build when it drifts.

## Risk state

```json
{
  "breaker_state": "closed",
  "breaker_reason": null,
  "equity_usd": 10000.0,
  "peak_equity_usd": 10250.0,
  "drawdown_pct": 2.44,
  "daily_pnl_usd": -120.5,
  "open_positions": 2,
  "gross_exposure_usd": 2100.0,
  "limits": {
    "max_position_pct_of_equity": 20.0,
    "max_gross_exposure_pct": 60.0,
    "max_daily_loss_pct": 3.0,
    "max_drawdown_pct": 10.0,
    "max_open_positions": 5,
    "max_order_notional_usd": 5000.0
  }
}
```

`breaker_state` is one of `closed`, `open`, `half_open`.

## WebSocket `/ws`

The server pushes JSON frames; every frame has a `type` field.

```json
{ "type": "tick",      "data": { "venue": "paper", "symbol": "SOL/USDT", "price": 142.31, "size": 0.5, "ts_ms": 1731200000000 } }
{ "type": "trade",     "data": { "id": "…", "venue": "paper", "symbol": "SOL/USDT", "side": "buy", "qty": 1.5, "price": 142.31, "fee": 0.02, "realized_pnl": 0, "ts_ms": 1731200000000, "rationale": "…", "source": "agent" } }
{ "type": "log",       "data": { "id": "…", "ts_ms": 1731200000000, "source": "agent", "level": "info", "message": "…", "context": {} } }
{ "type": "portfolio", "data": { "venue": "paper", "equity_usd": 10000.0, … } }
{ "type": "risk",      "data": { "breaker_state": "closed", … } }
{ "type": "status",    "data": { "state": "running", "detail": "…", "ts_ms": 1731200000000 } }
```

Clients may send `{"type":"ping"}` and receive `{"type":"pong"}`.

## MCP (stdio)

The Rust MCP server (`openether-mcp-server`) speaks JSON-RPC 2.0 over stdio and is the
only sanctioned path for the AI layer to touch the engine.

Tools:

| Tool | Args | Result |
| --- | --- | --- |
| `execute_trade` | `{venue, symbol, side, qty, order_type?, limit_price?, rationale?}` | risk verdict + venue acknowledgement (not proof of a fill) |
| `get_portfolio` | `{}` | portfolio snapshot with positions and PnL |
| `get_orders` | `{}` | tracked orders with venue-confirmed fills; the Verify step reads this |
| `cancel_order` | `{order_id}` | cancel a resting order; a refusal leaves it live |
| `get_positions` | `{}` | open positions |
| `get_market_data` | `{symbol, venue?, lookback?}` | recent ticks + last price |
| `get_risk_state` | `{}` | breaker state and utilisation |
| `get_trade_history` | `{limit?}` | recent fills |
| `fetch_onchain_data` | `{chain, address, ...}` | wallet balances / token holdings |
| `set_strategy` | `{path}` | hot-swap the WASM strategy |
| `get_agent_logs` | `{limit?}` | engine-side logs |

Every tool result includes `approved: boolean` and `reason: string` when a risk verdict
is involved. A rejected trade is never retried by the agent without a new plan.

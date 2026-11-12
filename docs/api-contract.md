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
| POST | `/api/breaker/reset` | Body `{ "confirm": true }` → manual breaker reset |
| GET | `/api/venues` | `[{ "venue": "paper", "healthy": true, "live": false }]` |
| GET | `/api/wallet` | `WalletMetadata` — public wallet metadata, never a secret |
| POST | `/api/wallet/sign-dry-run` | Body `{ "passphrase", "envelope", "account"? }` → `DryRunSignature` |
| GET | `/api/live` | Live-trading arming state (see below) |

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
| `execute_trade` | `{venue, symbol, side, qty, order_type?, limit_price?, rationale?}` | risk verdict + fill |
| `get_portfolio` | `{}` | portfolio snapshot with positions and PnL |
| `get_positions` | `{}` | open positions |
| `get_market_data` | `{symbol, venue?, lookback?}` | recent ticks + last price |
| `get_risk_state` | `{}` | breaker state and utilisation |
| `get_trade_history` | `{limit?}` | recent fills |
| `fetch_onchain_data` | `{chain, address, ...}` | wallet balances / token holdings |
| `set_strategy` | `{path}` | hot-swap the WASM strategy |
| `get_agent_logs` | `{limit?}` | engine-side logs |

Every tool result includes `approved: boolean` and `reason: string` when a risk verdict
is involved. A rejected trade is never retried by the agent without a new plan.

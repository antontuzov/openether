# openEther

An autonomous crypto trading agent with a **non-bypassable risk layer**.

A Rust core owns every order. A TypeScript agent reasons, plans and calls tools —
but it cannot place an order that the Rust risk layer refuses. The dashboard
runs the same rules in the browser so the public demo is tradeable without a
backend.

> **Live demo:** <https://antontuzov.github.io/openether>
> The hosted site is a **simulation**. It prices 50 markets from the free
> CoinGecko feed and fills locally in your browser. No exchange account, no
> keys, no real orders.

---

## Safety first

Read this before running anything.

- **Paper by default.** `OPENETHER_LIVE_TRADING=0` and `OPENETHER_VENUE=paper`.
  Orders go to a deterministic simulated venue.
- **Live trading needs a ceremony, not a flag.** The environment variable only
  asks for live trading. The core also needs a valid, expiring arming record
  that the operator wrote from the CLI. See
  [Live trading](#live-trading).
- **No broadcast path exists.** The wallet signs and logs; it never sends a
  transaction. `dry_run` is `true` and `broadcast` is `false` in every response,
  and the API refuses to sign at all if dry run is switched off.
- **The risk layer is absolute.** The agent has no tool, flag or prompt that
  reaches around it. See [Risk layer](#risk-layer).
- **Keys are never hardcoded.** The keystore is AES-256-GCM with an
  Argon2id-derived key. The passphrase is supplied per request, held only for
  the signing call, and zeroized afterwards.
- **The admin gate is not authentication.** The published dashboard is a static
  site. Its password gate keeps casual visitors out of the analytics view; it
  is *not* a substitute for keeping a real engine on a private network.

This software trades nothing by default and is provided for research and
education. It is **not financial advice**. Crypto trading can lose you all of
your capital. You are responsible for any credentials you configure and any
orders you enable. See [Disclaimer](#disclaimer).

---

## Architecture

```mermaid
flowchart TB
  subgraph cortex["TypeScript cortex"]
    AG["Agent orchestrator<br/>plan → execute → verify"]
    LLM["LLM providers<br/>local / OpenAI / Anthropic"]
    GUARD["Injection guard<br/>+ argument validation"]
    MEM["Vector memory"]
    AG --- LLM
    AG --- GUARD
    AG --- MEM
  end

  subgraph mcp["MCP boundary (stdio)"]
    MC["Rust MCP server<br/>execute_trade, get_portfolio,<br/>fetch_onchain_data, …"]
    MD["Data MCP server<br/>CoinGecko, DexScreener,<br/>EVM + Solana RPC"]
  end

  subgraph core["Rust core — the only path to an order"]
    BUS["Event bus"]
    RISK["Risk layer<br/>non-bypassable"]
    WASM["WASM strategy sandbox<br/>wasmtime, hot-swap"]
    WAL["Encrypted keystore<br/>dry-run signing"]
    EX["Connectors<br/>paper / Binance / Kraken / Hyperliquid"]
    BUS --> RISK --> EX
    WASM --> BUS
    RISK --- WAL
  end

  UI["React dashboard<br/>live core or browser simulation"]

  AG -->|"tools"| MC
  AG -->|"tools"| MD
  MC -->|"validated calls"| RISK
  MD -->|"sanitised data"| GUARD
  core -->|"REST + WebSocket"| UI
  core -->|"Prometheus"| PROM["Prometheus + Grafana"]
  cortex -->|"Prometheus"| PROM
```

The order path is deliberately one-way:

```mermaid
sequenceDiagram
  participant A as Agent (LLM)
  participant G as Guard
  participant M as Rust MCP
  participant R as Risk layer
  participant V as Venue

  A->>G: tool call + arguments
  G->>G: validate qty / side / symbol
  G->>M: execute_trade
  M->>R: order request
  R->>R: allowlist, caps, breaker
  alt refused
    R-->>A: rejected + reason (audit logged)
  else approved
    R->>V: place order
    V-->>R: fill
    R-->>A: fill + updated risk state
  end
```

### Layout

```
rust/
  crates/core/            engine, bus, risk, wallet, connectors, HTTP/WS API
  crates/mcp-server/      MCP stdio server exposing core functions as tools
  crates/strategies/      momentum strategy, compiled to wasm32
ts/
  packages/agent/         orchestrator, LLM providers, guard, memory, metrics
  packages/mcp-data/      market + on-chain MCP server (9 tools)
  packages/ui/            React dashboard + browser paper-trading engine
deploy/                   Prometheus config, Grafana dashboard
scripts/                  native run scripts (no Docker)
docs/api-contract.md      wire contract shared by the core and the dashboard
```

---

## Quick start

No Docker. You need **Rust 1.85+**, **Node 20.19+** and **pnpm 12**.

```bash
git clone https://github.com/antontuzov/openether.git
cd openether
cp .env.example .env
pnpm --dir ts install
```

### 1. Just the dashboard

```bash
./scripts/run-ui.sh          # http://127.0.0.1:5173
```

With no core running, the dashboard falls back to its **browser simulation**:
50 live markets, a trade ticket, the full risk layer, portfolio, positions and
the admin analytics — all client-side, persisted to `localStorage`.

### 2. The real engine

```bash
./scripts/run-core.sh        # paper venue on 127.0.0.1:8080
./scripts/run-ui.sh          # dashboard now talks to the core
```

Or both at once:

```bash
./scripts/dev.sh
```

### 3. The agent

```bash
./scripts/run-agent.sh
```

`LLM_PROVIDER=local` is the default: a deterministic offline planner that needs
no API key and still exercises the whole tool path and the risk layer. Set
`LLM_PROVIDER=openai` or `anthropic` with the matching key for a real model.

### 4. The WASM strategy

```bash
./scripts/build-strategy.sh
```

That writes `rust/target/wasm/momentum.wasm`, which is the path `.env.example`
already points `OPENETHER_STRATEGY_WASM` at. Strategies are sandboxed with fuel
and memory limits and can be hot-swapped at runtime via
`POST /api/strategy/reload` — no core rebuild.

### Tests

```bash
cargo test --workspace --manifest-path rust/Cargo.toml   # 116 tests
pnpm --dir ts -r test                                    # 92 tests
pnpm --dir ts -r typecheck
pnpm --dir ts -r build
```

---

## Risk layer

Every order passes the same checks, in the Rust core and in the browser
simulation. The agent cannot skip them.

| Limit | Default | Rejection reason |
| --- | --- | --- |
| Single order notional | $5,000 | `above_max_notional` |
| Minimum order notional | $10 | `below_min_notional` |
| Position size | 20% of equity | `position_cap` |
| Gross exposure | 60% of equity | `gross_exposure_cap` |
| Open positions | 5 | `max_open_positions` |
| Free cash | margin-aware | `insufficient_cash` |
| Daily loss | 3% | trips the breaker |
| Drawdown from peak | 10% | trips the breaker |
| Symbol / venue / contract | allowlist | `unknown_symbol` |
| Sells | reduce-only | `no_position`, `reduce_only` |

**The circuit breaker** opens on a daily-loss or drawdown breach and refuses
every subsequent order (`breaker_open`) until it is explicitly reset. A reset
always requires `{"confirm": true}` on `POST /api/breaker/reset` — there is no
configuration switch for it, so the agent cannot reset the breaker on its own.
Half-open states allow only a probe order. Every decision is written to an audit
ring buffer exposed at `GET /api/risk/decisions`.

### Paper accounting

The paper venue is a USD-margined account, so `equity = cash + unrealized PnL`
holds literally: opening a position **locks margin** (`|qty| × avg_entry`)
rather than spending cash. Only fees and realised PnL move the wallet balance.
The browser engine mirrors this exactly — buying $1,000 of SOL costs you the
fee, not $1,000 of equity.

### Durable state

The core keeps its risk-relevant state in a SQLite database so a restart is not
a way around the risk layer. Set `OPENETHER_STATE_DB` to a path and the engine
writes there:

```bash
OPENETHER_STATE_DB=./data/openether.db
```

It stores the portfolio (equity, cash, realised P&L), the open positions, the
full trade journal, the open orders, the client-order-id → venue-order-id
mapping, and the circuit-breaker state.

Three details matter:

- **A breaker that was open stays open.** Restarting does not close it.
- **The risk layer restores its own position map**, not just the engine's. The
  gross-exposure cap is measured from that map, so without it a restarted
  process would believe it holds nothing and open a fresh full-size position.
- **The venue gets its account back.** A live exchange re-reports its balances
  on every poll, but the simulator has no server to ask — so the engine hands
  it the persisted cash, realised P&L and positions. Without that the engine
  would believe it held a position while the simulator believed it held cash,
  and the two would disagree about every margin calculation.

The state is written on every fill, so a crash loses at most the trade in
flight.

Leaving the variable unset (or set to `memory`) keeps everything in RAM. That is
fine for a throwaway demo and **wrong for a live run**: equity would reset to
zero and every limit would re-open.

---

## Live trading

Live trading is not a flag. `OPENETHER_LIVE_TRADING=1` only says *"I would like
to trade live"* — on its own it arms nothing and the core stays on paper.

Two things are needed, and the core refuses to trade unless **both** are true:

1. `OPENETHER_LIVE_TRADING=1` — the operator asks for live trading.
2. A valid **arming record** at `OPENETHER_LIVE_ARMING_FILE` — the operator
   proves they meant it.

The record is written by the `openether-live` CLI, never over HTTP. Run this
from the repository root and use an absolute path for the record, so the CLI
and the core agree on where it is:

```bash
cargo build --release --manifest-path rust/Cargo.toml

export OPENETHER_LIVE_ARMING_FILE="$PWD/data/live-arming.json"

# See what the core would do right now.
./rust/target/release/openether-live status

# Arm it. The CLI prints the limits it is about to run under and then demands
# the confirmation phrase on stdin.
printf 'I UNDERSTAND THIS SENDS REAL ORDERS\n' | \
  ./rust/target/release/openether-live arm \
    --venue binance --max-notional 500 --hours 4 --note "morning session"

# Run the core with OPENETHER_LIVE_TRADING=1 and that arming file.
OPENETHER_LIVE_TRADING=1 ./scripts/run-core.sh

# Turn it off again. This is the only way to disarm.
./rust/target/release/openether-live disarm
```

### What the record binds

The arming is a record, not a permission slip you can copy around:

| Property | Rule |
| --- | --- |
| Venue | The record names one venue. The core must run on that venue. |
| Expiry | It expires (default 4 hours, maximum 30 days). |
| Config binding | It carries a SHA-256 digest of the risk configuration it was written against. Change a limit and the arming is stale. |
| Direction | It may only *tighten* the per-order cap. Raising it above the risk layer's cap is refused. |
| Confirmation | The phrase is typed by the operator; the CLI refuses without it. |
| Write path | Only the CLI writes it. There is no HTTP endpoint that can arm the engine. |

The digest covers the limits that decide how much money can be lost: position
cap, gross exposure, daily loss, drawdown, open positions, minimum notional,
breaker cooldown, the hard-halt flag and the symbol / venue / contract
allowlists. The per-order cap is not in the digest — the arming tightens that
one itself, and the effective cap is always the *smaller* of the risk layer's
cap and the armed cap, so it can only ever tighten.

So the full sequence to trade live is: set the risk configuration first, then
arm, then start the core. Re-order those and the arming is stale on purpose.

### When the core refuses

`GET /api/live` reports the decision and the reason:

| Reason | Meaning |
| --- | --- |
| `arming_file_unset` | `OPENETHER_LIVE_ARMING_FILE` is not set. |
| `arming_file_missing` | The path is set but there is no record. |
| `arming_file_unreadable` | The record cannot be read or parsed. |
| `arming_version_unsupported` | The record was written by a different build. |
| `arming_venue_mismatch` | The record is for another venue. |
| `arming_confirmation_invalid` | The record was never confirmed. |
| `arming_expired` | The record ran out. |
| `arming_config_changed` | A risk limit changed after the arming was written. |
| `live_not_requested` | The record is valid but `OPENETHER_LIVE_TRADING` is not set. |

While live trading is armed, the armed cap is what the risk layer enforces — it
is not an advisory number. An order above it is refused with
`notional_too_large` before it reaches the exchange.

### What live trading covers

- **Binance and Kraken place real orders.** The connectors sign the request
  and broadcast it; the risk layer has already approved it by then.
- **Hyperliquid is read-only.** It has no signer in this build, so
  `place_order` returns `Unsupported`.
- **The paper venue never trades live**, and `openether-live arm` refuses to
  arm it.
- **On-chain transactions are still dry-run only.** The wallet signs and logs;
  there is no broadcast path.

---

## Wallet

The wallet is **non-custodial and offline**: openEther holds an encrypted
keystore on your disk, derives addresses from it and signs locally. There is no
server in the middle, no key escrow and no keystore upload.

### One keystore, many accounts

A keystore is a single encrypted file that holds one of two things:

| Type | Holds | Accounts |
| --- | --- | --- |
| `single` | one raw private key | one |
| `hd` | a BIP-39 mnemonic | one per index per chain |

An HD wallet derives **EVM** accounts over BIP-44 (`m/44'/60'/0'/0/i`) and
**Solana** accounts over SLIP-0010 (`m/44'/501'/i'/0'`), so one recovery phrase
covers both chains. The derivation is implemented in
[rust/crates/core/src/hd.rs](rust/crates/core/src/hd.rs) against the published
test vectors.

### Creating a keystore

```bash
cd rust
export OPENETHER_WALLET_PASSPHRASE='a long passphrase'

# An HD wallet: 5 accounts per chain, recovery phrase printed once.
cargo run --bin openether-wallet -- create-hd --path ../wallet.json --chain evm --accounts 5

# Restore an existing recovery phrase.
cargo run --bin openether-wallet -- import-hd --path ../wallet.json --chain evm \
  --mnemonic "word one word two …"

# One raw key (32 bytes of hex).
cargo run --bin openether-wallet -- create --path ../wallet.json --chain solana \
  --secret <64 hex characters>

# Public metadata, without unlocking anything.
cargo run --bin openether-wallet -- show --path ../wallet.json
```

The passphrase is read from `OPENETHER_WALLET_PASSPHRASE`, never from the
command line, so it stays out of the shell history and the process list.

Then point the core at the file:

```bash
OPENETHER_WALLET_KEYSTORE=./wallet.json
OPENETHER_WALLET_CHAIN=evm
```

### How the file is protected

- **Argon2id** (19 MiB, 2 passes) turns the passphrase into a 256-bit key.
  The parameters are stored in the file, so a keystore written by an older
  build still opens.
- **AES-256-GCM** encrypts the payload. The associated data is a canonical
  serialisation of every public header field, plus the wallet type and the
  account list — so neither the wallet type nor the advertised addresses can
  be edited after the fact. Tampering breaks decryption.
- An **HMAC key-check tag** tells a wrong passphrase apart from a corrupted
  file, in constant time.
- The file is written with mode `0600` and holds **no secret in clear text**:
  for an HD wallet the mnemonic is inside the ciphertext, so the words you
  wrote down are the real backup.
- The unlocked key lives in a `zeroize` wrapper, is bounded by an **unlock
  lease** (`OPENETHER_WALLET_UNLOCK_TIMEOUT_MS`) and is wiped when the lease
  expires — even if nothing asks for it again.
- Version 1 keystores (one raw key) still open unchanged: their associated
  data is kept byte-for-byte identical.

### Choosing an account

`POST /api/wallet/sign-dry-run` takes an optional `account` object:

```json
{ "passphrase": "…", "envelope": { … }, "account": { "chain": "solana", "index": 2 } }
```

Omitting it signs with the primary account. Asking for an account the keystore
does not hold is refused — a single-key keystore answers `account_not_found`
rather than quietly signing with the wrong key.

### What it still cannot do

The wallet **signs and logs, it does not broadcast**. Every response carries
`"dry_run": true` and `"broadcast": false`, and `wallet.rs` contains no HTTP
client, no RPC client and no socket. On-chain transactions need a signer the
operator controls.

---

## Dashboard

- **Two themes.** Every colour is a CSS custom property; the light theme
  overrides the same token names. Both themes pass WCAG AA contrast for text.
- **Core / simulator tabs.** The dashboard auto-selects the simulator when no
  core is reachable, so the published site is never a dead page.
- **Trade ticket** with live notional, cap preview and the exact rejection
  reason from the risk layer.
- **Markets**: top 50 coins by market cap from the free CoinGecko endpoint,
  filterable and sortable, each with a trade button.
- **Admin analytics** (password-gated): net/gross P&L, fees, win rate, profit
  factor, equity curve, P&L by bucket, and a pivot table across symbol, venue,
  side, day, hour and source.

### Enabling the admin panel

The gate compares a SHA-256 digest computed in the browser against a digest
baked in at build time. **The password itself is never shipped.** With no
digest configured the panel stays permanently locked.

```bash
printf '%s' 'your-password' | shasum -a 256
```

Put the digest in `.env` as `VITE_ADMIN_PASSWORD_HASH` and rebuild. For the
hosted site, set it as the `ADMIN_PASSWORD_HASH` repository secret; the Pages
workflow passes it into the build.

This is a gate, not access control. Anyone can read the bundle. Never put
anything sensitive behind it.

---

## MCP tools

**Rust core** (`rust/crates/mcp-server`): `execute_trade`, `get_portfolio`,
`get_positions`, `get_trade_history`, `get_risk_state`, `get_market_data`,
`get_agent_logs`, `fetch_onchain_data`.

**Market + on-chain** (`ts/packages/mcp-data`): `list_top_coins`,
`get_coin_price`, `get_coin_details`, `search_dex_pairs`, `get_dex_liquidity`,
`get_token_metadata`, `get_evm_balances`, `get_evm_tx_history`,
`get_solana_balances`.

All of these run on free, keyless endpoints by default (CoinGecko, DexScreener,
public EVM and Solana RPC). Etherscan is optional. Responses are cached,
rate-limited and sanitised before they reach the model.

### Prompt-injection defence

Tool output is untrusted data. Before it reaches a prompt it is stripped of
ANSI and control characters, screened for injection patterns (instruction
overrides, persona replacement, role tags, approval bypass), truncated, and any
contract address outside the allowlist is redacted. Separately, the arguments
the model wants to send are validated before the call runs — and the Rust core
validates them again, because the TypeScript layer is not trusted either.

---

## Observability

Both layers export Prometheus metrics: the core on `:8080/metrics`, the agent
on `:9090/metrics`.

```bash
prometheus --config.file=deploy/prometheus.yml
```

Import `deploy/grafana/openether-dashboard.json` into Grafana. It covers equity,
PnL, event-bus dispatch latency (p50/p99 against the <10µs target), the order
funnel, drawdown, breaker state, risk rejections, LLM latency and strategy
ticks/errors.

---

## Hosting

The dashboard is a static bundle, so GitHub Pages hosts it for free at
`https://<user>.github.io/<repo>/` — no domain purchase, no DNS, no expiry.

This repo is published at **https://antontuzov.github.io/openether/**.

A published page has no Rust core behind it. The dashboard detects that on
boot and switches itself to the **simulator**, so a visitor gets a working
paper-trading terminal rather than an empty screen: 50 markets priced from the
free CoinGecko feed, the same risk caps, and fills persisted to `localStorage`.
Nothing there is real money and no keys are involved.

`.github/workflows/pages.yml` builds and deploys on every push to `main`.
Vite uses `base: '/openether/'` for production builds and `/` in dev; override
with `VITE_BASE_PATH` for a custom domain.

To publish your own fork:

1. Create a **public** repository (Pages requires it on the free plan).
2. Push to `main`.
3. Settings → Pages → Source: **GitHub Actions**.
4. Optionally add the `ADMIN_PASSWORD_HASH` secret.

---

## Configuration

Copy `.env.example` to `.env`. The important defaults:

| Variable | Default | Meaning |
| --- | --- | --- |
| `OPENETHER_LIVE_TRADING` | `0` | asks for real orders; needs an arming record too |
| `OPENETHER_LIVE_ARMING_FILE` | *(empty)* | path to the arming record written by `openether-live arm` |
| `OPENETHER_VENUE` | `paper` | `paper` / `binance` / `kraken` / `hyperliquid` (read-only) |
| `OPENETHER_WALLET_DRY_RUN` | `1` | sign and log, never broadcast |
| `OPENETHER_WALLET_ACCOUNTS` | `5` | accounts per chain in an HD keystore |
| `OPENETHER_SYMBOL_ALLOWLIST` | `SOL/USDT,BTC/USDT,ETH/USDT` | tradable symbols |
| `OPENETHER_CONTRACT_ALLOWLIST` | *(empty)* | permitted on-chain contracts |
| `LLM_PROVIDER` | `local` | `local` / `openai` / `anthropic` / `openai-compatible` |
| `VITE_ADMIN_PASSWORD_HASH` | *(empty)* | build-time admin gate digest |

Exchange credentials are only read when live trading is armed. `.env`,
`*.keystore` and `*.key` are gitignored.

### HTTP API

`GET /healthz`, `/metrics`, `/api/config`, `/api/portfolio`, `/api/positions`,
`/api/trades`, `/api/logs`, `/api/ticks`, `/api/risk`, `/api/risk/decisions`,
`/api/strategy`, `/api/venues`, `/api/wallet`, `/api/live`;
`POST /api/orders`, `/api/logs`, `/api/onchain`, `/api/strategy/reload`,
`/api/breaker/reset`, `/api/wallet/sign-dry-run`;
`GET /ws` for the live stream. Full shapes in [docs/api-contract.md](docs/api-contract.md).

---

## What is verified

Checked locally on macOS (arm64):

- `cargo test --workspace` — **130 passing**, including the encrypted keystore,
  EVM and Solana dry-run signing, signature tampering, wrong-passphrase
  rejection, and the API refusing to sign when dry run is off.
- The live-trading arming ceremony exercised end to end against the real
  binaries: the env var alone arms nothing; arming without the confirmation
  phrase is refused; arming writes a record that flips `GET /api/live` to
  `live_enabled: true`; widening a risk limit afterwards turns it back into
  `arming_config_changed`; lowering the per-order cap tightens the effective
  cap; an expired record and a record for another venue are both refused; and
  `disarm` removes it. With the core running against an armed record, an order
  above the armed cap was refused with `notional_too_large` against the $500
  armed cap, while a smaller one passed the risk layer and only stopped at the
  missing exchange credentials.
- The HD wallet cross-validated against an independent Python implementation
  of BIP-39, BIP-32/BIP-44 and SLIP-0010: all six derived addresses (3 EVM,
  3 Solana) matched exactly, so the recovery phrase really does restore the
  same accounts.
- `cargo fmt --check` and `cargo clippy --workspace --all-targets -- -D warnings`
  — clean, so the CI gate passes as written.
- `pnpm -r test` — **92 passing** across the agent guard, the data server and
  the browser trading engine.
- `pnpm -r typecheck` and `pnpm -r build` — clean.
- Dashboard exercised in a real browser: an oversized BTC order rejected with
  the notional cap reason, a within-limits SOL order filled, the position cap
  enforced on adding, a closing sell settling exactly two fees, the breaker
  blocking every order once open, both themes, and the admin gate rejecting a
  wrong password and unlocking on the right one.
- Contrast audited programmatically across every text node in both themes,
  including the breaker-open alarm state — no WCAG AA failures.
- The core run locally in paper mode with the WASM strategy loaded: an
  oversized BTC order refused with `notional_too_large`, DOGE refused with
  `symbol_not_allowed`, a 2 SOL round trip charging 5 bps per side and booking
  the realized PnL, and equity reconciling to the cent
  (`10000 − fees + realized`).
- The **published** site exercised in a real browser with no core running: it
  falls back to the simulator on its own, prices 50 markets from the free
  CoinGecko feed, refuses a 0.05 BTC order with the 20% position cap, fills
  0.02 BTC while locking margin and charging exactly one fee, and persists to
  `localStorage`. The admin gate rejects a wrong password and unlocks on the
  right one, revealing the pivot table and charts.
- Contrast re-audited on the deployed bundle in both themes — 855 visible text
  nodes each, zero WCAG AA failures.
- The core run against a SQLite state file, stopped and restarted: the
  position, the fee-charged equity, the locked margin, the trade journal, the
  client-order-id mapping and the risk anchors all came back. A second run
  against the same file refused an order that the position cap had to refuse.

**Not verified:** live exchange connections (no credentials were used), real
on-chain transactions (no broadcast path exists), and the <10µs dispatch target
on your hardware. Grafana and Prometheus configs are provided but were not run
against a live scrape here.

Bugs found and fixed during that verification:

- The browser ticket read its result out of a deferred React state updater, so
  every order reported "not executed" even when it filled.
- The browser engine spent cash instead of locking margin, which tripped the
  breaker on the first ordinary buy. Now covered by the accounting tests.
- Vite's `envDir` pointed at `ts/` rather than the repo root, so the admin
  password hash was never loaded.
- The injection guard filtered only the first occurrence of a pattern. Now
  covered by the guard tests.
- Nothing read `.env`, despite these instructions. `scripts/load-env.sh` now
  loads it for all three run scripts, without executing values that contain
  spaces.
- The WASM memory cap defaulted to 16 pages, but a Rust `wasm32` guest declares
  a 17-page minimum, so the shipped strategy never loaded. The default is now
  32 pages, with a regression test.
- The paper connector never implemented `fills_since`, so the engine fell back
  to a synthetic fill: every paper trade reported a **zero fee** and no
  slippage, disagreeing with the venue's own account. Now covered by a test.
- The engine's trade records hardcoded `realized_pnl: 0.0` even though the
  value was computed, which would have broken win-rate and profit-factor
  analytics.
- The price walk added a *dollar* gap to a *fractional* return, so mean
  reversion multiplied the price instead of nudging it — a 94% jump in one
  tick. Now covered by a test.
- `.env.example` documented `OPENETHER_PAPER_START_EQUITY` (the code reads
  `OPENETHER_PAPER_CASH`), a `HYPERLIQUID_PRIVATE_KEY` nothing reads (that
  connector is read-only), and an `OPENETHER_BREAKER_RESET_CONFIRM` switch that
  does not exist — reset confirmation is unconditional and cannot be disabled.
- `CORE_MCP_ARGS` used a manifest path relative to the repo root, but pnpm runs
  the agent from `ts/packages/agent`, so the MCP server could not start.
- The admin equity curve needs two points to draw, but with exactly one fill it
  said "no fills to plot yet" beside a badge reading "1 fills". Both now
  describe what is on screen.
- The core kept nothing on disk, so a restart reset equity to zero and re-opened
  every risk limit. It now persists to SQLite, and a breaker that was open
  stays open across a restart.
- The simulator kept its account in memory, so restoring the engine's copy alone
  left the engine holding a position the simulator knew nothing about — two
  accounts disagreeing about every margin calculation. The venue is now handed
  its account back on boot.
- The arming record's config digest covered the per-order cap, but the arming
  tightens that cap — so the digest could never match the record it was written
  with, and every armed core refused its own arming as stale. The cap is now
  bound by taking the smaller of the two caps instead, and it is excluded from
  the digest.

---

## Disclaimer

This project is for research and education. It is not financial advice, not a
recommendation to trade, and not a production trading system. Automated trading
can lose money rapidly and completely. Markets are volatile, APIs fail, models
hallucinate, and risk limits reduce but never eliminate loss.

The authors accept no liability for any financial loss, lost keys, or damage
arising from use of this software. If you enable live trading you do so entirely
at your own risk. Never commit private keys. Never fund a wallet you cannot
afford to lose. Start in paper mode and stay there until you understand every
order the agent produces.

## License

MIT — see [LICENSE](LICENSE).
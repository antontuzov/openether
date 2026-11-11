# @openether/mcp-data

A read-only Model Context Protocol server that gives the openEther trading agent
market data and on-chain state. It speaks MCP over stdio, so the agent spawns it
exactly like it spawns the Rust core server:

```bash
DATA_MCP_COMMAND=pnpm
DATA_MCP_ARGS=--filter @openether/mcp-data start
```

Every tool result is a compact JSON object. Errors are returned as
`{"ok": false, "error": "<short sanitised message>"}` instead of throwing, so the
model always receives something it can reason about.

## Upstreams

| Source | Used for | Auth |
| --- | --- | --- |
| CoinGecko | market caps, prices, coin metadata | none |
| DexScreener | DEX pairs, liquidity, volume | none |
| Solana JSON-RPC | SOL and SPL balances, mint decimals | none (own node recommended) |
| EVM JSON-RPC | native and ERC-20 balances, token metadata | none (own node recommended) |
| Etherscan V2 | EVM transaction history | `ETHERSCAN_API_KEY` |

## Tools

### `list_top_coins`

Top coins ranked by market cap.

| Argument | Type | Default | Notes |
| --- | --- | --- | --- |
| `limit` | number | `50` | 1-250 |
| `vs_currency` | enum | `usd` | usd, eur, gbp, jpy, aud, cad, chf, cny, inr, krw |
| `page` | number | `1` | 1-10 |

Result: `{ok, vs_currency, page, count, coins: [{rank, id, symbol, name, price,
market_cap, volume_24h, high_24h, low_24h, change_24h_pct, last_updated}]}`.

### `get_coin_price`

Price of one or more coins.

| Argument | Type | Default | Notes |
| --- | --- | --- | --- |
| `ids` | string[] | - | CoinGecko ids, e.g. `["bitcoin"]`, max 50 |
| `symbols` | string[] | - | Tickers, e.g. `["BTC"]`, max 50 |
| `vs_currency` | enum | `usd` | as above |

At least one id or symbol is required. Result: `{ok, vs_currency, count, coins:
[{id, symbol, price, market_cap, volume_24h, change_24h_pct}]}`.

### `get_coin_details`

Full metadata for one coin.

| Argument | Type | Notes |
| --- | --- | --- |
| `id` | string | CoinGecko id, `^[a-z0-9-]{1,64}$` |

Result: `{ok, coin: {id, symbol, name, categories, description, homepage, rank,
price, market_cap, volume_24h, high_24h, low_24h, change_24h_pct,
circulating_supply, total_supply, max_supply, ath, atl, genesis_date,
last_updated}}`.

### `search_dex_pairs`

DEX pair search across the supported chains.

| Argument | Type | Default | Notes |
| --- | --- | --- | --- |
| `query` | string | - | `^[A-Za-z0-9 ._/-]{1,64}$` |
| `limit` | number | `20` | 1-50, sorted by liquidity |

Result: `{ok, query, count, pairs: [{chain, dex, pair_address, url, base, quote,
price_usd, liquidity_usd, volume_24h, price_change_1h_pct, price_change_6h_pct,
price_change_24h_pct, txns_24h, market_cap, fdv, pair_created_at}]}`.

### `get_dex_liquidity`

Liquidity detail for one pair or one token.

| Argument | Type | Notes |
| --- | --- | --- |
| `chain` | enum | solana, ethereum, bsc, polygon, arbitrum, optimism, base, avalanche |
| `pair_address` | string | optional, chain specific format |
| `token_address` | string | optional, chain specific format |

Either `pair_address` or `token_address` is required. Result: `{ok, chain, pair}`
for a pair, `{ok, chain, count, pairs}` for a token.

### `get_solana_balances`

SOL balance and SPL holdings of a Solana wallet.

| Argument | Type | Default | Notes |
| --- | --- | --- | --- |
| `address` | string | - | base58, 32-44 characters |
| `include_tokens` | boolean | `true` | also read the SPL token accounts |
| `token_limit` | number | `10` | 0-25, largest nominal balance first |

Result: `{ok, address, slot, lamports, sol, token_count, tokens: [{mint,
amount_raw, decimals, ui_amount, symbol, name, price_usd, liquidity_usd}]}`.

### `get_evm_balances`

Native and ERC-20 balances of an EVM wallet.

| Argument | Type | Default | Notes |
| --- | --- | --- | --- |
| `address` | string | - | `^0x[a-fA-F0-9]{40}$` |
| `chain` | enum | `ethereum` | EVM chains only |
| `tokens` | string[] | `[]` | ERC-20 contracts, max 10 |

Result: `{ok, chain, address, native: {balance_wei, balance_native}, token_count,
tokens: [{address, balance_raw, balance, decimals, symbol, name}]}`.

### `get_evm_tx_history`

Recent transactions from Etherscan.

| Argument | Type | Default | Notes |
| --- | --- | --- | --- |
| `address` | string | - | `^0x[a-fA-F0-9]{40}$` |
| `chain` | enum | `ethereum` | EVM chains only |
| `limit` | number | `10` | 1-50 |

Requires `ETHERSCAN_API_KEY`; without it the tool returns
`{"ok": false, "error": "etherscan api key is not configured ..."}`.

Result: `{ok, chain, address, count, transactions: [{hash, block, timestamp, from,
to, value_wei, value_native, gas_used, gas_price_wei, success}]}`.

### `get_token_metadata`

Name, symbol and decimals of a token, plus its price and deepest pool.

| Argument | Type | Notes |
| --- | --- | --- |
| `chain` | enum | as above |
| `address` | string | chain specific format |

Solana mints are read from the chain (`getAccountInfo`), EVM tokens from the
contract (`name()`, `symbol()`, `decimals()`). Result: `{ok, chain, address,
symbol, name, decimals, price_usd, liquidity_usd}`.

## Rate limits

A per-host token bucket throttles every upstream. A 429 is answered with the
upstream `Retry-After` and the request is retried at most twice.

| Host | Budget |
| --- | --- |
| `api.coingecko.com` | 30 requests / minute |
| `api.dexscreener.com` | 60 requests / minute |
| `api.etherscan.io` | 5 requests / second |
| RPC hosts | 10 requests / second (default bucket) |

Responses are cached in memory, so the agent's polling loop does not hit the
free tiers: 60 s for market lists, 300 s for coin details, 30 s for DexScreener,
15 s for on-chain reads, 60 s for transaction history. The cache key is the full
request URL plus parameters, and identical concurrent requests share one call.

## Safety

* **No arbitrary fetching.** There is no "fetch this URL" tool. URLs are built
  from validated parts only, and chain names map to a fixed table of endpoints.
* **Strict input allowlists.** Coin ids `^[a-z0-9-]{1,64}$`, symbols
  `^[A-Za-z0-9]{1,20}$`, EVM addresses `^0x[a-fA-F0-9]{40}$`, Solana addresses
  `^[1-9A-HJ-NP-Za-km-z]{32,44}$`, DEX queries `^[A-Za-z0-9 ._/-]{1,64}$`.
  Anything else is rejected before a request is made.
* **Output sanitisation.** Upstream free text is stripped of ANSI escapes and
  control characters, screened for prompt-injection patterns (the rules mirror
  `ts/packages/agent/src/safety/guard.ts`) and truncated to 200 characters. A
  field that looks like an instruction is dropped entirely.
* **No secrets in the output.** The Etherscan key is never echoed and is
  redacted from error messages.
* **Timeouts and errors.** Every request has a 10 s abort timeout, and upstream
  failures come back as `{"ok": false, "error": "..."}` rather than a stack
  trace.

## Environment

| Variable | Default | Purpose |
| --- | --- | --- |
| `ETHERSCAN_API_KEY` | - | required for `get_evm_tx_history` |
| `SOLANA_RPC_URL` | `https://api.mainnet-beta.solana.com` | Solana RPC endpoint |
| `ETHEREUM_RPC_URL` | `https://ethereum-rpc.publicnode.com` | Ethereum RPC endpoint |
| `BSC_RPC_URL`, `POLYGON_RPC_URL`, `ARBITRUM_RPC_URL`, `OPTIMISM_RPC_URL`, `BASE_RPC_URL`, `AVALANCHE_RPC_URL` | publicnode | per-chain RPC endpoints |
| `MCP_DATA_HTTP_TIMEOUT_MS` | `10000` | HTTP timeout |

## Scripts

```bash
pnpm --filter @openether/mcp-data typecheck   # tsc --noEmit
pnpm --filter @openether/mcp-data test        # vitest run
pnpm --filter @openether/mcp-data build       # tsc -p tsconfig.json
pnpm --filter @openether/mcp-data start       # run the server over stdio
```

The tests never touch the network: `fetch` is mocked and the stdio test only
performs the MCP handshake.

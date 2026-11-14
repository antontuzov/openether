/**
 * Wire types for the openEther core engine.
 *
 * Mirrors `docs/api-contract.md` and the serde types in
 * `rust/crates/core/src/types.rs`. Field names are kept exactly as the engine
 * serialises them (snake_case) so no mapping layer is needed.
 */

/** Venues the engine can trade on. */
export type Venue = 'paper' | 'binance' | 'kraken' | 'hyperliquid';

/** Order side as serialised by the engine (`buy` / `sell`). */
export type Side = 'buy' | 'sell';

/** Circuit breaker state. */
export type BreakerState = 'closed' | 'half_open' | 'open';

/** A single market data point. */
export interface MarketTick {
  venue: Venue;
  symbol: string;
  price: number;
  size: number;
  ts_ms: number;
}

/** An open position. `qty` is signed: positive = long, negative = short. */
export interface Position {
  venue: Venue;
  symbol: string;
  qty: number;
  avg_entry: number;
  mark_price: number;
  unrealized_pnl: number;
  notional_usd: number;
  ts_ms: number;
}

/** A completed fill. */
export interface TradeRecord {
  id: string;
  venue: Venue;
  symbol: string;
  side: Side;
  qty: number;
  price: number;
  fee: number;
  realized_pnl: number;
  ts_ms: number;
  rationale: string;
  /** `agent` | `strategy` | `manual` */
  source: string;
}

/** Order type as serialised by the engine. */
export type OrderType = 'market' | 'limit' | 'stop' | 'take_profit';

/** Lifecycle state of a submitted order. */
export type OrderStatus = 'accepted' | 'filled' | 'rejected' | 'cancelled' | 'partially_filled';

/** A fill the venue confirmed. */
export interface OrderFill {
  venue: Venue;
  order_id: string;
  symbol: string;
  side: Side;
  qty: number;
  price: number;
  fee: number;
  ts_ms: number;
}

/**
 * An order the engine submitted, carrying only the fills the venue confirmed.
 *
 * A resting order has an empty `fills` list: it is neither a position nor a
 * trade until the venue says it filled. `missing_since_ms` is when the venue's
 * open list last stopped containing it — absence alone is not cancellation,
 * because trade history lags the open list.
 */
export interface TrackedOrder {
  venue: Venue;
  order_id: string;
  client_id: string;
  symbol: string;
  side: Side;
  order_type: OrderType;
  limit_price: number | null;
  /** The price a stop or take_profit waits for. */
  trigger_price: number | null;
  /** Orders sharing a group cancel each other when one fills. */
  oco_group: string | null;
  /** The quantity asked for, not the quantity filled. */
  qty: number;
  status: OrderStatus;
  rationale: string;
  submitted_ts_ms: number;
  updated_ts_ms: number;
  fills: OrderFill[];
  missing_since_ms: number | null;
}

/** A log line pushed by the engine or the AI agent. */
export interface LogEntry {
  id: string;
  ts_ms: number;
  /** `agent` | `risk` | `strategy` | `engine` | `mcp` */
  source: string;
  /** `info` | `warn` | `error` (free-form in practice) */
  level: string;
  message: string;
  context: unknown;
}

/** Account-level snapshot of the trading venue. */
export interface PortfolioSnapshot {
  venue: Venue;
  equity_usd: number;
  cash_usd: number;
  unrealized_pnl_usd: number;
  realized_pnl_usd: number;
  total_pnl_pct: number;
  /**
   * Value of the on-chain wallet. Reported, never traded: the risk layer keeps
   * measuring equity on the trading account alone, so this balance cannot widen
   * the limits of the account that can actually lose money.
   */
  onchain_value_usd: number;
  positions: Position[];
  ts_ms: number;
}

/** One token held on-chain, priced from the DEX feed. */
export interface TokenHolding {
  chain: string;
  /** Contract address (EVM) or mint (Solana). */
  asset: string;
  symbol: string;
  balance: number;
  decimals: number;
  /** `null` when no pool quoted the token. */
  price_usd: number | null;
  value_usd: number;
  /** False when the token is held but could not be priced. */
  priced: boolean;
}

/** Everything held on one chain by one address. */
export interface ChainHoldings {
  chain: string;
  address: string;
  native_symbol: string;
  native_balance: number;
  native_price_usd: number | null;
  native_value_usd: number;
  tokens: TokenHolding[];
  /** Dust tokens left unpriced so the free feed's rate limit survives. */
  tokens_skipped: number;
  value_usd: number;
  fetched_at_ms: number;
}

/** Response of `GET /api/wallet/holdings`. */
export interface WalletHoldings {
  /** False when no keystore is configured, so every balance is zero. */
  configured: boolean;
  chains: ChainHoldings[];
  value_usd: number;
  fetched_at_ms: number;
}

/** Hard limits enforced by the risk layer. */
export interface RiskLimits {
  max_position_pct_of_equity: number;
  max_gross_exposure_pct: number;
  max_daily_loss_pct: number;
  max_drawdown_pct: number;
  max_open_positions: number;
  max_order_notional_usd: number;
}

/** Live risk state, also pushed over the WebSocket. */
export interface RiskState {
  breaker_state: BreakerState;
  breaker_reason: string | null;
  equity_usd: number;
  peak_equity_usd: number;
  drawdown_pct: number;
  daily_pnl_usd: number;
  open_positions: number;
  gross_exposure_usd: number;
  limits: RiskLimits;
}

/** Metadata about the currently loaded WASM strategy. */
export interface StrategyInfo {
  name: string;
  loaded_at_ms: number;
  wasm_sha256: string;
  ticks_processed: number;
  fuel_used: number;
}

/** Health of a single venue. */
export interface VenueHealth {
  venue: Venue;
  healthy: boolean;
  live: boolean;
}

/** Engine lifecycle status pushed as a `status` frame. */
export interface EngineStatus {
  state: string;
  detail: string;
  ts_ms: number;
}

/**
 * Response of `POST /api/strategy/reload`.
 * The contract does not pin the body down, so every field is optional and the
 * panel renders whatever the server sends.
 */
export interface StrategyReloadResponse {
  name?: string;
  loaded_at_ms?: number;
  wasm_sha256?: string;
  ticks_processed?: number;
  fuel_used?: number;
  ok?: boolean;
  message?: string;
  error?: string;
}

/** Response of `POST /api/breaker/reset`. */
export interface BreakerResetResponse {
  ok?: boolean;
  message?: string;
  error?: string;
  breaker_state?: BreakerState;
}

/** A raw WebSocket frame: always `{ "type": ..., "data": ... }`. */
export interface WsFrame {
  type: string;
  data?: unknown;
}

/** Maps a frame type to the payload carried in `data`. */
export interface WsFrameDataMap {
  tick: MarketTick;
  trade: TradeRecord;
  log: LogEntry;
  portfolio: PortfolioSnapshot;
  risk: RiskState;
  status: EngineStatus;
  pong: null;
}

/** Frame types with a known payload shape. */
export type WsFrameType = keyof WsFrameDataMap;

/**
 * Narrow a raw frame to a known type without casting at the call site.
 *
 * ```ts
 * if (isWsFrame(frame, 'tick')) frame.data.price; // MarketTick
 * ```
 */
export function isWsFrame<K extends WsFrameType>(
  frame: WsFrame,
  type: K,
): frame is WsFrame & { type: K; data: WsFrameDataMap[K] } {
  return frame.type === type;
}

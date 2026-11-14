/**
 * Shared types for the agent runtime.
 *
 * The runtime talks to language models through a narrow interface so a provider
 * can be swapped without touching the trading loop.
 */

export type Role = 'system' | 'user' | 'assistant' | 'tool';

export interface ChatMessage {
  role: Role;
  content: string;
  /** Set on tool results so the provider can correlate the call. */
  toolCallId?: string;
  /** Tool name for tool results. */
  name?: string;
}

export interface ToolSpec {
  name: string;
  description: string;
  inputSchema: unknown;
}

export interface ToolCall {
  id: string;
  name: string;
  arguments: Record<string, unknown>;
}

export interface LlmRequest {
  messages: ChatMessage[];
  tools: ToolSpec[];
  maxTokens?: number;
  temperature?: number;
}

export interface LlmUsage {
  promptTokens?: number;
  completionTokens?: number;
}

export interface LlmResponse {
  content: string;
  toolCalls: ToolCall[];
  usage?: LlmUsage;
}

export interface LlmProvider {
  readonly name: string;
  complete(request: LlmRequest): Promise<LlmResponse>;
}

/** Risk verdict returned by the core for every order attempt. */
export interface RiskVerdict {
  approved: boolean;
  reason: string;
  detail: string;
  breaker_state: string;
  checked_at_ms?: number;
}

export interface SubmitOutcome {
  approved: boolean;
  reason: string;
  detail: string;
  order_id?: string;
  status?: string;
  fill_price?: number;
  fee?: number;
  risk: RiskVerdict;
}

export interface PortfolioPosition {
  symbol: string;
  qty: number;
  avg_entry: number;
  mark_price: number;
  unrealized_pnl: number;
  notional_usd: number;
}

export interface PortfolioSnapshot {
  venue: string;
  equity_usd: number;
  cash_usd: number;
  unrealized_pnl_usd: number;
  realized_pnl_usd: number;
  total_pnl_pct: number;
  /**
   * On-chain wallet value. Reported only — the agent must never treat it as
   * spendable trading equity.
   */
  onchain_value_usd?: number;
  positions: PortfolioPosition[];
  ts_ms: number;
}

export interface RiskState {
  breaker_state: 'closed' | 'half_open' | 'open';
  breaker_reason: string | null;
  equity_usd: number;
  peak_equity_usd: number;
  drawdown_pct: number;
  daily_pnl_usd: number;
  open_positions: number;
  gross_exposure_usd: number;
  limits: Record<string, number>;
  ts_ms: number;
}

export interface MarketTick {
  venue: string;
  symbol: string;
  price: number;
  size: number;
  ts_ms: number;
}

export interface OrderFill {
  venue: string;
  order_id: string;
  symbol: string;
  side: 'buy' | 'sell';
  qty: number;
  price: number;
  fee: number;
  ts_ms: number;
}

/**
 * An order the engine is tracking.
 *
 * `status` is the engine's inference from the venue's open list; `fills` is what
 * the venue actually confirmed. Only the fills are money, so verification reads
 * them rather than trusting the label.
 */
export interface TrackedOrder {
  venue: string;
  order_id: string;
  symbol: string;
  side: 'buy' | 'sell';
  order_type: 'market' | 'limit';
  limit_price: number | null;
  qty: number;
  status: 'accepted' | 'filled' | 'rejected' | 'cancelled' | 'partially_filled';
  rationale: string;
  submitted_ts_ms: number;
  updated_ts_ms: number;
  fills: OrderFill[];
}

export interface TradeRecord {
  id: string;
  venue: string;
  symbol: string;
  side: 'buy' | 'sell';
  qty: number;
  price: number;
  fee: number;
  realized_pnl: number;
  ts_ms: number;
  rationale: string;
  source: string;
}

import type {
  ChatMessage,
  LlmProvider,
  MarketTick,
  PortfolioSnapshot,
  RiskState,
  ToolCall,
  ToolSpec,
  TrackedOrder,
} from '../types.js';
import { PLANNING_INSTRUCTIONS, PERSONA } from '../persona.js';
import { AgentLogger } from '../logger.js';
import { AgentMetrics } from '../metrics.js';
import { MemoryStore } from '../memory/store.js';
import { sanitizeToolOutput, validateToolArguments } from '../safety/guard.js';

export interface ToolExecutor {
  listTools(): Promise<ToolSpec[]>;
  callTool(name: string, args: Record<string, unknown>): Promise<string>;
}

export interface OrchestratorOptions {
  provider: LlmProvider;
  coreTools: ToolExecutor;
  dataTools?: ToolExecutor;
  logger: AgentLogger;
  metrics: AgentMetrics;
  memory: MemoryStore;
  maxSteps: number;
  symbol: string;
  contractAllowlist: string[];
  /** Called with the raw cycle context before the model is invoked. */
  onContext?: (context: CycleSnapshot) => void;
}

/** Everything the orchestrator knows about the market when a cycle starts. */
export interface CycleSnapshot {
  portfolio: PortfolioSnapshot | null;
  risk: RiskState | null;
  ticks: MarketTick[];
  orders: TrackedOrder[];
}

export interface CycleResult {
  plan: string;
  toolCalls: ToolCall[];
  tradeAttempted: boolean;
  tradeApproved: boolean;
  tradeReason: string;
}

/** Tools the model may call to move money. At most one is allowed per cycle. */
const TRADE_TOOLS = new Set(['execute_trade']);

/**
 * The plan / execute / verify loop.
 *
 * The orchestrator owns the policy: how many trades a cycle may contain, which
 * tools exist, what happens to tool output before it reaches the model and what
 * is written to the log. The model only decides *what* to do; the policy
 * decides what is allowed to happen.
 */
export class Orchestrator {
  constructor(private readonly options: OrchestratorOptions) {}

  async runCycle(): Promise<CycleResult> {
    const { logger, metrics, memory, provider } = this.options;

    const portfolio = await this.readPortfolio();
    const risk = await this.readRisk();
    const ticks = await this.readTicks();
    const orders = await this.readOrders();

    this.options.onContext?.({ portfolio, risk, ticks, orders });

    if (risk) {
      metrics.breakerState.set(
        risk.breaker_state === 'closed' ? 0 : risk.breaker_state === 'half_open' ? 1 : 2,
      );
    }

    const memories = await memory.search(
      `${this.options.symbol} ${portfolio?.positions.map((p) => p.symbol).join(' ') ?? ''} trade risk`,
      5,
    );

    const context = {
      ts: new Date().toISOString(),
      symbol: this.options.symbol,
      portfolio,
      risk,
      recent_ticks: ticks.slice(-30),
      // Resting orders are exposure that has not been booked yet. Without them
      // the model would re-submit a trade it already has working.
      working_orders: orders.filter(isWorking).map(summarizeOrder),
      relevant_memories: memories.map((record) => ({
        ts: record.ts,
        kind: record.kind,
        text: record.text,
      })),
    };

    const messages: ChatMessage[] = [
      { role: 'system', content: PERSONA },
      {
        role: 'user',
        content: `${PLANNING_INSTRUCTIONS}\n\nCurrent cycle context:\n${JSON.stringify(context, null, 2)}`,
      },
    ];

    const tools = await this.availableTools();
    const toolCalls: ToolCall[] = [];
    let tradeAttempted = false;
    let tradeApproved = false;
    let tradeReason = '';
    let tradeOrderId: string | null = null;
    let plan = '';

    for (let step = 0; step < this.options.maxSteps; step += 1) {
      const started = Date.now();
      let response;
      try {
        response = await provider.complete({ messages, tools });
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        metrics.llmCalls.inc({ provider: provider.name, result: 'error' });
        await logger.error(`the model call failed: ${message}`, { step });
        break;
      }
      metrics.llmCalls.inc({ provider: provider.name, result: 'ok' });
      metrics.llmLatency.observe({ provider: provider.name }, (Date.now() - started) / 1000);

      if (response.content) {
        plan = response.content;
      }

      if (response.toolCalls.length === 0) {
        break;
      }

      messages.push({
        role: 'assistant',
        content: response.content || 'calling tools',
      });

      for (const call of response.toolCalls) {
        const outcome = await this.runTool(call, {
          tradeAttempted,
          risk,
        });
        toolCalls.push(call);

        if (call.name === 'execute_trade') {
          tradeAttempted = true;
          tradeApproved = outcome.approved;
          tradeReason = outcome.reason;
          tradeOrderId = readOrderId(outcome.value);
        }

        messages.push({
          role: 'tool',
          name: call.name,
          toolCallId: call.id,
          content: JSON.stringify(outcome.value),
        });
      }
    }

    if (tradeAttempted) {
      await this.verify(tradeApproved, tradeReason, tradeOrderId);
    }

    await memory.append({
      kind: tradeAttempted ? 'outcome' : 'plan',
      text: `cycle on ${this.options.symbol}: ${plan.slice(0, 800)}`,
      tags: tradeAttempted ? ['trade', tradeApproved ? 'approved' : 'rejected'] : ['plan'],
    });

    metrics.cycles.inc();
    return { plan, toolCalls, tradeAttempted, tradeApproved, tradeReason };
  }

  private async availableTools(): Promise<ToolSpec[]> {
    const core = await this.options.coreTools.listTools();
    if (!this.options.dataTools) {
      return core;
    }
    const data = await this.options.dataTools.listTools();
    // Data tools are read-only by construction: anything that could move money
    // is filtered out even if the data server exposes it.
    const readOnly = data.filter((tool) => !TRADE_TOOLS.has(tool.name));
    return [...core, ...readOnly];
  }

  private async runTool(
    call: ToolCall,
    context: { tradeAttempted: boolean; risk: RiskState | null },
  ): Promise<{ value: unknown; approved: boolean; reason: string }> {
    const { logger, metrics } = this.options;

    if (TRADE_TOOLS.has(call.name) && context.tradeAttempted) {
      await logger.warn('refused a second trade in the same cycle', { tool: call.name });
      metrics.toolCalls.inc({ tool: call.name, result: 'refused' });
      return { value: { error: 'only one trade per cycle is allowed' }, approved: false, reason: 'cycle_trade_limit' };
    }

    if (TRADE_TOOLS.has(call.name) && context.risk && context.risk.breaker_state !== 'closed') {
      await logger.warn('refused to plan a trade while the circuit breaker is not closed', {
        breaker_state: context.risk.breaker_state,
      });
      metrics.toolCalls.inc({ tool: call.name, result: 'refused' });
      return { value: { error: 'the circuit breaker is not closed' }, approved: false, reason: 'breaker_not_closed' };
    }

    const validation = validateToolArguments(call.name, call.arguments);
    if (!validation.ok) {
      await logger.warn(`tool arguments rejected: ${validation.error}`, { tool: call.name });
      metrics.toolCalls.inc({ tool: call.name, result: 'invalid' });
      return { value: { error: validation.error }, approved: false, reason: 'invalid_arguments' };
    }

    const executor = this.executorFor(call.name);
    if (!executor) {
      await logger.warn(`the model asked for an unknown tool: ${call.name}`);
      metrics.toolCalls.inc({ tool: call.name, result: 'unknown' });
      return { value: { error: `unknown tool ${call.name}` }, approved: false, reason: 'unknown_tool' };
    }

    try {
      const raw = await executor.callTool(call.name, call.arguments);
      const parsed = safeParse(raw);
      const guarded = sanitizeToolOutput(parsed, {
        maxChars: 20_000,
        contractAllowlist: this.options.contractAllowlist,
      });
      for (const finding of guarded.findings) {
        metrics.guardFindings.inc({ kind: finding });
        await logger.warn(`tool output flagged: ${finding}`, { tool: call.name });
      }

      const approved = readApproved(guarded.value);
      const reason = readReason(guarded.value);
      metrics.toolCalls.inc({ tool: call.name, result: approved === false ? 'rejected' : 'ok' });
      await logger.info(`tool ${call.name} completed`, {
        arguments: call.arguments,
        approved,
        reason,
        findings: guarded.findings,
      });
      return { value: guarded.value, approved: approved ?? true, reason: reason ?? '' };
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      metrics.toolCalls.inc({ tool: call.name, result: 'error' });
      await logger.error(`tool ${call.name} failed: ${message}`, { arguments: call.arguments });
      return { value: { error: message }, approved: false, reason: 'tool_error' };
    }
  }

  private executorFor(name: string): ToolExecutor | null {
    const coreNames = new Set(['execute_trade', 'get_portfolio', 'get_orders', 'cancel_order', 'get_positions', 'get_market_data', 'get_risk_state', 'get_trade_history', 'fetch_onchain_data', 'set_strategy', 'get_agent_logs']);
    if (coreNames.has(name)) {
      return this.options.coreTools;
    }
    return this.options.dataTools ?? null;
  }

  /**
   * Report what actually happened, not what was asked for.
   *
   * An accepted order is not a fill: it may still be resting, or partly filled.
   * The venue's fills are the only evidence of money moving, so the log carries
   * them next to the latest trade and whatever is still working.
   */
  private async verify(approved: boolean, reason: string, orderId: string | null): Promise<void> {
    const { logger, metrics } = this.options;
    const trades = await this.readTrades();
    const latest = trades[0];
    const orders = await this.readOrders();
    const submitted = orderId ? orders.find((order) => order.order_id === orderId) ?? null : null;
    await logger.info('verified the outcome of the trade', {
      approved,
      reason,
      order: submitted
        ? {
            ...summarizeOrder(submitted),
            fills: submitted.fills.map((fill) => ({
              qty: fill.qty,
              price: fill.price,
              fee: fill.fee,
            })),
          }
        : null,
      latest_trade: latest
        ? {
            symbol: latest.symbol,
            side: latest.side,
            qty: latest.qty,
            price: latest.price,
            fee: latest.fee,
          }
        : null,
      still_working: orders.filter(isWorking).map(summarizeOrder),
    });
    metrics.trades.inc({ result: approved ? 'approved' : 'rejected' });
  }

  private async readPortfolio(): Promise<PortfolioSnapshot | null> {
    const raw = await this.safeCall('get_portfolio');
    const parsed = safeParse(raw);
    return isObject(parsed) ? (parsed as unknown as PortfolioSnapshot) : null;
  }

  private async readRisk(): Promise<RiskState | null> {
    const raw = await this.safeCall('get_risk_state');
    const parsed = safeParse(raw);
    return isObject(parsed) ? (parsed as unknown as RiskState) : null;
  }

  private async readTicks(): Promise<MarketTick[]> {
    const raw = await this.safeCall('get_market_data', { symbol: this.options.symbol, lookback: 60 });
    const parsed = safeParse(raw);
    if (isObject(parsed) && Array.isArray((parsed as { ticks?: unknown }).ticks)) {
      return (parsed as { ticks: MarketTick[] }).ticks;
    }
    return [];
  }

  private async readTrades() {
    const raw = await this.safeCall('get_trade_history', { limit: 5 });
    const parsed = safeParse(raw);
    return Array.isArray(parsed) ? parsed : [];
  }

  private async readOrders(): Promise<TrackedOrder[]> {
    const raw = await this.safeCall('get_orders');
    const parsed = safeParse(raw);
    return Array.isArray(parsed) ? (parsed as TrackedOrder[]) : [];
  }

  private async safeCall(name: string, args: Record<string, unknown> = {}): Promise<string> {
    try {
      return await this.options.coreTools.callTool(name, args);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      await this.options.logger.warn(`the core tool ${name} failed: ${message}`);
      return '';
    }
  }
}

function safeParse(raw: string): unknown {
  if (!raw) {
    return null;
  }
  try {
    return JSON.parse(raw);
  } catch {
    return raw;
  }
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function readApproved(value: unknown): boolean | undefined {
  if (isObject(value) && typeof value['approved'] === 'boolean') {
    return value['approved'];
  }
  return undefined;
}

function readReason(value: unknown): string | undefined {
  if (isObject(value) && typeof value['reason'] === 'string') {
    return value['reason'];
  }
  return undefined;
}

function readOrderId(value: unknown): string | null {
  if (isObject(value) && typeof value['order_id'] === 'string') {
    return value['order_id'];
  }
  return null;
}

/** Still resting at the venue, so it can fill without the agent asking again. */
function isWorking(order: TrackedOrder): boolean {
  return order.status === 'accepted' || order.status === 'partially_filled';
}

function summarizeOrder(order: TrackedOrder) {
  const filled = order.fills.reduce((total, fill) => total + fill.qty, 0);
  return {
    order_id: order.order_id,
    symbol: order.symbol,
    side: order.side,
    order_type: order.order_type,
    limit_price: order.limit_price,
    trigger_price: order.trigger_price,
    oco_group: order.oco_group,
    qty: order.qty,
    filled_qty: filled,
    status: order.status,
  };
}

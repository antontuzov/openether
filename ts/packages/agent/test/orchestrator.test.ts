import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { Orchestrator, type ToolExecutor } from '../src/loop/orchestrator.js';
import { AgentLogger } from '../src/logger.js';
import { AgentMetrics } from '../src/metrics.js';
import { MemoryStore } from '../src/memory/store.js';
import { HashEmbedder } from '../src/memory/embeddings.js';
import type { LlmProvider, LlmRequest, LlmResponse, ToolCall, TrackedOrder } from '../src/types.js';

/**
 * The verify half of the loop, driven through the answers a real venue gives.
 *
 * The point of these tests is that an accepted order is not a fill: the agent
 * has to report what the venue confirmed, including an order that is still
 * resting, rather than logging the trade it asked for.
 */

let directory: string;

beforeEach(async () => {
  directory = await mkdtemp(join(tmpdir(), 'openether-agent-'));
});

afterEach(async () => {
  await rm(directory, { recursive: true, force: true });
});

function order(overrides: Partial<TrackedOrder> = {}): TrackedOrder {
  return {
    venue: 'binance',
    order_id: 'o-1',
    symbol: 'SOL/USDT',
    side: 'buy',
    order_type: 'limit',
    limit_price: 130,
    trigger_price: null,
    oco_group: null,
    qty: 1,
    status: 'accepted',
    rationale: 'momentum',
    submitted_ts_ms: 1_700_000_000_000,
    updated_ts_ms: 1_700_000_000_000,
    fills: [],
    ...overrides,
  };
}

function fill(qty: number, price: number) {
  return {
    venue: 'binance',
    order_id: 'o-1',
    symbol: 'SOL/USDT',
    side: 'buy' as const,
    qty,
    price,
    fee: 0.01,
    ts_ms: 1_700_000_001_000,
  };
}

class ScriptedCore implements ToolExecutor {
  readonly calls: Array<{ name: string; args: Record<string, unknown> }> = [];

  constructor(private readonly answers: Record<string, unknown>) {}

  async listTools() {
    return Object.keys(this.answers).map((name) => ({
      name,
      description: name,
      inputSchema: { type: 'object' },
    }));
  }

  async callTool(name: string, args: Record<string, unknown>): Promise<string> {
    this.calls.push({ name, args });
    const answer = this.answers[name];
    if (answer === undefined) {
      throw new Error(`the core does not expose ${name}`);
    }
    return JSON.stringify(answer);
  }
}

class ScriptedModel implements LlmProvider {
  readonly name = 'scripted';
  readonly requests: LlmRequest[] = [];
  private step = 0;

  constructor(private readonly script: ToolCall[][]) {}

  async complete(request: LlmRequest): Promise<LlmResponse> {
    this.requests.push(request);
    const toolCalls = this.script[this.step] ?? [];
    this.step += 1;
    return { content: 'THESIS: scripted.', toolCalls };
  }
}

const portfolio = {
  venue: 'binance',
  equity_usd: 10_000,
  cash_usd: 10_000,
  unrealized_pnl_usd: 0,
  realized_pnl_usd: 0,
  total_pnl_pct: 0,
  positions: [],
  ts_ms: 1,
};

const risk = {
  breaker_state: 'closed',
  breaker_reason: null,
  equity_usd: 10_000,
  peak_equity_usd: 10_000,
  drawdown_pct: 0,
  daily_pnl_usd: 0,
  open_positions: 0,
  gross_exposure_usd: 0,
  limits: {},
  ts_ms: 1,
};

async function run(core: ScriptedCore, model: ScriptedModel) {
  const logger = new AgentLogger(directory, 'info', 'http://127.0.0.1:1');
  const orchestrator = new Orchestrator({
    provider: model,
    coreTools: core,
    logger,
    metrics: new AgentMetrics(),
    memory: await MemoryStore.open(directory, new HashEmbedder()),
    maxSteps: 3,
    symbol: 'SOL/USDT',
    contractAllowlist: [],
  });
  const result = await orchestrator.runCycle();
  // A cycle that calls no tools logs nothing, so the file may not exist yet.
  const raw = await readFile(join(directory, 'agent.jsonl'), 'utf8').catch(() => '');
  const lines = raw
    .split('\n')
    .filter((line) => line.trim().length > 0)
    .map((line) => JSON.parse(line) as { message: string; context: Record<string, unknown> });
  return { result, lines };
}

function tradeCall(): ToolCall {
  return {
    id: 'call-1',
    name: 'execute_trade',
    arguments: { symbol: 'SOL/USDT', side: 'buy', qty: 1, order_type: 'limit', limit_price: 130 },
  };
}

describe('the verify step', () => {
  it('reports the fills the venue confirmed, not the trade that was asked for', async () => {
    const core = new ScriptedCore({
      get_portfolio: portfolio,
      get_risk_state: risk,
      get_market_data: { ticks: [] },
      get_trade_history: [],
      get_orders: [order({ status: 'partially_filled', fills: [fill(0.4, 130)] })],
      execute_trade: { approved: true, reason: '', detail: '', order_id: 'o-1', status: 'accepted' },
    });
    const { lines } = await run(core, new ScriptedModel([[tradeCall()], []]));

    const verified = lines.find((line) => line.message === 'verified the outcome of the trade');
    expect(verified).toBeDefined();
    const context = verified?.context as {
      order: { status: string; filled_qty: number; qty: number; fills: Array<{ qty: number; price: number }> };
      still_working: Array<{ order_id: string }>;
    };
    expect(context.order.status).toBe('partially_filled');
    expect(context.order.qty).toBe(1);
    expect(context.order.filled_qty).toBe(0.4);
    expect(context.order.fills).toEqual([{ qty: 0.4, price: 130, fee: 0.01 }]);
    expect(context.still_working.map((item) => item.order_id)).toEqual(['o-1']);
  });

  it('says so when the order is still resting and nothing has filled', async () => {
    const core = new ScriptedCore({
      get_portfolio: portfolio,
      get_risk_state: risk,
      get_market_data: { ticks: [] },
      get_trade_history: [],
      get_orders: [order()],
      execute_trade: { approved: true, reason: '', detail: '', order_id: 'o-1', status: 'accepted' },
    });
    const { lines } = await run(core, new ScriptedModel([[tradeCall()], []]));

    const verified = lines.find((line) => line.message === 'verified the outcome of the trade');
    const context = verified?.context as {
      order: { filled_qty: number; fills: unknown[] };
      latest_trade: unknown;
    };
    expect(context.order.filled_qty).toBe(0);
    expect(context.order.fills).toEqual([]);
    expect(context.latest_trade).toBeNull();
  });

  it('shows working orders to the model before it plans', async () => {
    const core = new ScriptedCore({
      get_portfolio: portfolio,
      get_risk_state: risk,
      get_market_data: { ticks: [] },
      get_trade_history: [],
      get_orders: [order(), order({ order_id: 'o-2', status: 'filled', fills: [fill(1, 130)] })],
    });
    const model = new ScriptedModel([[]]);
    await run(core, model);

    const prompt = model.requests[0]?.messages[1]?.content ?? '';
    const context = JSON.parse(prompt.slice(prompt.indexOf('{'))) as {
      working_orders: Array<{ order_id: string; filled_qty: number }>;
    };
    expect(context.working_orders).toEqual([
      {
        order_id: 'o-1',
        symbol: 'SOL/USDT',
        side: 'buy',
        order_type: 'limit',
        limit_price: 130,
        trigger_price: null,
        oco_group: null,
        qty: 1,
        filled_qty: 0,
        status: 'accepted',
      },
    ]);
  });

  it('routes cancel_order to the core and reports a refusal as an outcome', async () => {
    const core = new ScriptedCore({
      get_portfolio: portfolio,
      get_risk_state: risk,
      get_market_data: { ticks: [] },
      get_trade_history: [],
      get_orders: [order()],
      cancel_order: { ok: false, status: 400, response: 'order o-1 is filled: only a resting order can be cancelled' },
    });
    const cancel: ToolCall = { id: 'call-2', name: 'cancel_order', arguments: { order_id: 'o-1' } };
    const { result } = await run(core, new ScriptedModel([[cancel], []]));

    expect(core.calls.some((call) => call.name === 'cancel_order')).toBe(true);
    expect(result.tradeAttempted).toBe(false);
  });
});
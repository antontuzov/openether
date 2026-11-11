import type { LlmProvider, LlmRequest, LlmResponse, ToolCall } from '../types.js';
import type { MarketTick, PortfolioSnapshot, RiskState } from '../types.js';

export interface CycleContext {
  portfolio: PortfolioSnapshot;
  risk: RiskState;
  ticks: MarketTick[];
  symbol: string;
}

/**
 * Deterministic offline planner.
 *
 * It runs the same tool-calling protocol as a hosted model, so the whole
 * plan/execute/verify loop, the MCP transport and the risk layer stay under
 * test without an API key and without network access to a model provider.
 */
export class LocalPlannerProvider implements LlmProvider {
  readonly name = 'local';

  /** Set by the orchestrator so the planner can read the cycle context. */
  context: CycleContext | null = null;

  async complete(request: LlmRequest): Promise<LlmResponse> {
    const context = this.context;
    if (!context) {
      return {
        content:
          'No cycle context available. The offline planner needs a portfolio, a risk state and market data before it can plan.',
        toolCalls: [],
      };
    }

    const { portfolio, risk, ticks, symbol } = context;

    if (risk.breaker_state !== 'closed') {
      return {
        content: [
          'THESIS: the circuit breaker is open, so no analysis is actionable.',
          `EVIDENCE: breaker_state=${risk.breaker_state}, reason=${risk.breaker_reason ?? 'unknown'}.`,
          'PLAN: hold every position and do not open new risk.',
          'RISK: $0. The engine has already halted trading.',
          'INVALIDATION: the breaker returning to closed after a successful probe order.',
        ].join('\n'),
        toolCalls: [],
      };
    }

    const prices = ticks.filter((tick) => tick.symbol === symbol).map((tick) => tick.price);
    const last = prices.at(-1) ?? 0;
    const fast = average(prices.slice(-5));
    const slow = average(prices.slice(-20));
    const spread = slow > 0 ? ((fast - slow) / slow) * 100 : 0;

    const position = portfolio.positions.find((p) => p.symbol === symbol);
    const exposurePct =
      portfolio.equity_usd > 0
        ? (Math.abs(position?.qty ?? 0) * last * 100) / portfolio.equity_usd
        : 0;

    const thesis =
      spread > 0.15
        ? `price is trending up: the fast average sits ${spread.toFixed(2)}% above the slow average`
        : spread < -0.15
          ? `price is trending down: the fast average sits ${Math.abs(spread).toFixed(2)}% below the slow average`
          : 'price action is flat, with no momentum worth trading';

    const evidence = [
      `price=${last.toFixed(4)}`,
      `fast_sma=${fast.toFixed(4)}`,
      `slow_sma=${slow.toFixed(4)}`,
      `spread=${spread.toFixed(2)}%`,
      `equity=$${portfolio.equity_usd.toFixed(2)}`,
      `exposure=${exposurePct.toFixed(2)}% of equity`,
      `drawdown=${risk.drawdown_pct.toFixed(2)}%`,
      `samples=${prices.length}`,
    ].join(', ');

    const maxPositionPct = risk.limits['max_position_pct_of_equity'] ?? 20;
    const minNotional = risk.limits['min_order_notional_usd'] ?? 10;
    const maxLossUsd = Math.max(0, portfolio.equity_usd * 0.005);

    if (spread > -0.15 && spread <= 0.15) {
      return {
        content: [
          `THESIS: ${thesis}.`,
          `EVIDENCE: ${evidence}.`,
          'PLAN: no trade. The signal is inside the noise band and there is nothing worth paying spread for.',
          `RISK: $0. Holding costs nothing in this market.`,
          'INVALIDATION: a fast average more than 0.15% away from the slow average.',
        ].join('\n'),
        toolCalls: [],
      };
    }

    if (spread > 0.15) {
      const budget = (portfolio.equity_usd * (maxPositionPct / 100)) / 2;
      const qty = Math.floor((budget / last) * 10_000) / 10_000;
      if (qty <= 0 || qty * last < minNotional) {
        return {
          content: [
            `THESIS: ${thesis}.`,
            `EVIDENCE: ${evidence}.`,
            'PLAN: no trade. The position size that the risk limits allow is below the minimum order size.',
            `RISK: $0.`,
            'INVALIDATION: a larger equity base or a lower minimum order size.',
          ].join('\n'),
          toolCalls: [],
        };
      }

      const toolCall: ToolCall = {
        id: `local-${Date.now()}`,
        name: 'execute_trade',
        arguments: {
          symbol,
          side: 'buy',
          qty,
          rationale: `momentum: fast SMA ${spread.toFixed(2)}% above slow SMA; risking at most $${maxLossUsd.toFixed(2)} (0.5% of equity)`,
        },
      };

      return {
        content: [
          `THESIS: ${thesis}.`,
          `EVIDENCE: ${evidence}.`,
          `PLAN: buy ${qty} ${symbol} at market, sized to ${((qty * last * 100) / portfolio.equity_usd).toFixed(2)}% of equity.`,
          `RISK: at most $${maxLossUsd.toFixed(2)}, which is 0.5% of equity, with the stop at the invalidation level.`,
          'INVALIDATION: the fast average crossing back below the slow average.',
        ].join('\n'),
        toolCalls: [toolCall],
      };
    }

    // Negative momentum: close a long position rather than opening a short.
    if (position && position.qty > 0) {
      const toolCall: ToolCall = {
        id: `local-${Date.now()}`,
        name: 'execute_trade',
        arguments: {
          symbol,
          side: 'sell',
          qty: position.qty,
          rationale: `momentum reversal: fast SMA ${spread.toFixed(2)}% below slow SMA, closing the long to protect the position`,
        },
      };
      return {
        content: [
          `THESIS: ${thesis}.`,
          `EVIDENCE: ${evidence}.`,
          `PLAN: close the ${position.qty} ${symbol} long.`,
          `RISK: at most $${maxLossUsd.toFixed(2)}.`,
          'INVALIDATION: the fast average recovering above the slow average.',
        ].join('\n'),
        toolCalls: [toolCall],
      };
    }

    return {
      content: [
        `THESIS: ${thesis}.`,
        `EVIDENCE: ${evidence}.`,
        'PLAN: no trade. Momentum is down but there is no position to protect.',
        `RISK: $0.`,
        'INVALIDATION: a long position that needs protecting, or momentum turning back up.',
      ].join('\n'),
      toolCalls: [],
    };
  }
}

function average(values: number[]): number {
  if (values.length === 0) {
    return 0;
  }
  return values.reduce((total, value) => total + value, 0) / values.length;
}

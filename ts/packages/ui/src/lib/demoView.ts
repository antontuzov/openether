/**
 * Adapters between the browser simulation and the engine's wire types.
 *
 * The panels already know how to render a `PortfolioSnapshot`, a `Position[]`
 * and a `TradeRecord[]`, so the simulator maps its own state onto those shapes
 * instead of duplicating every panel.
 */

import type { PortfolioSnapshot, Position, RiskState, TradeRecord } from '../api/types';
import { equityOf, freeCash, grossExposure, type DemoState } from './demo';
import { DEFAULT_LIMITS, type RuleLimits } from './riskRules';

export function demoToPortfolio(state: DemoState): PortfolioSnapshot {
  const equity = equityOf(state);
  const unrealized = Object.values(state.positions).reduce(
    (total, position) => total + position.unrealizedPnl,
    0,
  );
  const realized = state.trades.reduce((total, trade) => total + trade.realizedPnl, 0);
  const startEquity = state.dayStartEquity || equity;
  return {
    venue: 'paper',
    equity_usd: equity,
    cash_usd: freeCash(state),
    unrealized_pnl_usd: unrealized,
    realized_pnl_usd: realized,
    total_pnl_pct: startEquity > 0 ? ((equity - startEquity) / startEquity) * 100 : 0,
    positions: demoToPositions(state),
    ts_ms: Date.now(),
  };
}

export function demoToPositions(state: DemoState): Position[] {
  return Object.values(state.positions).map((position) => ({
    venue: 'paper' as const,
    symbol: position.symbol,
    qty: position.qty,
    avg_entry: position.avgEntry,
    mark_price: position.markPrice,
    unrealized_pnl: position.unrealizedPnl,
    notional_usd: position.notionalUsd,
    ts_ms: Date.now(),
  }));
}

export function demoToTrades(state: DemoState): TradeRecord[] {
  return state.trades.map((trade) => ({
    id: trade.id,
    venue: 'paper' as const,
    symbol: trade.symbol,
    side: trade.side,
    qty: trade.qty,
    price: trade.price,
    fee: trade.fee,
    realized_pnl: trade.realizedPnl,
    ts_ms: trade.tsMs,
    rationale: trade.rationale,
    source: 'manual',
  }));
}

export function demoToRisk(state: DemoState, limits: RuleLimits = DEFAULT_LIMITS): RiskState {
  const equity = equityOf(state);
  const peak = state.peakEquity > 0 ? state.peakEquity : equity;
  const startEquity = state.dayStartEquity || equity;
  return {
    breaker_state: state.breakerState,
    breaker_reason: state.breakerReason,
    equity_usd: equity,
    peak_equity_usd: peak,
    drawdown_pct: peak > 0 ? ((peak - equity) / peak) * 100 : 0,
    daily_pnl_usd: equity - startEquity,
    open_positions: Object.values(state.positions).filter((position) => position.qty !== 0).length,
    gross_exposure_usd: grossExposure(state),
    limits: {
      max_position_pct_of_equity: limits.max_position_pct_of_equity,
      max_gross_exposure_pct: limits.max_gross_exposure_pct,
      max_daily_loss_pct: limits.max_daily_loss_pct,
      max_drawdown_pct: limits.max_drawdown_pct,
      max_open_positions: limits.max_open_positions,
      max_order_notional_usd: limits.max_order_notional_usd,
    },
  };
}

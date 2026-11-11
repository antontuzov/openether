/**
 * Client-side paper trading.
 *
 * The published dashboard is a static site, so it has no Rust core to talk
 * to. This module simulates the engine in the browser: it fills orders
 * immediately at the quoted price and applies the SAME risk limits the Rust
 * risk layer enforces, so what a visitor sees on the public site is a
 * faithful picture of what the real engine would do.
 *
 * Nothing here is real money. There is no exchange credential, no signing and
 * no broadcast path anywhere in this file.
 */

import type { Coin } from './coins';

export type Side = 'buy' | 'sell';
export type BreakerState = 'closed' | 'half_open' | 'open';

export interface DemoLimits {
  max_position_pct_of_equity: number;
  max_gross_exposure_pct: number;
  max_daily_loss_pct: number;
  max_drawdown_pct: number;
  max_open_positions: number;
  max_order_notional_usd: number;
  min_order_notional_usd: number;
}

/** Mirrors the defaults in `rust/crates/core/src/config.rs`. */
export const DEFAULT_LIMITS: DemoLimits = {
  max_position_pct_of_equity: 20,
  max_gross_exposure_pct: 60,
  max_daily_loss_pct: 3,
  max_drawdown_pct: 10,
  max_open_positions: 5,
  max_order_notional_usd: 5000,
  min_order_notional_usd: 10,
};

export const STARTING_CASH = 10_000;

export interface DemoPosition {
  symbol: string;
  qty: number;
  avgEntry: number;
  markPrice: number;
  unrealizedPnl: number;
  notionalUsd: number;
}

export interface DemoTrade {
  id: string;
  symbol: string;
  side: Side;
  qty: number;
  price: number;
  fee: number;
  realizedPnl: number;
  tsMs: number;
  rationale: string;
}

export interface DemoOrderResult {
  approved: boolean;
  reason: string;
  detail: string;
  symbol: string;
  side: Side;
  qty: number;
  price: number;
  notionalUsd: number;
}

export interface DemoState {
  cash: number;
  positions: Record<string, DemoPosition>;
  trades: DemoTrade[];
  peakEquity: number;
  dayStartEquity: number;
  breakerState: BreakerState;
  breakerReason: string | null;
  prices: Record<string, number>;
  /** Rolling equity curve, oldest first. */
  equityHistory: number[];
}

export const EQUITY_HISTORY_POINTS = 240;

export function emptyState(): DemoState {
  return {
    cash: STARTING_CASH,
    positions: {},
    trades: [],
    peakEquity: STARTING_CASH,
    dayStartEquity: STARTING_CASH,
    breakerState: 'closed',
    breakerReason: null,
    prices: {},
    equityHistory: [STARTING_CASH],
  };
}

/** Mark prices for the tracked coins, keyed by trading symbol. */
export function pricesFromCoins(coins: Coin[]): Record<string, number> {
  const prices: Record<string, number> = {};
  for (const coin of coins) {
    prices[`${coin.symbol}/USDT`] = coin.price;
  }
  return prices;
}

export function equityOf(state: DemoState): number {
  let unrealized = 0;
  for (const position of Object.values(state.positions)) {
    unrealized += position.qty * (position.markPrice - position.avgEntry);
  }
  return state.cash + unrealized;
}

/**
 * Margin locked by the open positions.
 *
 * Like the Rust paper venue, this is a USD-margined account: opening a
 * position locks `|qty| * avgEntry` rather than spending it, which is what
 * makes `equity = cash + unrealizedPnl` hold.
 */
export function lockedMargin(state: DemoState): number {
  let total = 0;
  for (const position of Object.values(state.positions)) {
    total += Math.abs(position.qty) * position.avgEntry;
  }
  return total;
}

export function freeCash(state: DemoState): number {
  return state.cash - lockedMargin(state);
}

export function grossExposure(state: DemoState): number {
  let total = 0;
  for (const position of Object.values(state.positions)) {
    total += Math.abs(position.qty * position.markPrice);
  }
  return total;
}

export function drawdownPct(state: DemoState): number {
  const peak = state.peakEquity > 0 ? state.peakEquity : STARTING_CASH;
  const equity = equityOf(state);
  if (peak <= 0) {
    return 0;
  }
  return ((peak - equity) / peak) * 100;
}

/**
 * Apply the risk rules to a candidate order.
 *
 * The checks mirror `rust/crates/core/src/risk.rs` one for one, so the demo
 * refuses exactly what the real engine refuses.
 */
export function checkOrder(
  state: DemoState,
  input: { symbol: string; side: Side; qty: number },
  limits: DemoLimits = DEFAULT_LIMITS,
): { approved: boolean; reason: string; detail: string } {
  if (!(input.qty > 0) || !Number.isFinite(input.qty)) {
    return { approved: false, reason: 'invalid_qty', detail: 'the order size must be a positive number' };
  }

  const price = state.prices[input.symbol];
  if (!price || !Number.isFinite(price)) {
    return { approved: false, reason: 'unknown_symbol', detail: `no market price for ${input.symbol}` };
  }

  if (state.breakerState !== 'closed') {
    return {
      approved: false,
      reason: 'breaker_open',
      detail: `the circuit breaker is ${state.breakerState.replace('_', ' ')}: ${state.breakerReason ?? 'trading is halted'}`,
    };
  }

  const notional = input.qty * price;
  if (notional < limits.min_order_notional_usd) {
    return {
      approved: false,
      reason: 'below_min_notional',
      detail: `$${notional.toFixed(2)} is below the $${limits.min_order_notional_usd} minimum order size`,
    };
  }
  if (notional > limits.max_order_notional_usd) {
    return {
      approved: false,
      reason: 'above_max_notional',
      detail: `$${notional.toFixed(2)} exceeds the $${limits.max_order_notional_usd} single-order cap`,
    };
  }

  const equity = equityOf(state);
  const existing = state.positions[input.symbol];
  const existingQty = existing ? existing.qty : 0;

  if (input.side === 'sell') {
    if (existingQty <= 0) {
      return {
        approved: false,
        reason: 'no_position',
        detail: `there is no open ${input.symbol} position to sell`,
      };
    }
    if (input.qty > existingQty + 1e-9) {
      return {
        approved: false,
        reason: 'reduce_only',
        detail: `selling ${input.qty} would flip the ${input.symbol} position; only the ${existingQty} held can be closed`,
      };
    }
    return { approved: true, reason: 'approved', detail: 'the position can be closed within the risk limits' };
  }

  const available = freeCash(state);
  if (notional > available + 1e-9) {
    return {
      approved: false,
      reason: 'insufficient_cash',
      detail: `$${notional.toFixed(2)} exceeds the $${available.toFixed(2)} of free cash`,
    };
  }

  const maxPositionUsd = (equity * limits.max_position_pct_of_equity) / 100;
  const projectedNotional = (existingQty + input.qty) * price;
  if (projectedNotional > maxPositionUsd + 1e-9) {
    return {
      approved: false,
      reason: 'position_cap',
      detail: `$${projectedNotional.toFixed(2)} would exceed the ${limits.max_position_pct_of_equity}% position cap ($${maxPositionUsd.toFixed(2)})`,
    };
  }

  const maxGross = (equity * limits.max_gross_exposure_pct) / 100;
  const projectedGross = grossExposure(state) + notional;
  if (projectedGross > maxGross + 1e-9) {
    return {
      approved: false,
      reason: 'gross_exposure_cap',
      detail: `$${projectedGross.toFixed(2)} would exceed the ${limits.max_gross_exposure_pct}% gross exposure cap ($${maxGross.toFixed(2)})`,
    };
  }

  const openPositions = Object.values(state.positions).filter((position) => position.qty !== 0).length;
  if (existingQty === 0 && openPositions >= limits.max_open_positions) {
    return {
      approved: false,
      reason: 'max_open_positions',
      detail: `${openPositions} positions are already open; the limit is ${limits.max_open_positions}`,
    };
  }

  return { approved: true, reason: 'approved', detail: 'the order is within every risk limit' };
}

const FEE_RATE = 0.0005;

/** Execute an approved order against the simulated book. */
export function executeOrder(
  state: DemoState,
  input: { symbol: string; side: Side; qty: number; rationale?: string },
): DemoOrderResult {
  const price = state.prices[input.symbol] ?? 0;
  const notionalUsd = input.qty * price;
  const check = checkOrder(state, input);
  if (!check.approved) {
    return {
      approved: false,
      reason: check.reason,
      detail: check.detail,
      symbol: input.symbol,
      side: input.side,
      qty: input.qty,
      price,
      notionalUsd,
    };
  }

  const existing = state.positions[input.symbol];
  const fee = notionalUsd * FEE_RATE;
  let realizedPnl = 0;

  if (input.side === 'buy') {
    const oldQty = existing?.qty ?? 0;
    const oldAvg = existing?.avgEntry ?? 0;
    const newQty = oldQty + input.qty;
    const avgEntry = newQty === 0 ? 0 : (oldQty * oldAvg + input.qty * price) / newQty;
    state.positions[input.symbol] = {
      symbol: input.symbol,
      qty: newQty,
      avgEntry,
      markPrice: price,
      unrealizedPnl: newQty * (price - avgEntry),
      notionalUsd: newQty * price,
    };
    state.cash -= fee;
  } else {
    const position = existing as DemoPosition;
    realizedPnl = input.qty * (price - position.avgEntry);
    const newQty = position.qty - input.qty;
    state.cash += realizedPnl - fee;
    if (Math.abs(newQty) < 1e-9) {
      delete state.positions[input.symbol];
    } else {
      state.positions[input.symbol] = {
        ...position,
        qty: newQty,
        markPrice: price,
        unrealizedPnl: newQty * (price - position.avgEntry),
        notionalUsd: newQty * price,
      };
    }
  }

  state.trades.unshift({
    id: `demo-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
    symbol: input.symbol,
    side: input.side,
    qty: input.qty,
    price,
    fee,
    realizedPnl,
    tsMs: Date.now(),
    rationale: input.rationale ?? 'manual trade from the dashboard',
  });
  state.trades = state.trades.slice(0, 200);

  // Trip the breaker on the same conditions the Rust engine trips it on.
  const equity = equityOf(state);
  state.peakEquity = Math.max(state.peakEquity, equity);
  state.equityHistory.push(equity);
  if (state.equityHistory.length > EQUITY_HISTORY_POINTS) {
    state.equityHistory = state.equityHistory.slice(state.equityHistory.length - EQUITY_HISTORY_POINTS);
  }
  const dailyPnlPct = equity > 0 ? ((equity - state.dayStartEquity) / state.dayStartEquity) * 100 : 0;
  const drawdown = drawdownPct(state);

  if (dailyPnlPct <= -DEFAULT_LIMITS.max_daily_loss_pct) {
    state.breakerState = 'open';
    state.breakerReason = `the daily loss limit of ${DEFAULT_LIMITS.max_daily_loss_pct}% was hit`;
  } else if (drawdown >= DEFAULT_LIMITS.max_drawdown_pct) {
    state.breakerState = 'open';
    state.breakerReason = `the drawdown limit of ${DEFAULT_LIMITS.max_drawdown_pct}% was hit`;
  }

  return {
    approved: true,
    reason: 'approved',
    detail: `filled ${input.qty} ${input.symbol} at $${price.toFixed(2)}`,
    symbol: input.symbol,
    side: input.side,
    qty: input.qty,
    price,
    notionalUsd,
  };
}

/** Mark positions to the latest prices. */
export function markToMarket(state: DemoState, prices: Record<string, number>): void {
  for (const [symbol, price] of Object.entries(prices)) {
    state.prices[symbol] = price;
  }
  for (const position of Object.values(state.positions)) {
    const price = state.prices[position.symbol];
    if (!price) {
      continue;
    }
    position.markPrice = price;
    position.unrealizedPnl = position.qty * (price - position.avgEntry);
    position.notionalUsd = position.qty * price;
  }
  const equity = equityOf(state);
  state.peakEquity = Math.max(state.peakEquity, equity);
  state.equityHistory.push(equity);
  if (state.equityHistory.length > EQUITY_HISTORY_POINTS) {
    state.equityHistory = state.equityHistory.slice(state.equityHistory.length - EQUITY_HISTORY_POINTS);
  }
  if (state.breakerState === 'closed') {
    const dailyPnlPct = equity > 0 ? ((equity - state.dayStartEquity) / state.dayStartEquity) * 100 : 0;
    if (dailyPnlPct <= -DEFAULT_LIMITS.max_daily_loss_pct) {
      state.breakerState = 'open';
      state.breakerReason = `the daily loss limit of ${DEFAULT_LIMITS.max_daily_loss_pct}% was hit`;
    } else if (drawdownPct(state) >= DEFAULT_LIMITS.max_drawdown_pct) {
      state.breakerState = 'open';
      state.breakerReason = `the drawdown limit of ${DEFAULT_LIMITS.max_drawdown_pct}% was hit`;
    }
  }
}

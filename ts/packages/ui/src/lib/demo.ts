/**
 * Client-side paper trading.
 *
 * The published dashboard is a static site, so it has no Rust core to talk
 * to. This module simulates the engine in the browser: it fills orders
 * immediately at the quoted price and applies the SAME risk limits the Rust
 * risk layer enforces, so what a visitor sees on the public site is a
 * faithful picture of what the real engine would do.
 *
 * The limits, the order the checks run in and the reason codes all come from
 * the Rust risk layer through `riskRules.ts`, so the two cannot drift apart.
 *
 * Nothing here is real money. There is no exchange credential, no signing and
 * no broadcast path anywhere in this file.
 */

import type { Coin } from './coins';
import {
  DEFAULT_LIMITS,
  PAPER_VENUE,
  RISK_CONTRACT,
  type RiskContract,
  type RuleLimits,
} from './riskRules';

export type Side = 'buy' | 'sell';
export type BreakerState = 'closed' | 'half_open' | 'open';

export { DEFAULT_LIMITS };
export type { RuleLimits as DemoLimits } from './riskRules';

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
 * The checks run in the order the Rust risk layer runs them, and they refuse
 * with the same reason codes. Both come from the contract in `riskRules.ts`,
 * so the demo refuses exactly what the real engine refuses.
 *
 * A refusal is reported with the first check that failed, so the order of the
 * checks is part of the contract.
 */
export function checkOrder(
  state: DemoState,
  input: { symbol: string; side: Side; qty: number },
  contract: RiskContract = RISK_CONTRACT,
): { approved: boolean; reason: string; detail: string } {
  const limits = contract.limits;
  const reject = (reason: string, detail: string) => ({ approved: false, reason, detail });

  if (state.breakerState === 'open') {
    return reject(
      'breaker_open',
      `trading halted: ${state.breakerReason ?? 'unknown'}`,
    );
  }

  if (!Number.isFinite(input.qty) || input.qty <= 0) {
    return reject(
      'invalid_quantity',
      `quantity must be a positive finite number, got ${input.qty}`,
    );
  }

  const price = state.prices[input.symbol];
  if (price === undefined || !Number.isFinite(price) || price <= 0) {
    return reject(
      'invalid_price',
      `price must be a positive finite number, got ${price ?? 'none'}`,
    );
  }

  const equity = equityOf(state);
  if (!Number.isFinite(equity) || equity <= 0) {
    return reject('invalid_equity', 'equity is unknown; refusing to size the order');
  }

  if (!input.symbol || input.symbol.trim() === '') {
    return reject('invalid_symbol', 'symbol must not be empty');
  }

  if (contract.allowlists.symbols.length > 0 && !contract.allowlists.symbols.includes(input.symbol)) {
    return reject('symbol_not_allowed', `${input.symbol} is not on the symbol allowlist`);
  }

  if (contract.allowlists.venues.length > 0 && !contract.allowlists.venues.includes(PAPER_VENUE)) {
    return reject('venue_not_allowed', `${PAPER_VENUE} is not on the venue allowlist`);
  }

  const notional = Math.abs(input.qty) * price;
  if (notional < limits.min_order_notional_usd) {
    return reject(
      'notional_too_small',
      `notional $${notional.toFixed(2)} is below the $${limits.min_order_notional_usd.toFixed(2)} minimum`,
    );
  }
  if (notional > limits.max_order_notional_usd) {
    return reject(
      'notional_too_large',
      `notional $${notional.toFixed(2)} exceeds the $${limits.max_order_notional_usd.toFixed(2)} per-order cap`,
    );
  }

  const existingQty = state.positions[input.symbol]?.qty ?? 0;
  const signedQty = input.side === 'buy' ? Math.abs(input.qty) : -Math.abs(input.qty);
  const resultingQty = existingQty + signedQty;
  const positionCap = (equity * limits.max_position_pct_of_equity) / 100;
  const resultingNotional = Math.abs(resultingQty) * price;

  if (resultingNotional > positionCap) {
    const detail =
      `position would be $${resultingNotional.toFixed(2)} ` +
      `(${((resultingNotional / equity) * 100).toFixed(2)}% of equity), ` +
      `cap is $${positionCap.toFixed(2)} (${limits.max_position_pct_of_equity.toFixed(2)}%)`;
    if (limits.hard_halt_on_oversized_order) {
      // An oversized order halts trading entirely, not just that one order.
      state.breakerState = 'open';
      state.breakerReason = 'position_size_exceeds_max_pct';
      return reject(
        'position_size_exceeds_max_pct',
        `${detail}. Trading halted by the circuit breaker`,
      );
    }
    return reject('position_size_exceeds_max_pct', detail);
  }

  const projectedExposure = grossExposure(state) + notional;
  const exposureCap = (equity * limits.max_gross_exposure_pct) / 100;
  if (projectedExposure > exposureCap) {
    return reject(
      'gross_exposure_exceeded',
      `projected exposure $${projectedExposure.toFixed(2)} exceeds $${exposureCap.toFixed(2)} ` +
        `(${limits.max_gross_exposure_pct.toFixed(2)}% of equity)`,
    );
  }

  const openPositions = Object.values(state.positions).filter(
    (position) => Math.abs(position.qty) >= 0.000001,
  ).length;
  if (Math.abs(existingQty) < 0.000001 && openPositions >= limits.max_open_positions) {
    return reject(
      'max_open_positions',
      `${openPositions} open positions already (limit ${limits.max_open_positions})`,
    );
  }

  if (state.breakerState === 'half_open') {
    // Only a small probe order may re-open the door.
    const probeCap = (equity * (limits.max_position_pct_of_equity / 4)) / 100;
    if (notional > probeCap) {
      return reject(
        'half_open_probe_too_large',
        `half-open breaker allows a probe of at most $${probeCap.toFixed(2)}, got $${notional.toFixed(2)}`,
      );
    }
    return {
      approved: true,
      reason: 'approved',
      detail: `probe order approved while the breaker is half-open ($${notional.toFixed(2)})`,
    };
  }

  // The risk layer is done. What is left is what the venue does with the
  // order it was handed: the simulator is its own venue, so it applies the
  // paper venue's own rules.
  if (input.side === 'sell') {
    if (existingQty <= 0) {
      return reject('no_position', `there is no open ${input.symbol} position to sell`);
    }
    if (input.qty > existingQty + 1e-9) {
      return reject(
        'reduce_only',
        `selling ${input.qty} would flip the ${input.symbol} position; only the ${existingQty} held can be closed`,
      );
    }
    return {
      approved: true,
      reason: 'approved',
      detail: 'the position can be closed within the risk limits',
    };
  }

  const available = freeCash(state);
  if (notional > available + 1e-9) {
    return reject(
      'insufficient_cash',
      `$${notional.toFixed(2)} exceeds the $${available.toFixed(2)} of free cash`,
    );
  }

  return {
    approved: true,
    reason: 'approved',
    detail: `within limits: $${notional.toFixed(2)} order, ${resultingNotional.toFixed(2)} position ` +
      `(${((resultingNotional / equity) * 100).toFixed(2)}% of equity)`,
  };
}

const FEE_RATE = 0.0005;

/** Execute an approved order against the simulated book. */
export function executeOrder(
  state: DemoState,
  input: { symbol: string; side: Side; qty: number; rationale?: string },
  contract: RiskContract = RISK_CONTRACT,
): DemoOrderResult {
  const price = state.prices[input.symbol] ?? 0;
  const notionalUsd = input.qty * price;
  const check = checkOrder(state, input, contract);
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
  tripBreaker(state, contract.limits);

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

/**
 * A fresh book with the latest prices.
 *
 * The price book has to come back with a reset: an empty one refuses every
 * order with `invalid_price` until the next price refresh.
 */
export function freshState(
  prices: Record<string, number>,
  contract: RiskContract = RISK_CONTRACT,
): DemoState {
  const state = emptyState();
  markToMarket(state, prices, contract);
  return state;
}

/** Mark positions to the latest prices. */
export function markToMarket(
  state: DemoState,
  prices: Record<string, number>,
  contract: RiskContract = RISK_CONTRACT,
): void {
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
    tripBreaker(state, contract.limits);
  }
}

/**
 * Open the breaker when the daily loss or the drawdown limit is hit.
 *
 * The limits come from the contract, so the simulator halts on the same
 * numbers the engine halts on.
 */
function tripBreaker(state: DemoState, limits: RuleLimits): void {
  const equity = equityOf(state);
  const dailyPnlPct = equity > 0 ? ((equity - state.dayStartEquity) / state.dayStartEquity) * 100 : 0;
  if (dailyPnlPct <= -limits.max_daily_loss_pct) {
    state.breakerState = 'open';
    state.breakerReason = `the daily loss limit of ${limits.max_daily_loss_pct}% was hit`;
  } else if (drawdownPct(state) >= limits.max_drawdown_pct) {
    state.breakerState = 'open';
    state.breakerReason = `the drawdown limit of ${limits.max_drawdown_pct}% was hit`;
  }
}

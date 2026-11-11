/**
 * Trade aggregation helpers for the private analytics view.
 *
 * Everything in this module is pure: no React, no fetch, no side effects.
 * The functions work on a minimal structural trade shape so the same code
 * pivots over the live core's `TradeRecord[]` and over the browser-side
 * demo engine's `DemoTrade[]` (see `adaptDemoTrades`).
 */

import type { DemoTrade } from './demo';

/**
 * The smallest trade shape the pivot functions need.
 *
 * `TradeRecord` from the core already matches this structurally. `DemoTrade`
 * does not (it uses `tsMs` and `realizedPnl`), so it is adapted first.
 */
export interface PivotableTrade {
  symbol: string;
  venue?: string;
  side: 'buy' | 'sell';
  qty: number;
  price: number;
  fee: number;
  realized_pnl: number;
  ts_ms: number;
  source?: string;
}

/** One aggregated bucket of the pivot table. */
export interface PivotRow {
  key: string;
  trades: number;
  grossPnl: number;
  fees: number;
  netPnl: number;
  wins: number;
  losses: number;
  winRate: number;
  volumeUsd: number;
}

/** A dimension the trades can be grouped by. */
export interface PivotColumn {
  id: 'symbol' | 'venue' | 'side' | 'day' | 'hour' | 'source';
  label: string;
}

/** The dimensions offered by the admin pivot table, in display order. */
export const PIVOT_COLUMNS: PivotColumn[] = [
  { id: 'symbol', label: 'Symbol' },
  { id: 'venue', label: 'Venue' },
  { id: 'side', label: 'Side' },
  { id: 'day', label: 'Day' },
  { id: 'hour', label: 'Hour' },
  { id: 'source', label: 'Source' },
];

/** A large but finite stand-in for an infinite profit factor. */
export const PROFIT_FACTOR_CAP = 99;

interface Bucket {
  trades: number;
  grossPnl: number;
  fees: number;
  wins: number;
  losses: number;
  volumeUsd: number;
}

/** Net PnL of a single fill: realised profit minus the fee that produced it. */
export function netPnlOf(trade: PivotableTrade): number {
  return trade.realized_pnl - trade.fee;
}

function pad2(value: number): string {
  return value < 10 ? `0${value}` : String(value);
}

/** `ts_ms` → the UTC calendar day, `YYYY-MM-DD`. */
export function utcDayOf(tsMs: number): string {
  const date = new Date(tsMs);
  if (Number.isNaN(date.getTime())) {
    return 'unknown';
  }
  return `${date.getUTCFullYear()}-${pad2(date.getUTCMonth() + 1)}-${pad2(date.getUTCDate())}`;
}

/** `ts_ms` → the UTC hour of the day, zero padded (`07`). */
export function utcHourOf(tsMs: number): string {
  const date = new Date(tsMs);
  if (Number.isNaN(date.getTime())) {
    return 'unknown';
  }
  return pad2(date.getUTCHours());
}

function keyFor(trade: PivotableTrade, column: PivotColumn['id']): string {
  switch (column) {
    case 'symbol':
      return trade.symbol || 'unknown';
    case 'venue':
      return trade.venue || 'unknown';
    case 'side':
      return trade.side || 'unknown';
    case 'day':
      return utcDayOf(trade.ts_ms);
    case 'hour':
      return utcHourOf(trade.ts_ms);
    case 'source':
      return trade.source || 'unknown';
    default:
      return 'unknown';
  }
}

/**
 * Group trades by a dimension and aggregate them.
 *
 * A trade counts as a win when its own net PnL (realised PnL minus fee) is
 * positive, so `winRate` is the share of profitable fills inside the bucket.
 * Rows are sorted by net PnL, best first.
 */
export function pivotBy(trades: readonly PivotableTrade[], column: PivotColumn['id']): PivotRow[] {
  const buckets = new Map<string, Bucket>();

  for (const trade of trades) {
    const key = keyFor(trade, column);
    let bucket = buckets.get(key);
    if (!bucket) {
      bucket = { trades: 0, grossPnl: 0, fees: 0, wins: 0, losses: 0, volumeUsd: 0 };
      buckets.set(key, bucket);
    }
    bucket.trades += 1;
    bucket.grossPnl += trade.realized_pnl;
    bucket.fees += trade.fee;
    bucket.volumeUsd += trade.qty * trade.price;
    const net = netPnlOf(trade);
    if (net > 0) {
      bucket.wins += 1;
    } else if (net < 0) {
      bucket.losses += 1;
    }
  }

  const rows: PivotRow[] = [];
  for (const [key, bucket] of buckets) {
    rows.push({
      key,
      trades: bucket.trades,
      grossPnl: bucket.grossPnl,
      fees: bucket.fees,
      netPnl: bucket.grossPnl - bucket.fees,
      wins: bucket.wins,
      losses: bucket.losses,
      winRate: bucket.trades > 0 ? bucket.wins / bucket.trades : 0,
      volumeUsd: bucket.volumeUsd,
    });
  }

  rows.sort((a, b) => b.netPnl - a.netPnl || a.key.localeCompare(b.key));
  return rows;
}

/**
 * Running net PnL, oldest trade first.
 *
 * The trade feeds arrive newest-first, so the list is reversed before the
 * running total is accumulated.
 */
export function cumulativePnl(trades: readonly PivotableTrade[]): number[] {
  const series: number[] = [];
  let running = 0;
  for (let index = trades.length - 1; index >= 0; index -= 1) {
    running += netPnlOf(trades[index]);
    series.push(running);
  }
  return series;
}

/** The cumulative net PnL expressed as an equity curve. */
export function equitySeriesFromTrades(trades: readonly PivotableTrade[], startingEquity: number): number[] {
  const base = Number.isFinite(startingEquity) ? startingEquity : 0;
  return cumulativePnl(trades).map((value) => base + value);
}

/** Aggregate statistics over a set of trades. */
export interface TradeSummary<T extends PivotableTrade = PivotableTrade> {
  totalTrades: number;
  grossPnl: number;
  fees: number;
  netPnl: number;
  winRate: number;
  bestTrade: T | null;
  worstTrade: T | null;
  avgWin: number;
  avgLoss: number;
  profitFactor: number;
}

/**
 * Headline statistics for a set of trades.
 *
 * `bestTrade` / `worstTrade` are the fills with the highest and lowest net
 * PnL. `profitFactor` is gross profit over gross loss, capped at
 * {@link PROFIT_FACTOR_CAP} when there is nothing to lose against.
 */
export function summariseTrades<T extends PivotableTrade = PivotableTrade>(
  trades: readonly T[],
): TradeSummary<T> {
  let grossPnl = 0;
  let fees = 0;
  let wins = 0;
  let losses = 0;
  let grossProfit = 0;
  let grossLoss = 0;
  let winSum = 0;
  let lossSum = 0;
  let best: T | null = null;
  let worst: T | null = null;

  for (const trade of trades) {
    const net = netPnlOf(trade);
    grossPnl += trade.realized_pnl;
    fees += trade.fee;

    if (net > 0) {
      wins += 1;
      grossProfit += net;
      winSum += net;
    } else if (net < 0) {
      losses += 1;
      grossLoss += -net;
      lossSum += net;
    }

    if (!best || net > netPnlOf(best)) {
      best = trade;
    }
    if (!worst || net < netPnlOf(worst)) {
      worst = trade;
    }
  }

  const totalTrades = trades.length;
  const profitFactor =
    grossLoss > 0 ? grossProfit / grossLoss : grossProfit > 0 ? PROFIT_FACTOR_CAP : 0;

  return {
    totalTrades,
    grossPnl,
    fees,
    netPnl: grossPnl - fees,
    winRate: totalTrades > 0 ? wins / totalTrades : 0,
    bestTrade: best,
    worstTrade: worst,
    avgWin: wins > 0 ? winSum / wins : 0,
    avgLoss: losses > 0 ? lossSum / losses : 0,
    profitFactor,
  };
}

/**
 * Map the demo engine's trades onto the pivotable shape.
 *
 * The demo engine has no venue and no source, so both are labelled
 * explicitly instead of being left blank: `demo` for the venue and
 * `simulation` for the source.
 */
export function adaptDemoTrades(trades: readonly DemoTrade[]): PivotableTrade[] {
  return trades.map((trade) => ({
    symbol: trade.symbol,
    venue: 'demo',
    side: trade.side,
    qty: trade.qty,
    price: trade.price,
    fee: trade.fee,
    realized_pnl: trade.realizedPnl,
    ts_ms: trade.tsMs,
    source: 'simulation',
  }));
}

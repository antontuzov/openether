import { describe, expect, it } from 'vitest';
import type { OrderFill, TrackedOrder } from '../src/api/types';
import { avgFillPrice, filledQty, isActive, remainingQty, totalFee } from '../src/lib/orders';

function fill(qty: number, price: number, fee = 0, tsMs = 1_731_200_000_000): OrderFill {
  return {
    venue: 'paper',
    order_id: 'order-1',
    symbol: 'SOL/USDT',
    side: 'buy',
    qty,
    price,
    fee,
    ts_ms: tsMs,
  };
}

function order(overrides: Partial<TrackedOrder> = {}): TrackedOrder {
  return {
    venue: 'paper',
    order_id: 'order-1',
    client_id: 'client-1',
    symbol: 'SOL/USDT',
    side: 'buy',
    order_type: 'limit',
    limit_price: 130,
    trigger_price: null,
    oco_group: null,
    qty: 2,
    status: 'accepted',
    rationale: '',
    submitted_ts_ms: 1_731_200_000_000,
    updated_ts_ms: 1_731_200_000_000,
    fills: [],
    missing_since_ms: null,
    ...overrides,
  };
}

describe('a resting order', () => {
  it('has filled nothing, so it is not a position', () => {
    const resting = order();
    expect(filledQty(resting)).toBe(0);
    expect(remainingQty(resting)).toBe(2);
    expect(avgFillPrice(resting)).toBe(0);
    expect(totalFee(resting)).toBe(0);
    expect(isActive(resting)).toBe(true);
  });

  it('is still active when the venue has not reported it for a while', () => {
    // Absence from the open list is not cancellation: trade history lags it.
    expect(isActive(order({ missing_since_ms: 1_731_200_000_000 }))).toBe(true);
  });
});

describe('a partial fill', () => {
  const partial = order({ status: 'partially_filled', fills: [fill(1, 100, 0.05)] });

  it('books only the confirmed part', () => {
    expect(filledQty(partial)).toBe(1);
    expect(remainingQty(partial)).toBe(1);
    expect(isActive(partial)).toBe(true);
  });

  it('carries the venue fee', () => {
    expect(totalFee(partial)).toBeCloseTo(0.05, 10);
  });
});

describe('a completed order', () => {
  const done = order({
    status: 'filled',
    fills: [fill(1, 100, 0.05), fill(1, 102, 0.06, 1_731_200_000_001)],
  });

  it('averages the fills by quantity, not by the last price', () => {
    expect(avgFillPrice(done)).toBeCloseTo(101, 10);
  });

  it('has nothing left resting and is no longer active', () => {
    expect(filledQty(done)).toBe(2);
    expect(remainingQty(done)).toBe(0);
    expect(isActive(done)).toBe(false);
    expect(totalFee(done)).toBeCloseTo(0.11, 10);
  });
});

describe('a retired order', () => {
  it('is not active once cancelled or rejected', () => {
    expect(isActive(order({ status: 'cancelled' }))).toBe(false);
    expect(isActive(order({ status: 'rejected' }))).toBe(false);
  });

  it('still reports the part that filled before it was cancelled', () => {
    const cancelled = order({ status: 'cancelled', fills: [fill(0.5, 100)] });
    expect(filledQty(cancelled)).toBeCloseTo(0.5, 10);
    expect(remainingQty(cancelled)).toBeCloseTo(1.5, 10);
  });
});
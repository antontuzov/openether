import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it } from 'vitest';
import type { Position } from '../src/api/types';
import { PositionsTable } from '../src/components/PositionsTable';

function position(overrides: Partial<Position> = {}): Position {
  return {
    venue: 'binance',
    symbol: 'SOL/USDT',
    qty: 2,
    avg_entry: 130,
    mark_price: 141,
    unrealized_pnl: 21.79,
    notional_usd: 282,
    ts_ms: 1_731_200_000_000,
    ...overrides,
  };
}

describe('the positions table', () => {
  it('names the venue that holds a position once several are configured', () => {
    const html = renderToStaticMarkup(
      createElement(PositionsTable, { positions: [position()], showVenue: true }),
    );
    expect(html).toContain('class="venue-tag"');
    expect(html).toContain('binance');
  });

  it('leaves the venue off a single-book dashboard', () => {
    const html = renderToStaticMarkup(createElement(PositionsTable, { positions: [position()] }));
    expect(html).not.toContain('venue-tag');
  });
});

import { useMemo, useRef } from 'react';
import type { MarketTick } from '../api/types';
import { fmtPrice, fmtQty } from '../lib/format';

interface TickerStripProps {
  ticks: MarketTick[];
}

interface StripItem {
  key: string;
  symbol: string;
  price: number;
  size: number;
  dir: 'up' | 'down' | null;
}

const WINDOW = 40;

/**
 * Horizontally scrolling tape of the most recent ticks.
 *
 * The direction is decided the first time a tick is seen (compared against the
 * previous price of the same symbol) and cached, so the colour flash plays
 * exactly once per new tick instead of flickering on every re-render.
 */
export function TickerStrip({ ticks }: TickerStripProps) {
  const lastPrice = useRef(new Map<string, number>());
  const directions = useRef(new Map<string, 'up' | 'down' | null>());
  const identities = useRef(new Map<string, number>());
  const nextIdentity = useRef(0);

  const items = useMemo<StripItem[]>(() => {
    const window = ticks.slice(-WINDOW);
    const live = new Set<string>();
    const result: StripItem[] = [];

    for (const tick of window) {
      const idKey = `${tick.symbol}|${tick.ts_ms}|${tick.price}|${tick.size}`;
      live.add(idKey);

      let identity = identities.current.get(idKey);
      if (identity === undefined) {
        identity = nextIdentity.current;
        nextIdentity.current += 1;
        identities.current.set(idKey, identity);
      }

      let direction = directions.current.get(idKey);
      if (direction === undefined) {
        const previous = lastPrice.current.get(tick.symbol);
        direction =
          previous === undefined || previous === tick.price
            ? null
            : tick.price > previous
              ? 'up'
              : 'down';
        directions.current.set(idKey, direction);
      }
      lastPrice.current.set(tick.symbol, tick.price);

      result.push({
        key: `${idKey}#${identity}`,
        symbol: tick.symbol,
        price: tick.price,
        size: tick.size,
        dir: direction,
      });
    }

    // Keep the caches bounded: the window only ever slides forward.
    for (const key of directions.current.keys()) {
      if (!live.has(key)) directions.current.delete(key);
    }
    for (const key of identities.current.keys()) {
      if (!live.has(key)) identities.current.delete(key);
    }

    return result;
  }, [ticks]);

  const duration = Math.max(16, items.length * 1.8);

  return (
    <div className="strip" role="marquee" aria-label="Live market ticks">
      <div className="strip__label">tape</div>
      {items.length === 0 ? (
        <div className="strip__idle">waiting for market data…</div>
      ) : (
        <div className="strip__viewport">
          <div className="strip__track" style={{ animationDuration: `${duration}s` }}>
            {[0, 1].map((copy) => (
              <div className="strip__run" key={copy} aria-hidden={copy === 1}>
                {items.map((item) => (
                  <span
                    className={`strip__item${item.dir ? ` strip__item--${item.dir}` : ''}`}
                    key={`${copy}-${item.key}`}
                  >
                    <span className="strip__sym">{item.symbol}</span>
                    <span className="strip__px num">{fmtPrice(item.price)}</span>
                    <span className="strip__sz num">{fmtQty(item.size)}</span>
                  </span>
                ))}
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import type { Coin } from '../lib/coins';
import { fetchTopCoins, symbolForCoin } from '../lib/coins';
import {
  DEFAULT_LIMITS,
  checkOrder,
  emptyState,
  equityOf,
  executeOrder,
  grossExposure,
  markToMarket,
  pricesFromCoins,
  type DemoPosition,
  type DemoState,
  type DemoTrade,
} from '../lib/demo';

const STORAGE_KEY = 'openether.demo.v2';

export interface DemoEngine {
  state: DemoState;
  coins: Coin[];
  market: Array<{ symbol: string; price: number; name: string }>;
  loading: boolean;
  error: string | null;
  equity: number;
  exposure: number;
  positions: DemoPosition[];
  trades: DemoTrade[];
  /** Quote the risk rules for an order without executing it. */
  preview: (symbol: string, side: 'buy' | 'sell', qty: number) => { approved: boolean; reason: string; detail: string };
  submit: (order: { symbol: string; side: 'buy' | 'sell'; qty: number; rationale: string }) => {
    approved: boolean;
    reason: string;
    detail: string;
  };
  reset: () => void;
  refresh: () => void;
}

function loadPersisted(): DemoState | null {
  try {
    const raw = window.localStorage.getItem(STORAGE_KEY);
    if (!raw) {
      return null;
    }
    const parsed = JSON.parse(raw) as DemoState;
    if (!parsed || typeof parsed.cash !== 'number' || !parsed.positions) {
      return null;
    }
    return parsed;
  } catch {
    return null;
  }
}

function persist(state: DemoState): void {
  try {
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(state));
  } catch {
    // A full or blocked storage quota must not break trading.
  }
}

function cloneState(state: DemoState): DemoState {
  return {
    ...state,
    positions: { ...state.positions },
    trades: [...state.trades],
    prices: { ...state.prices },
    equityHistory: [...state.equityHistory],
  };
}

/**
 * Drives the browser-side paper trading engine.
 *
 * The engine keeps the positions and the trade history in localStorage, so a
 * visitor who reloads the page keeps their simulated book. Prices come from
 * the free CoinGecko endpoint and are refreshed on a timer.
 */
export function useDemoEngine(enabled: boolean): DemoEngine {
  const [state, setState] = useState<DemoState>(() => loadPersisted() ?? emptyState());
  const [coins, setCoins] = useState<Coin[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const stateRef = useRef(state);
  stateRef.current = state;

  const refresh = useCallback(() => {
    if (!enabled) {
      return;
    }
    setLoading(true);
    void fetchTopCoins(50, true)
      .then((list) => {
        setCoins(list);
        setError(list.length === 0 ? 'the market data service is unavailable right now' : null);
      })
      .catch((cause: unknown) => {
        setError(cause instanceof Error ? cause.message : 'the market data service is unavailable');
      })
      .finally(() => setLoading(false));
  }, [enabled]);

  useEffect(() => {
    if (!enabled) {
      return;
    }
    void fetchTopCoins(50).then((list) => {
      setCoins(list);
      if (list.length === 0) {
        setError('the market data service is unavailable right now');
      }
    });
    const timer = window.setInterval(refresh, 60_000);
    return () => window.clearInterval(timer);
  }, [enabled, refresh]);

  // Mark the book to the latest prices whenever they change.
  useEffect(() => {
    if (!enabled || coins.length === 0) {
      return;
    }
    const next = cloneState(stateRef.current);
    markToMarket(next, pricesFromCoins(coins));
    stateRef.current = next;
    setState(next);
  }, [coins, enabled]);

  const market = useMemo(
    () =>
      coins.map((coin) => ({
        symbol: symbolForCoin(coin),
        price: coin.price,
        name: `${coin.name} · ${coin.symbol}`,
      })),
    [coins],
  );

  const preview = useCallback((symbol: string, side: 'buy' | 'sell', qty: number) => {
    return checkOrder(stateRef.current, { symbol, side, qty }, DEFAULT_LIMITS);
  }, []);

  const submit = useCallback(
    (order: { symbol: string; side: 'buy' | 'sell'; qty: number; rationale: string }) => {
      const next = cloneState(stateRef.current);
      const result = executeOrder(next, order);
      stateRef.current = next;
      setState(next);
      if (result.approved) {
        persist(next);
      }
      return { approved: result.approved, reason: result.reason, detail: result.detail };
    },
    [],
  );

  const reset = useCallback(() => {
    const fresh = emptyState();
    stateRef.current = fresh;
    setState(fresh);
    try {
      window.localStorage.removeItem(STORAGE_KEY);
    } catch {
      // ignore
    }
  }, []);

  return {
    state,
    coins,
    market,
    loading,
    error,
    equity: equityOf(state),
    exposure: grossExposure(state),
    positions: Object.values(state.positions),
    trades: state.trades,
    preview,
    submit,
    reset,
    refresh,
  };
}

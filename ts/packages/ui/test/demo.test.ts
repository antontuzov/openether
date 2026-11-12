import { describe, expect, it } from 'vitest';
import {
  EQUITY_HISTORY_POINTS,
  STARTING_CASH,
  checkOrder,
  emptyState,
  equityOf,
  executeOrder,
  freeCash,
  freshState,
  lockedMargin,
  markToMarket,
  type DemoState,
} from '../src/lib/demo';
import { DEFAULT_LIMITS, RISK_CONTRACT, toBrowserContract, type RiskContract } from '../src/lib/riskRules';

const SOL = 'SOL/USDT';
const BTC = 'BTC/USDT';

function priced(): DemoState {
  const state = emptyState();
  state.prices = { [SOL]: 100, [BTC]: 50_000 };
  return state;
}

describe('accounting', () => {
  it('opens a position by locking margin, not by spending cash', () => {
    const state = priced();
    const result = executeOrder(state, { symbol: SOL, side: 'buy', qty: 10 });

    expect(result.approved).toBe(true);
    // 10 * 100 = 1000 notional, 5 bps fee.
    expect(state.cash).toBeCloseTo(STARTING_CASH - 0.5, 6);
    expect(lockedMargin(state)).toBeCloseTo(1000, 6);
    expect(freeCash(state)).toBeCloseTo(STARTING_CASH - 0.5 - 1000, 6);
  });

  it('loses only the fee on entry, not the notional', () => {
    const state = priced();
    executeOrder(state, { symbol: SOL, side: 'buy', qty: 10 });

    expect(equityOf(state)).toBeCloseTo(STARTING_CASH - 0.5, 6);
    expect(state.breakerState).toBe('closed');
  });

  it('returns the margin and settles realised PnL on a round trip', () => {
    const state = priced();
    executeOrder(state, { symbol: SOL, side: 'buy', qty: 10 });

    state.prices[SOL] = 110;
    markToMarket(state, { [SOL]: 110 });
    expect(equityOf(state)).toBeCloseTo(STARTING_CASH - 0.5 + 100, 6);

    const close = executeOrder(state, { symbol: SOL, side: 'sell', qty: 10 });
    expect(close.approved).toBe(true);
    // +100 realised, minus 0.5 entry fee and 0.55 exit fee.
    expect(equityOf(state)).toBeCloseTo(STARTING_CASH + 100 - 0.5 - 0.55, 6);
    expect(state.positions[SOL]).toBeUndefined();
    expect(lockedMargin(state)).toBe(0);
    expect(freeCash(state)).toBeCloseTo(equityOf(state), 6);
  });

  it('keeps a flat book unchanged apart from fees', () => {
    const state = priced();
    executeOrder(state, { symbol: SOL, side: 'buy', qty: 5 });
    executeOrder(state, { symbol: SOL, side: 'sell', qty: 5 });

    expect(equityOf(state)).toBeCloseTo(STARTING_CASH - 0.25 - 0.25, 6);
    expect(state.breakerState).toBe('closed');
  });

  it('averages the entry price when adding to a position', () => {
    const state = priced();
    executeOrder(state, { symbol: SOL, side: 'buy', qty: 2 });
    state.prices[SOL] = 200;
    const add = executeOrder(state, { symbol: SOL, side: 'buy', qty: 2 });

    expect(add.approved).toBe(true);
    expect(state.positions[SOL].avgEntry).toBeCloseTo(150, 6);
    expect(state.positions[SOL].qty).toBe(4);
  });
});

describe('risk limits', () => {
  it('rejects a non-positive size', () => {
    const state = priced();
    expect(checkOrder(state, { symbol: SOL, side: 'buy', qty: 0 }).reason).toBe('invalid_quantity');
    expect(checkOrder(state, { symbol: SOL, side: 'buy', qty: -1 }).reason).toBe('invalid_quantity');
    expect(checkOrder(state, { symbol: SOL, side: 'buy', qty: NaN }).reason).toBe('invalid_quantity');
  });

  it('rejects an unpriced symbol', () => {
    const state = priced();
    expect(checkOrder(state, { symbol: 'DOGE/USDT', side: 'buy', qty: 1 }).reason).toBe(
      'invalid_price',
    );
  });

  it('rejects an order above the single-order cap', () => {
    const state = priced();
    const check = checkOrder(state, { symbol: BTC, side: 'buy', qty: 1 });
    expect(check.reason).toBe('notional_too_large');
    expect(check.detail).toContain(String(DEFAULT_LIMITS.max_order_notional_usd));
  });

  it('rejects an order below the minimum notional', () => {
    const state = priced();
    expect(checkOrder(state, { symbol: SOL, side: 'buy', qty: 0.01 }).reason).toBe(
      'notional_too_small',
    );
  });

  it('rejects a buy the account cannot fund', () => {
    // The gross cap is what normally binds first: free cash is always larger
    // than the per-order cap while the book is inside the gross cap. So the
    // venue check only shows up once the caps are widened past it.
    const state = priced();
    state.cash = 1200;
    const wide: RiskContract = {
      ...RISK_CONTRACT,
      limits: { ...RISK_CONTRACT.limits, max_position_pct_of_equity: 200, max_gross_exposure_pct: 200 },
    };
    expect(checkOrder(state, { symbol: SOL, side: 'buy', qty: 15 }, wide).reason).toBe(
      'insufficient_cash',
    );
  });

  it('rejects a buy that breaks the position cap', () => {
    const state = priced();
    executeOrder(state, { symbol: SOL, side: 'buy', qty: 19 });
    // Equity ~9999, so the 20% position cap is ~2000 and 19 SOL already uses 1900.
    expect(checkOrder(state, { symbol: SOL, side: 'buy', qty: 2 }).reason).toBe(
      'position_size_exceeds_max_pct',
    );
  });

  it('halts trading when an order breaks the position cap', () => {
    const state = priced();
    executeOrder(state, { symbol: SOL, side: 'buy', qty: 19 });
    checkOrder(state, { symbol: SOL, side: 'buy', qty: 2 });
    expect(state.breakerState).toBe('open');
    expect(state.breakerReason).toBe('position_size_exceeds_max_pct');
  });

  it('rejects a sell with no position', () => {
    const state = priced();
    expect(checkOrder(state, { symbol: SOL, side: 'sell', qty: 1 }).reason).toBe('no_position');
  });

  it('rejects a sell that would flip the position', () => {
    const state = priced();
    executeOrder(state, { symbol: SOL, side: 'buy', qty: 5 });
    expect(checkOrder(state, { symbol: SOL, side: 'sell', qty: 6 }).reason).toBe('reduce_only');
  });

  it('rejects a new symbol once the open-position limit is reached', () => {
    const state = emptyState();
    for (let i = 0; i < DEFAULT_LIMITS.max_open_positions; i += 1) {
      state.prices[`C${i}/USDT`] = 100;
    }
    state.prices['EXTRA/USDT'] = 100;
    for (let i = 0; i < DEFAULT_LIMITS.max_open_positions; i += 1) {
      expect(executeOrder(state, { symbol: `C${i}/USDT`, side: 'buy', qty: 1 }).approved).toBe(true);
    }
    expect(checkOrder(state, { symbol: 'EXTRA/USDT', side: 'buy', qty: 1 }).reason).toBe(
      'max_open_positions',
    );
  });

  it('caps gross exposure across symbols', () => {
    const state = emptyState();
    // Four 1500 positions sit exactly on the 60% gross cap of a 10k equity,
    // each still under the 20% single-position cap.
    for (let i = 0; i < 4; i += 1) {
      const symbol = `C${i}/USDT`;
      state.prices[symbol] = 100;
      state.positions[symbol] = {
        symbol,
        qty: 15,
        avgEntry: 100,
        markPrice: 100,
        unrealizedPnl: 0,
        notionalUsd: 1500,
      };
    }
    expect(equityOf(state)).toBe(STARTING_CASH);
    expect(checkOrder(state, { symbol: 'C0/USDT', side: 'buy', qty: 1 }).reason).toBe(
      'gross_exposure_exceeded',
    );
  });

  it('refuses a symbol that is not on the allowlist', () => {
    const state = priced();
    const restricted: RiskContract = {
      ...RISK_CONTRACT,
      allowlists: { ...RISK_CONTRACT.allowlists, symbols: [SOL] },
    };
    expect(checkOrder(state, { symbol: BTC, side: 'buy', qty: 1 }, restricted).reason).toBe(
      'symbol_not_allowed',
    );
    expect(checkOrder(state, { symbol: SOL, side: 'buy', qty: 1 }, restricted).approved).toBe(true);
  });

  it('refuses a venue that is not on the allowlist', () => {
    const state = priced();
    const restricted: RiskContract = {
      ...RISK_CONTRACT,
      allowlists: { ...RISK_CONTRACT.allowlists, venues: ['binance'] },
    };
    expect(checkOrder(state, { symbol: SOL, side: 'buy', qty: 1 }, restricted).reason).toBe(
      'venue_not_allowed',
    );
  });

  it('refuses to size an order when the equity is unknown', () => {
    const state = priced();
    state.cash = 0;
    expect(checkOrder(state, { symbol: SOL, side: 'buy', qty: 1 }).reason).toBe('invalid_equity');
  });

  it('allows only a probe order while the breaker is half open', () => {
    const state = priced();
    state.breakerState = 'half_open';
    // The probe cap is a quarter of the position cap: 20% / 4 of a 10k equity
    // is $500, so 6 SOL at $100 is too big and 1 SOL is not.
    expect(checkOrder(state, { symbol: SOL, side: 'buy', qty: 6 }).reason).toBe(
      'half_open_probe_too_large',
    );
    expect(checkOrder(state, { symbol: SOL, side: 'buy', qty: 1 }).approved).toBe(true);
  });
});

describe('the risk contract', () => {
  it('only reports reasons the engine knows', () => {
    const state = priced();
    const reasons = [
      checkOrder(state, { symbol: SOL, side: 'buy', qty: 0 }),
      checkOrder(state, { symbol: 'DOGE/USDT', side: 'buy', qty: 1 }),
      checkOrder(state, { symbol: BTC, side: 'buy', qty: 1 }),
      checkOrder(state, { symbol: SOL, side: 'buy', qty: 0.01 }),
      checkOrder(state, { symbol: SOL, side: 'sell', qty: 1 }),
    ].map((check) => check.reason);

    for (const reason of reasons) {
      expect(RISK_CONTRACT.reasons).toContain(reason);
    }
  });

  it('runs the checks in the order the engine runs them', () => {
    // An open breaker wins over everything else, exactly like the engine.
    const state = priced();
    state.breakerState = 'open';
    expect(checkOrder(state, { symbol: SOL, side: 'buy', qty: 0 }).reason).toBe('breaker_open');
  });

  it('keeps the engine limits but not its venue allowlists in the browser', () => {
    const engine: RiskContract = {
      ...RISK_CONTRACT,
      limits: { ...RISK_CONTRACT.limits, max_order_notional_usd: 750 },
      allowlists: { symbols: [SOL], venues: ['binance'], contracts: [] },
    };
    const browser = toBrowserContract(engine);

    expect(browser.limits).toEqual(engine.limits);
    expect(browser.check_order).toEqual(engine.check_order);
    expect(browser.reasons).toEqual(engine.reasons);
    expect(browser.allowlists).toEqual({ symbols: [], venues: [], contracts: [] });
  });

  it('trades every market the feed prices, but under the engine money limits', () => {
    const engine: RiskContract = {
      ...RISK_CONTRACT,
      limits: { ...RISK_CONTRACT.limits, max_order_notional_usd: 750 },
      allowlists: { symbols: [SOL], venues: [], contracts: [] },
    };
    const browser = toBrowserContract(engine);
    const state = priced();
    state.prices['DOGE/USDT'] = 0.5;

    // A symbol outside the engine's allowlist is tradeable in the simulator.
    expect(checkOrder(state, { symbol: 'DOGE/USDT', side: 'buy', qty: 100 }, browser).approved).toBe(
      true,
    );
    // The engine's own cap still applies.
    expect(checkOrder(state, { symbol: 'DOGE/USDT', side: 'buy', qty: 2_000 }, browser).reason).toBe(
      'notional_too_large',
    );
  });

  it('brings the price book back when the simulation is reset', () => {
    const state = freshState({ [SOL]: 100 });
    expect(state.prices[SOL]).toBe(100);
    expect(checkOrder(state, { symbol: SOL, side: 'buy', qty: 1 }).approved).toBe(true);
  });
});

describe('circuit breaker', () => {
  it('halts every order once open', () => {
    const state = priced();
    state.breakerState = 'open';
    state.breakerReason = 'the drawdown limit of 10% was hit';

    const check = checkOrder(state, { symbol: SOL, side: 'buy', qty: 1 });
    expect(check.approved).toBe(false);
    expect(check.reason).toBe('breaker_open');

    const before = state.trades.length;
    expect(executeOrder(state, { symbol: SOL, side: 'buy', qty: 1 }).approved).toBe(false);
    expect(state.trades).toHaveLength(before);
  });

  it('trips on a drawdown past the limit', () => {
    const state = priced();
    // The peak is above the day's opening equity, so the day is still up while
    // the drawdown from the peak is already past the limit.
    state.peakEquity = 12_000;
    state.dayStartEquity = 10_000;
    state.cash = 10_500;
    markToMarket(state, { [SOL]: 100 });

    expect(state.breakerState).toBe('open');
    expect(state.breakerReason).toContain('drawdown');
  });

  it('trips on the daily loss limit', () => {
    const state = priced();
    executeOrder(state, { symbol: SOL, side: 'buy', qty: 19 });
    markToMarket(state, { [SOL]: 20 });

    expect(state.breakerState).toBe('open');
    expect(state.breakerReason).toContain('daily loss');
  });

  it('stays closed on ordinary price noise', () => {
    const state = priced();
    executeOrder(state, { symbol: SOL, side: 'buy', qty: 10 });
    markToMarket(state, { [SOL]: 99 });
    expect(state.breakerState).toBe('closed');
  });
});

describe('equity history', () => {
  it('is bounded and keeps the newest samples', () => {
    const state = priced();
    for (let i = 0; i < EQUITY_HISTORY_POINTS + 50; i += 1) {
      markToMarket(state, { [SOL]: 100 + (i % 5) });
    }
    expect(state.equityHistory).toHaveLength(EQUITY_HISTORY_POINTS);
    expect(state.equityHistory.at(-1)).toBeCloseTo(equityOf(state), 6);
  });
});
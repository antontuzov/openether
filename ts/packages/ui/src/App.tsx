import { useCallback, useEffect, useMemo, useState } from 'react';
import * as api from './api/client';
import type { StrategyReloadResponse } from './api/types';
import { AdminPanel } from './components/AdminPanel';
import { CoinTable } from './components/CoinTable';
import { Header } from './components/Header';
import { LogPanel } from './components/LogPanel';
import { PortfolioPanel } from './components/PortfolioPanel';
import { PositionsTable } from './components/PositionsTable';
import { RiskPanel } from './components/RiskPanel';
import type { ActionOutcome } from './components/StrategyPanel';
import { StrategyPanel } from './components/StrategyPanel';
import { TickerStrip } from './components/TickerStrip';
import { TradeTicket } from './components/TradeTicket';
import { TradesTable } from './components/TradesTable';
import { WalletPanel } from './components/WalletPanel';
import { useDashboard } from './hooks/useDashboard';
import { useDemoEngine } from './hooks/useDemoEngine';
import { useTheme } from './hooks/useTheme';
import { demoToPortfolio, demoToPositions, demoToRisk, demoToTrades } from './lib/demoView';
import { fmtAgo, shortHash } from './lib/format';

type View = 'core' | 'sim';

const VIEWS: Array<{ id: View; label: string; hint: string }> = [
  { id: 'core', label: 'core', hint: 'the live Rust engine' },
  { id: 'sim', label: 'simulator', hint: 'paper trading in your browser' },
];

function summarizeReload(response: StrategyReloadResponse | null): string {
  if (!response) return 'strategy reloaded';
  if (response.error) return `reload rejected: ${response.error}`;
  if (response.name) {
    const sha = response.wasm_sha256 ? ` · sha ${shortHash(response.wasm_sha256)}` : '';
    const ticks = typeof response.ticks_processed === 'number' ? ` · ${response.ticks_processed} ticks` : '';
    return `loaded ${response.name}${sha}${ticks}`;
  }
  if (response.message) return response.message;
  const serialised = JSON.stringify(response);
  return serialised && serialised !== '{}' ? serialised : 'strategy reloaded';
}

export default function App() {
  const { theme, toggle } = useTheme();
  const dash = useDashboard();
  // The simulator runs the engine's own money rules when a core is reachable,
  // and the generated ones when it is not.
  const demo = useDemoEngine(true, dash.simulatorContract);

  const [view, setView] = useState<View>('core');
  const [picked, setPicked] = useState(false);
  const [adminOpen, setAdminOpen] = useState(false);
  const [ticketSymbol, setTicketSymbol] = useState<string | null>(null);
  const [outcome, setOutcome] = useState<{ approved: boolean; reason: string; detail: string } | null>(null);

  // The published site has no Rust core behind it, so fall back to the
  // simulator rather than showing an empty dashboard.
  useEffect(() => {
    if (!picked && dash.booted && !dash.coreReachable) {
      setView('sim');
    }
  }, [dash.booted, dash.coreReachable, picked]);

  const selectView = useCallback((next: View) => {
    setPicked(true);
    setView(next);
  }, []);

  const demoPortfolio = useMemo(() => demoToPortfolio(demo.state), [demo.state]);
  const demoPositions = useMemo(() => demoToPositions(demo.state), [demo.state]);
  const demoTrades = useMemo(() => demoToTrades(demo.state), [demo.state]);
  const demoRisk = useMemo(
    () => demoToRisk(demo.state, dash.simulatorContract.limits),
    [demo.state, dash.simulatorContract],
  );

  const simTicks = useMemo(
    () =>
      demo.coins.map((coin) => ({
        venue: 'paper' as const,
        symbol: `${coin.symbol}/USDT`,
        price: coin.price,
        size: 0,
        ts_ms: Date.now(),
      })),
    [demo.coins],
  );

  const venueName = dash.portfolio?.venue ?? dash.venues[0]?.venue ?? 'paper';
  const liveVenue = dash.venues.find((venue) => venue.live);
  const inSim = view === 'sim';

  const handleReload = useCallback(
    async (path: string): Promise<ActionOutcome> => {
      try {
        const response = await api.reloadStrategy(path);
        await dash.refreshStrategy();
        return { ok: true, message: summarizeReload(response) };
      } catch (error) {
        return { ok: false, message: error instanceof Error ? error.message : String(error) };
      }
    },
    [dash],
  );

  const handleResetBreaker = useCallback(async (): Promise<ActionOutcome> => {
    try {
      const response = await api.resetBreaker();
      await dash.refreshRisk();
      const safe = response ?? {};
      const state = safe.breaker_state ? ` · breaker now ${safe.breaker_state.replace('_', ' ')}` : '';
      const message =
        safe.message ?? (safe.ok === false ? 'reset rejected by the risk layer' : 'breaker reset requested');
      return { ok: safe.ok !== false, message: `${message}${state}` };
    } catch (error) {
      return { ok: false, message: error instanceof Error ? error.message : String(error) };
    }
  }, [dash]);

  const handleDemoSubmit = useCallback(
    (order: { symbol: string; side: 'buy' | 'sell'; qty: number; rationale: string }) => {
      setOutcome(demo.submit(order));
    },
    [demo],
  );

  const header = {
    venueLabel: inSim
      ? 'SIMULATION'
      : liveVenue
        ? `LIVE · ${liveVenue.venue.toUpperCase()}`
        : venueName.toUpperCase(),
    venueLive: !inSim && Boolean(liveVenue),
    connection: inSim ? ('sim' as const) : dash.connection,
    equity: inSim ? demoPortfolio.equity_usd : dash.portfolio?.equity_usd ?? null,
    dayPnl: inSim ? demoRisk.daily_pnl_usd : dash.risk?.daily_pnl_usd ?? null,
    breakerState: inSim ? demo.state.breakerState : dash.risk?.breaker_state ?? null,
    engineState: inSim ? 'simulated' : dash.engineStatus?.state ?? null,
    theme,
    onToggleTheme: toggle,
  };

  // The admin panel pivots the live engine's trades when the core is
  // connected, and the browser simulation otherwise.
  const liveTrades = dash.trades;
  const adminPositions = liveTrades.length > 0 ? dash.positions : demoPositions;
  const adminPortfolio = liveTrades.length > 0 ? dash.portfolio : demoPortfolio;
  const adminRisk = liveTrades.length > 0 ? dash.risk : demoRisk;

  return (
    <div className="app">
      <Header {...header} />

      <nav className="tabs" aria-label="Dashboard views">
        {VIEWS.map((entry) => (
          <button
            key={entry.id}
            type="button"
            className={`tabs__btn ${view === entry.id ? 'is-active' : ''}`}
            aria-current={view === entry.id ? 'page' : undefined}
            title={entry.hint}
            onClick={() => selectView(entry.id)}
          >
            {entry.label}
          </button>
        ))}
        {view === 'sim' ? (
          <button type="button" className="btn btn--tiny tabs__reset" onClick={demo.reset}>
            reset simulation
          </button>
        ) : null}
        <button
          type="button"
          className="btn btn--tiny tabs__admin"
          onClick={() => setAdminOpen(true)}
          title="Private analytics"
        >
          admin
        </button>
      </nav>

      {view === 'core' ? (
        <>
          <TickerStrip ticks={dash.ticks} />

          {!dash.booted ? (
            <div className="banner banner--wait" role="status">
              <span className="banner__spinner" aria-hidden="true" />
              <span className="banner__text">connecting to core…</span>
            </div>
          ) : !dash.coreReachable ? (
            <div className="banner banner--down" role="status">
              <span className="banner__spinner" aria-hidden="true" />
              <span className="banner__text">
                core unreachable — the simulator runs entirely in your browser
              </span>
              {dash.lastError ? <span className="banner__detail">{dash.lastError}</span> : null}
              <button type="button" className="btn btn--tiny" onClick={() => selectView('sim')}>
                open simulator
              </button>
            </div>
          ) : null}

          <main className="dash">
            <div className="dash__col">
              <PortfolioPanel portfolio={dash.portfolio} equitySeries={dash.equitySeries} />
              <WalletPanel wallet={dash.wallet} />
              <PositionsTable positions={dash.positions} />
              <TradesTable trades={dash.trades} />
            </div>

            <div className="dash__col">
              <RiskPanel risk={dash.risk} positions={dash.positions} trades={dash.trades} />
              <StrategyPanel
                strategy={dash.strategy}
                breakerState={dash.risk?.breaker_state ?? null}
                onReload={handleReload}
                onResetBreaker={handleResetBreaker}
              />
              <LogPanel logs={dash.logs} />
            </div>
          </main>
        </>
      ) : null}

      {view === 'sim' ? (
        <>
          <TickerStrip ticks={simTicks} />

          <div className="banner banner--wait banner--sim" role="status">
            <span className="banner__text">
              simulation — {demo.coins.length} markets priced from the free CoinGecko feed. Fills are
              simulated locally: no exchange account, no keys, no real orders.
            </span>
            {demo.error ? <span className="banner__detail">{demo.error}</span> : null}
          </div>

          <main className="dash">
            <div className="dash__col">
              <TradeTicket
                market={demo.market}
                limits={dash.simulatorContract.limits}
                symbol={ticketSymbol ?? undefined}
                onSymbolChange={setTicketSymbol}
                onSubmit={handleDemoSubmit}
                outcome={outcome}
                onDismissOutcome={() => setOutcome(null)}
              />
              <CoinTable
                coins={demo.coins}
                onTrade={(symbol) => setTicketSymbol(symbol)}
                loading={demo.loading}
                error={demo.error}
              />
            </div>

            <div className="dash__col">
              <PortfolioPanel portfolio={demoPortfolio} equitySeries={demo.state.equityHistory} />
              <RiskPanel risk={demoRisk} positions={demoPositions} trades={demoTrades} />
              <PositionsTable positions={demoPositions} />
              <TradesTable trades={demoTrades} />
            </div>
          </main>
        </>
      ) : null}

      {adminOpen ? (
        <AdminPanel
          trades={liveTrades}
          demoTrades={demo.trades}
          positions={adminPositions}
          portfolio={adminPortfolio}
          risk={adminRisk}
          logs={dash.logs}
          onClose={() => setAdminOpen(false)}
        />
      ) : null}

      <footer className="foot">
        <span className="foot__item">
          core <span className="num">{api.CORE_URL}</span>
        </span>
        <span className="foot__item">
          last sync <span className="num">{dash.lastSync ? fmtAgo(dash.lastSync) : '—'}</span>
        </span>
        <span className="foot__item">
          transport <span className="num">{inSim ? 'simulation' : dash.connection}</span>
        </span>
        {dash.lastError && !inSim ? <span className="foot__item foot__item--err">{dash.lastError}</span> : null}
      </footer>
    </div>
  );
}

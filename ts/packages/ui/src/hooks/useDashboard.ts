import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import * as api from '../api/client';
import { isWsFrame } from '../api/types';
import { RISK_CONTRACT, isRiskContract, toBrowserContract, type RiskContract } from '../lib/riskRules';
import type {
  EngineStatus,
  LogEntry,
  MarketTick,
  PortfolioSnapshot,
  Position,
  RiskState,
  StrategyInfo,
  TrackedOrder,
  TradeRecord,
  VenueHealth,
  WalletHoldings,
  WsFrame,
} from '../api/types';

/** How the dashboard is currently receiving data. */
export type ConnectionState = 'live' | 'polling' | 'down';

const MAX_LOGS = 400;
const MAX_TRADES = 200;
const MAX_TICKS = 300;
const EQUITY_POINTS = 240;
const TRADE_LIMIT = 200;
const LOG_LIMIT = 200;
const TICK_LIMIT = 200;

/** How often the REST snapshot is refreshed while the socket is down. */
const POLL_INTERVAL_MS = 5_000;
/** How often the REST snapshot is reconciled while the socket is live. */
const RECONCILE_INTERVAL_MS = 20_000;

type Settled<T> = { ok: true; value: T } | { ok: false; error: string };

async function settle<T>(promise: Promise<T>): Promise<Settled<T>> {
  try {
    return { ok: true, value: await promise };
  } catch (error) {
    return { ok: false, error: error instanceof Error ? error.message : String(error) };
  }
}

/** Everything the panels need, plus the derived connection state. */
export interface DashboardData {
  portfolio: PortfolioSnapshot | null;
  positions: Position[];
  trades: TradeRecord[];
  /** Orders the engine submitted, with only the fills the venue confirmed. */
  orders: TrackedOrder[];
  logs: LogEntry[];
  risk: RiskState | null;
  /** The on-chain wallet, valued but never traded. `null` until the core answers. */
  wallet: WalletHoldings | null;
  /** The engine's risk contract, or the generated one when no core is running. */
  riskContract: RiskContract;
  /** The same limits, on the browser simulator's own venue. */
  simulatorContract: RiskContract;
  strategy: StrategyInfo | null;
  venues: VenueHealth[];
  ticks: MarketTick[];
  equitySeries: number[];
  engineStatus: EngineStatus | null;
  connection: ConnectionState;
  coreReachable: boolean;
  booted: boolean;
  lastSync: number | null;
  lastError: string | null;
  refreshStrategy: () => Promise<void>;
  refreshRisk: () => Promise<void>;
  refreshOrders: () => Promise<void>;
}

/**
 * Owns every piece of dashboard state.
 *
 * REST snapshots keep the UI truthful even when the WebSocket is down, and the
 * live frame stream keeps it fresh when the socket is up. Both paths write into
 * the same state so the panels never have to care which one produced a value.
 */
export function useDashboard(): DashboardData {
  const [portfolio, setPortfolio] = useState<PortfolioSnapshot | null>(null);
  const [positions, setPositions] = useState<Position[]>([]);
  const [trades, setTrades] = useState<TradeRecord[]>([]);
  const [orders, setOrders] = useState<TrackedOrder[]>([]);
  const [logs, setLogs] = useState<LogEntry[]>([]);
  const [risk, setRisk] = useState<RiskState | null>(null);
  const [wallet, setWallet] = useState<WalletHoldings | null>(null);
  const [riskContract, setRiskContract] = useState<RiskContract>(RISK_CONTRACT);
  const [strategy, setStrategy] = useState<StrategyInfo | null>(null);
  const [venues, setVenues] = useState<VenueHealth[]>([]);
  const [ticks, setTicks] = useState<MarketTick[]>([]);
  const [equitySeries, setEquitySeries] = useState<number[]>([]);
  const [engineStatus, setEngineStatus] = useState<EngineStatus | null>(null);
  const [wsState, setWsState] = useState<api.WsConnectionState>('connecting');
  const [coreReachable, setCoreReachable] = useState(false);
  const [booted, setBooted] = useState(false);
  const [lastSync, setLastSync] = useState<number | null>(null);
  const [lastError, setLastError] = useState<string | null>(null);

  const lastEquityKey = useRef<string | null>(null);

  /** Store a portfolio snapshot and extend the rolling equity buffer. */
  const applyPortfolio = useCallback((snapshot: PortfolioSnapshot) => {
    setPortfolio(snapshot);
    // The dedupe key is tracked outside the state updater: updaters must stay
    // pure, otherwise StrictMode's double invocation discards the append.
    const key = `${snapshot.ts_ms}:${snapshot.equity_usd}`;
    if (lastEquityKey.current === key) return;
    lastEquityKey.current = key;
    setEquitySeries((previous) => {
      const next = previous.concat(snapshot.equity_usd);
      return next.length > EQUITY_POINTS ? next.slice(next.length - EQUITY_POINTS) : next;
    });
  }, []);

  /** Pull every REST snapshot. Individual failures never abort the batch. */
  const refreshAll = useCallback(async () => {
    const [
      portfolioResult,
      positionsResult,
      tradesResult,
      logsResult,
      riskResult,
      strategyResult,
      ticksResult,
      venuesResult,
      rulesResult,
      walletResult,
      ordersResult,
    ] = await Promise.all([
      settle(api.getPortfolio()),
      settle(api.getPositions()),
      settle(api.getTrades(TRADE_LIMIT)),
      settle(api.getLogs(LOG_LIMIT)),
      settle(api.getRisk()),
      settle(api.getStrategy()),
      settle(api.getTicks(TICK_LIMIT)),
      settle(api.getVenues()),
      settle(api.getRiskRules()),
      settle(api.getWalletHoldings()),
      settle(api.getOrders()),
    ]);

    const failures: string[] = [];
    let reachable = false;

    if (portfolioResult.ok) {
      applyPortfolio(portfolioResult.value);
      reachable = true;
    } else {
      failures.push(portfolioResult.error);
    }
    if (positionsResult.ok) {
      setPositions(positionsResult.value);
      reachable = true;
    } else {
      failures.push(positionsResult.error);
    }
    if (tradesResult.ok) {
      setTrades(tradesResult.value);
      reachable = true;
    } else {
      failures.push(tradesResult.error);
    }
    if (logsResult.ok) {
      setLogs(logsResult.value);
      reachable = true;
    } else {
      failures.push(logsResult.error);
    }
    if (riskResult.ok) {
      setRisk(riskResult.value);
      reachable = true;
    } else {
      failures.push(riskResult.error);
    }
    if (strategyResult.ok) {
      setStrategy(strategyResult.value);
      reachable = true;
    } else {
      failures.push(strategyResult.error);
    }
    if (ticksResult.ok) {
      // The REST endpoint returns newest first; the UI keeps a chronological list.
      setTicks(ticksResult.value.slice().reverse());
      reachable = true;
    } else {
      failures.push(ticksResult.error);
    }
    if (venuesResult.ok) {
      setVenues(venuesResult.value);
      reachable = true;
    } else {
      failures.push(venuesResult.error);
    }
    if (rulesResult.ok && isRiskContract(rulesResult.value)) {
      setRiskContract(rulesResult.value);
    }
    // A wallet read that fails is not a trading outage, so it stays out of the
    // headline error: the wallet panel reports its own missing read.
    if (walletResult.ok) {
      setWallet(walletResult.value);
      reachable = true;
    }
    if (ordersResult.ok) {
      setOrders(ordersResult.value);
      reachable = true;
    } else {
      failures.push(ordersResult.error);
    }

    setCoreReachable(reachable);
    setLastError(failures.length > 0 ? failures[0] : null);
    setLastSync(Date.now());
    setBooted(true);
  }, [applyPortfolio]);

  const refreshStrategy = useCallback(async () => {
    const result = await settle(api.getStrategy());
    if (result.ok) setStrategy(result.value);
  }, []);

  const refreshRisk = useCallback(async () => {
    const result = await settle(api.getRisk());
    if (result.ok) setRisk(result.value);
  }, []);

  const refreshOrders = useCallback(async () => {
    const result = await settle(api.getOrders());
    if (result.ok) setOrders(result.value);
  }, []);

  /** Apply one live frame. */
  const handleFrame = useCallback(
    (frame: WsFrame) => {
      if (isWsFrame(frame, 'tick')) {
        const tick = frame.data;
        setTicks((previous) => {
          const next = previous.concat(tick);
          return next.length > MAX_TICKS ? next.slice(next.length - MAX_TICKS) : next;
        });
        return;
      }
      if (isWsFrame(frame, 'trade')) {
        const trade = frame.data;
        setTrades((previous) => [trade, ...previous].slice(0, MAX_TRADES));
        return;
      }
      if (isWsFrame(frame, 'log')) {
        const entry = frame.data;
        setLogs((previous) => [entry, ...previous].slice(0, MAX_LOGS));
        return;
      }
      if (isWsFrame(frame, 'portfolio')) {
        applyPortfolio(frame.data);
        return;
      }
      if (isWsFrame(frame, 'risk')) {
        setRisk(frame.data);
        return;
      }
      if (isWsFrame(frame, 'status')) {
        setEngineStatus(frame.data);
      }
    },
    [applyPortfolio],
  );

  // Live frame stream.
  useEffect(() => {
    const disconnect = api.connectWebSocket(handleFrame, {
      onState: setWsState,
      onReconnect: () => {
        void refreshAll();
      },
    });
    return disconnect;
  }, [handleFrame, refreshAll]);

  // REST snapshots: fast while the socket is down, slow reconciliation when live.
  useEffect(() => {
    void refreshAll();
    const interval = window.setInterval(
      () => {
        void refreshAll();
      },
      wsState === 'connected' ? RECONCILE_INTERVAL_MS : POLL_INTERVAL_MS,
    );
    return () => window.clearInterval(interval);
  }, [refreshAll, wsState]);

  const connection: ConnectionState =
    wsState === 'connected' ? 'live' : coreReachable ? 'polling' : 'down';

  // The simulator mirrors the engine's money rules, on its own venue.
  const simulatorContract = useMemo(() => toBrowserContract(riskContract), [riskContract]);

  return {
    portfolio,
    positions,
    trades,
    orders,
    logs,
    risk,
    wallet,
    riskContract,
    simulatorContract,
    strategy,
    venues,
    ticks,
    equitySeries,
    engineStatus,
    connection,
    coreReachable,
    booted,
    lastSync,
    lastError,
    refreshStrategy,
    refreshRisk,
    refreshOrders,
  };
}

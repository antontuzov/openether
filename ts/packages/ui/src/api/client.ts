/**
 * The single place where the dashboard talks to the openEther core.
 *
 * Everything HTTP and WebSocket lives here: the components never touch
 * `fetch` or `WebSocket` directly, which keeps retries, backoff and error
 * handling in one auditable spot.
 *
 * Base URL comes from `VITE_CORE_URL` (see `.env.example`). In dev the Vite
 * server proxies `/api` and `/ws` to that URL, so the browser talks to the
 * same origin and CORS never enters the picture.
 */

import type {
  BreakerResetResponse,
  LogEntry,
  MarketTick,
  PortfolioSnapshot,
  Position,
  RiskState,
  StrategyInfo,
  StrategyReloadResponse,
  TradeRecord,
  VenueHealth,
  WsFrame,
} from './types';

export type {
  BreakerResetResponse,
  BreakerState,
  EngineStatus,
  LogEntry,
  MarketTick,
  PortfolioSnapshot,
  Position,
  RiskLimits,
  RiskState,
  Side,
  StrategyInfo,
  StrategyReloadResponse,
  TradeRecord,
  Venue,
  VenueHealth,
  WsFrame,
  WsFrameDataMap,
  WsFrameType,
} from './types';
export { isWsFrame } from './types';

/** Raw core URL, e.g. `http://localhost:8080`. */
export const CORE_URL: string = (import.meta.env.VITE_CORE_URL ?? 'http://localhost:8080').replace(/\/+$/, '');

/**
 * In dev the Vite proxy forwards `/api` + `/ws` to `CORE_URL`, so the client
 * uses same-origin relative paths. A production build has no proxy, so it
 * talks to the absolute core URL.
 */
const USE_SAME_ORIGIN = import.meta.env.DEV === true;

/** Prefix every REST call is built on (empty string = same origin). */
export const API_BASE: string = USE_SAME_ORIGIN ? '' : CORE_URL;

/** Build an absolute (or same-origin) REST URL. */
export function apiUrl(path: string): string {
  return `${API_BASE}${path}`;
}

/** Resolve the WebSocket endpoint: same host, path `/ws`. */
export function wsUrl(): string {
  if (USE_SAME_ORIGIN) {
    const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
    return `${proto}//${window.location.host}/ws`;
  }
  try {
    const url = new URL(CORE_URL);
    const proto = url.protocol === 'https:' ? 'wss:' : 'ws:';
    return `${proto}//${url.host}/ws`;
  } catch {
    const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
    return `${proto}//${window.location.host}/ws`;
  }
}

/** A transport or protocol failure, carrying the HTTP status when there is one. */
export class ApiError extends Error {
  readonly status: number;

  constructor(message: string, status: number) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
  }
}

function isAbort(error: unknown): boolean {
  return error instanceof DOMException && error.name === 'AbortError';
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const headers: Record<string, string> = { accept: 'application/json' };
  if (init?.body !== undefined) headers['content-type'] = 'application/json';

  let response: Response;
  try {
    response = await fetch(apiUrl(path), { ...init, headers: { ...headers, ...init?.headers } });
  } catch (error) {
    if (isAbort(error)) throw error;
    throw new ApiError(`cannot reach core at ${CORE_URL}: ${(error as Error).message}`, 0);
  }

  const body = await response.text();
  if (!response.ok) {
    const detail = body.trim().slice(0, 240);
    throw new ApiError(
      `${response.status} ${response.statusText || 'request failed'}${detail ? ` — ${detail}` : ''}`,
      response.status,
    );
  }
  if (!body) return null as unknown as T;
  try {
    return JSON.parse(body) as T;
  } catch {
    throw new ApiError('core returned a malformed JSON payload', response.status);
  }
}

/* ------------------------------------------------------------------ REST -- */

/** Current account snapshot (equity, cash, PnL). */
export function getPortfolio(): Promise<PortfolioSnapshot> {
  return request<PortfolioSnapshot>('/api/portfolio');
}

/** Open positions. */
export function getPositions(): Promise<Position[]> {
  return request<Position[]>('/api/positions');
}

/** Recent fills, newest first. */
export function getTrades(limit = 200): Promise<TradeRecord[]> {
  return request<TradeRecord[]>(`/api/trades?limit=${encodeURIComponent(String(limit))}`);
}

/** Recent log lines, newest first. Optionally narrowed to a single source. */
export function getLogs(limit = 200, source?: string): Promise<LogEntry[]> {
  const params = new URLSearchParams({ limit: String(limit) });
  if (source) params.set('source', source);
  return request<LogEntry[]>(`/api/logs?${params.toString()}`);
}

/** Live risk state and configured limits. */
export function getRisk(): Promise<RiskState> {
  return request<RiskState>('/api/risk');
}

/**
 * The canonical risk contract the engine is running with.
 *
 * The browser simulator uses it so it refuses exactly what the engine refuses.
 */
export function getRiskRules(): Promise<unknown> {
  return request<unknown>('/api/risk/rules');
}

/** Metadata of the loaded WASM strategy. */
export function getStrategy(): Promise<StrategyInfo> {
  return request<StrategyInfo>('/api/strategy');
}

/** Recent market ticks, newest first. */
export function getTicks(limit = 200): Promise<MarketTick[]> {
  return request<MarketTick[]>(`/api/ticks?limit=${encodeURIComponent(String(limit))}`);
}

/** Venue health flags. */
export function getVenues(): Promise<VenueHealth[]> {
  return request<VenueHealth[]>('/api/venues');
}

/** Hot-swap the sandboxed strategy from an absolute `.wasm` path. */
export function reloadStrategy(path: string): Promise<StrategyReloadResponse> {
  return request<StrategyReloadResponse>('/api/strategy/reload', {
    method: 'POST',
    body: JSON.stringify({ path }),
  });
}

/** Manually reset the circuit breaker (the engine requires an explicit confirm). */
export function resetBreaker(): Promise<BreakerResetResponse> {
  return request<BreakerResetResponse>('/api/breaker/reset', {
    method: 'POST',
    body: JSON.stringify({ confirm: true }),
  });
}

/* ------------------------------------------------------------- WebSocket -- */

/** Transport state of the live frame stream. */
export type WsConnectionState = 'connecting' | 'connected' | 'disconnected';

export interface ConnectOptions {
  /** Called whenever the socket transitions. */
  onState?: (state: WsConnectionState) => void;
  /** Called after a *re*connection, so the caller can refresh REST snapshots. */
  onReconnect?: () => void;
}

const RECONNECT_MIN_MS = 1_000;
const RECONNECT_MAX_MS = 30_000;
const PING_INTERVAL_MS = 20_000;

/**
 * Open the live frame stream.
 *
 * Reconnects with exponential backoff (1s → 30s) and notifies `onReconnect`
 * after a successful reconnect so the caller can re-sync its REST snapshots
 * instead of showing stale data.
 *
 * @returns a disposer that closes the socket and cancels any pending retry.
 */
export function connectWebSocket(onFrame: (frame: WsFrame) => void, options: ConnectOptions = {}): () => void {
  let socket: WebSocket | null = null;
  let reconnectTimer: number | undefined;
  let pingTimer: number | undefined;
  let attempt = 0;
  let disposed = false;
  let everConnected = false;

  const clearTimers = (): void => {
    if (reconnectTimer !== undefined) {
      window.clearTimeout(reconnectTimer);
      reconnectTimer = undefined;
    }
    if (pingTimer !== undefined) {
      window.clearInterval(pingTimer);
      pingTimer = undefined;
    }
  };

  const scheduleReconnect = (): void => {
    if (disposed) return;
    const delay = Math.min(RECONNECT_MIN_MS * 2 ** attempt, RECONNECT_MAX_MS);
    attempt += 1;
    reconnectTimer = window.setTimeout(() => {
      reconnectTimer = undefined;
      open();
    }, delay);
  };

  const open = (): void => {
    if (disposed) return;
    options.onState?.('connecting');

    let url: string;
    try {
      url = wsUrl();
    } catch {
      scheduleReconnect();
      return;
    }

    try {
      socket = new WebSocket(url);
    } catch (error) {
      console.warn('[openEther] websocket could not be opened:', (error as Error).message);
      scheduleReconnect();
      return;
    }

    socket.onopen = () => {
      attempt = 0;
      options.onState?.('connected');
      if (everConnected) options.onReconnect?.();
      everConnected = true;
      if (pingTimer !== undefined) window.clearInterval(pingTimer);
      pingTimer = window.setInterval(() => {
        if (socket && socket.readyState === WebSocket.OPEN) {
          socket.send(JSON.stringify({ type: 'ping' }));
        }
      }, PING_INTERVAL_MS);
    };

    socket.onmessage = (event: MessageEvent<string>) => {
      let parsed: unknown;
      try {
        parsed = JSON.parse(typeof event.data === 'string' ? event.data : String(event.data));
      } catch {
        return;
      }
      if (parsed && typeof parsed === 'object' && 'type' in parsed) {
        onFrame(parsed as WsFrame);
      }
    };

    socket.onerror = () => {
      // `onclose` always follows; reconnection is driven from there.
    };

    socket.onclose = () => {
      if (pingTimer !== undefined) {
        window.clearInterval(pingTimer);
        pingTimer = undefined;
      }
      options.onState?.('disconnected');
      scheduleReconnect();
    };
  };

  open();

  return () => {
    disposed = true;
    clearTimers();
    if (socket) {
      socket.onopen = null;
      socket.onmessage = null;
      socket.onerror = null;
      socket.onclose = null;
      try {
        socket.close();
      } catch {
        // ignore: the socket is being torn down anyway
      }
      socket = null;
    }
  };
}

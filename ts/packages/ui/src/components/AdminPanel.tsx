import { useCallback, useEffect, useMemo, useState } from 'react';
import type { FormEvent } from 'react';
import type { LogEntry, PortfolioSnapshot, Position, RiskState, TradeRecord } from '../api/types';
import { STARTING_CASH } from '../lib/demo';
import type { DemoTrade } from '../lib/demo';
import { fmtClock, fmtPct, fmtPrice, fmtSignedUsd, fmtUsd, titleise, truncate } from '../lib/format';
import type { PivotColumn, PivotRow, PivotableTrade } from '../lib/pivot';
import {
  PIVOT_COLUMNS,
  adaptDemoTrades,
  equitySeriesFromTrades,
  pivotBy,
  summariseTrades,
} from '../lib/pivot';
import { Panel } from './Panel';
import './admin.css';

/** sessionStorage key that remembers an unlocked admin session. */
const UNLOCK_KEY = 'openether.admin.unlocked';

/**
 * The SHA-256 hex digest of the admin password, baked in at build time.
 *
 * There is deliberately no default: when the variable is missing the panel
 * refuses to open at all.
 */
const CONFIGURED_HASH: string = ((import.meta.env.VITE_ADMIN_PASSWORD_HASH as string | undefined) ?? '').trim();

export interface AdminPanelProps {
  /** Live core trades. When empty the simulation is used instead. */
  trades: TradeRecord[];
  /** Trades produced by the in-browser simulator. */
  demoTrades?: DemoTrade[];
  positions: Position[];
  portfolio: PortfolioSnapshot | null;
  risk: RiskState | null;
  logs: LogEntry[];
  startingEquity?: number;
  /** Called when the viewer dismisses the panel. */
  onClose?: () => void;
  /**
   * Optional pre-computed equity curve.
   *
   * When the host already tracks an equity history it is preferred over the
   * curve derived from the fills, because it also carries the unrealised
   * part of the equity.
   */
  equitySeries?: number[];
}

/* ------------------------------------------------------------------ gating */

function toHex(buffer: ArrayBuffer): string {
  const bytes = new Uint8Array(buffer);
  let out = '';
  for (const byte of bytes) {
    out += byte.toString(16).padStart(2, '0');
  }
  return out;
}

async function sha256Hex(text: string): Promise<string> {
  const data = new TextEncoder().encode(text);
  const digest = await crypto.subtle.digest('SHA-256', data);
  return toHex(digest);
}

/**
 * Compare two hex digests without an early return.
 *
 * Every character is folded into one accumulator so the loop always runs to
 * the end, whatever the first difference is.
 */
function timingSafeEqual(a: string, b: string): boolean {
  const length = Math.max(a.length, b.length);
  let diff = a.length ^ b.length;
  for (let index = 0; index < length; index += 1) {
    const left = index < a.length ? a.charCodeAt(index) : 0;
    const right = index < b.length ? b.charCodeAt(index) : 0;
    diff |= left ^ right;
  }
  return diff === 0;
}

function readUnlocked(): boolean {
  try {
    return window.sessionStorage.getItem(UNLOCK_KEY) === '1';
  } catch {
    return false;
  }
}

function writeUnlocked(unlocked: boolean): void {
  try {
    if (unlocked) {
      window.sessionStorage.setItem(UNLOCK_KEY, '1');
    } else {
      window.sessionStorage.removeItem(UNLOCK_KEY);
    }
  } catch {
    // A blocked storage quota must not break the panel.
  }
}

/* ------------------------------------------------------------------- utils */

function levelClass(level: string): string {
  switch (level.toLowerCase()) {
    case 'warn':
    case 'warning':
      return 'lvl--warn';
    case 'error':
    case 'fatal':
      return 'lvl--error';
    case 'debug':
    case 'trace':
      return 'lvl--debug';
    default:
      return 'lvl--info';
  }
}

function fmtVolume(value: number): string {
  const absolute = Math.abs(value);
  const sign = value < 0 ? '-' : '';
  if (absolute >= 1_000_000_000) return `${sign}$${(absolute / 1_000_000_000).toFixed(2)}B`;
  if (absolute >= 1_000_000) return `${sign}$${(absolute / 1_000_000).toFixed(2)}M`;
  if (absolute >= 1_000) return `${sign}$${(absolute / 1_000).toFixed(1)}K`;
  return `${sign}$${absolute.toFixed(2)}`;
}

function fmtRatio(value: number): string {
  return `${(value * 100).toFixed(1)}%`;
}

/* ------------------------------------------------------------------ charts */

const EQUITY_VIEW_W = 1000;
const EQUITY_VIEW_H = 220;
const EQUITY_PAD_Y = 10;
/** Unique so an admin chart never collides with the dashboard's equity chart. */
const EQUITY_FILL_ID = 'openether-admin-equity-fill';

interface EquityGeometry {
  line: string;
  area: string;
  min: number;
  max: number;
  first: number;
  last: number;
}

/** Hand-rolled SVG area chart of the equity curve, in the house style. */
function AdminEquityChart({ points, height = 200 }: { points: number[]; height?: number }) {
  const geometry = useMemo<EquityGeometry | null>(() => {
    if (points.length < 2) return null;

    let min = points[0];
    let max = points[0];
    for (const value of points) {
      if (value < min) min = value;
      if (value > max) max = value;
    }
    const span = max - min;
    const pad = span > 0 ? span * 0.18 : Math.max(Math.abs(max) * 0.002, 1);
    const low = min - pad;
    const high = max + pad;
    const range = high - low || 1;

    const coords = points.map((value, index) => ({
      x: (index / (points.length - 1)) * EQUITY_VIEW_W,
      y: EQUITY_PAD_Y + (1 - (value - low) / range) * (EQUITY_VIEW_H - EQUITY_PAD_Y * 2),
    }));

    let line = `M ${coords[0].x.toFixed(2)} ${coords[0].y.toFixed(2)}`;
    for (let index = 1; index < coords.length; index += 1) {
      const previous = coords[index - 1];
      const current = coords[index];
      const midX = (previous.x + current.x) / 2;
      const midY = (previous.y + current.y) / 2;
      line += ` Q ${previous.x.toFixed(2)} ${previous.y.toFixed(2)} ${midX.toFixed(2)} ${midY.toFixed(2)}`;
    }
    const last = coords[coords.length - 1];
    line += ` L ${last.x.toFixed(2)} ${last.y.toFixed(2)}`;

    const area = `${line} L ${last.x.toFixed(2)} ${EQUITY_VIEW_H} L ${coords[0].x.toFixed(2)} ${EQUITY_VIEW_H} Z`;

    return { line, area, min, max, first: points[0], last: points[points.length - 1] };
  }, [points]);

  if (!geometry) {
    return (
      <div className="chart chart--empty" style={{ height }}>
        <svg
          className="chart__svg"
          viewBox={`0 0 ${EQUITY_VIEW_W} ${EQUITY_VIEW_H}`}
          preserveAspectRatio="none"
          aria-hidden="true"
        >
          <line
            x1="0"
            y1={EQUITY_VIEW_H / 2}
            x2={EQUITY_VIEW_W}
            y2={EQUITY_VIEW_H / 2}
            className="chart__flat"
            vectorEffect="non-scaling-stroke"
          />
        </svg>
        <span className="chart__placeholder">no fills to plot yet</span>
      </div>
    );
  }

  const rising = geometry.last >= geometry.first;
  const stroke = rising ? 'var(--up)' : 'var(--down)';
  const delta = geometry.last - geometry.first;

  return (
    <div className="chart" style={{ height }}>
      <svg
        className="chart__svg"
        viewBox={`0 0 ${EQUITY_VIEW_W} ${EQUITY_VIEW_H}`}
        preserveAspectRatio="none"
        role="img"
        aria-label={`Equity curve, ${points.length} samples, change ${delta.toFixed(2)}`}
      >
        <defs>
          <linearGradient id={EQUITY_FILL_ID} x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor={stroke} stopOpacity="0.2" />
            <stop offset="100%" stopColor={stroke} stopOpacity="0" />
          </linearGradient>
        </defs>

        {[0.25, 0.5, 0.75].map((ratio) => (
          <line
            key={ratio}
            className="chart__grid"
            x1="0"
            y1={EQUITY_VIEW_H * ratio}
            x2={EQUITY_VIEW_W}
            y2={EQUITY_VIEW_H * ratio}
            vectorEffect="non-scaling-stroke"
          />
        ))}

        <path d={geometry.area} fill={`url(#${EQUITY_FILL_ID})`} />
        <path d={geometry.line} className="chart__line" stroke={stroke} vectorEffect="non-scaling-stroke" />
      </svg>

      <div className="chart__overlay">
        <span className="chart__tag chart__tag--top num">{geometry.max.toFixed(2)}</span>
        <span className="chart__tag chart__tag--bottom num">{geometry.min.toFixed(2)}</span>
      </div>

      <div className="chart__legend">
        <span className="chart__legend-item">
          window <span className={`num ${delta >= 0 ? 'pos' : 'neg'}`}>{fmtSignedUsd(delta)}</span>
        </span>
        <span className="chart__legend-item chart__legend-item--dim">{points.length} fills</span>
      </div>
    </div>
  );
}

const BAR_VIEW_W = 340;
const BAR_LABEL_W = 74;
const BAR_VALUE_W = 66;
const BAR_ROW_H = 16;
const BAR_TOP = 8;

/**
 * Horizontal bar chart of net PnL per pivot row.
 *
 * The bars are drawn in a fixed viewBox so the labels stay legible whatever
 * the container width is.
 */
function PnlBars({ rows }: { rows: PivotRow[] }) {
  const plotWidth = BAR_VIEW_W - BAR_LABEL_W - BAR_VALUE_W;
  const height = BAR_TOP * 2 + rows.length * BAR_ROW_H;

  let positiveMax = 0;
  let negativeMax = 0;
  for (const row of rows) {
    if (row.netPnl > positiveMax) positiveMax = row.netPnl;
    if (row.netPnl < negativeMax) negativeMax = row.netPnl;
  }
  const negativeSpan = Math.abs(negativeMax);
  const positiveSpan = positiveMax;
  const total = negativeSpan + positiveSpan;
  const negativeWidth = total > 0 ? (negativeSpan / total) * plotWidth : 0;
  const positiveWidth = total > 0 ? (positiveSpan / total) * plotWidth : 0;
  const zeroX = BAR_LABEL_W + negativeWidth;

  return (
    <div className="adm-bars">
      <svg
        className="adm-bars__svg"
        viewBox={`0 0 ${BAR_VIEW_W} ${Math.max(height, 40)}`}
        role="img"
        aria-label={`Net profit and loss per bucket, ${rows.length} rows`}
      >
        {rows.map((row, index) => {
          const y = BAR_TOP + index * BAR_ROW_H;
          const barHeight = BAR_ROW_H - 5;
          const scale = row.netPnl >= 0 ? positiveWidth / (positiveSpan || 1) : negativeWidth / (negativeSpan || 1);
          const barWidth = Math.max(1, Math.abs(row.netPnl) * scale);
          const x = row.netPnl >= 0 ? zeroX : zeroX - barWidth;
          return (
            <g key={row.key}>
              <text className="adm-bars__label" x={BAR_LABEL_W - 6} y={y + barHeight - 2} textAnchor="end">
                {truncate(row.key, 9)}
              </text>
              <rect
                className={`adm-bars__bar ${row.netPnl >= 0 ? 'adm-bars__bar--up' : 'adm-bars__bar--down'}`}
                x={x}
                y={y}
                width={barWidth}
                height={barHeight}
                rx={1}
              />
              <text
                className={`adm-bars__value num ${row.netPnl >= 0 ? 'pos' : 'neg'}`}
                x={BAR_VIEW_W - 2}
                y={y + barHeight - 2}
                textAnchor="end"
              >
                {fmtSignedUsd(row.netPnl, 0)}
              </text>
            </g>
          );
        })}
        <line
          className="adm-bars__axis"
          x1={zeroX}
          y1={BAR_TOP - 2}
          x2={zeroX}
          y2={Math.max(height, 40) - 2}
          vectorEffect="non-scaling-stroke"
        />
      </svg>
    </div>
  );
}

/* ------------------------------------------------------------------ panel */

type PivotSortKey = 'key' | 'trades' | 'grossPnl' | 'fees' | 'netPnl' | 'winRate' | 'volumeUsd';

export function AdminPanel({
  trades,
  demoTrades = [],
  positions,
  portfolio,
  risk,
  logs,
  startingEquity,
  onClose,
  equitySeries: equitySeriesProp,
}: AdminPanelProps) {
  const configured = CONFIGURED_HASH.length > 0;
  const [unlocked, setUnlocked] = useState<boolean>(() => configured && readUnlocked());
  const [password, setPassword] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const [dimension, setDimension] = useState<PivotColumn['id']>('symbol');
  const [sortKey, setSortKey] = useState<PivotSortKey>('netPnl');
  const [sortAsc, setSortAsc] = useState(false);

  useEffect(() => {
    if (!configured) {
      writeUnlocked(false);
      setUnlocked(false);
    }
  }, [configured]);

  const handleUnlock = useCallback(
    async (event: FormEvent<HTMLFormElement>) => {
      event.preventDefault();
      if (!configured) {
        return;
      }
      setError(null);
      setBusy(true);
      try {
        const digest = await sha256Hex(password);
        if (timingSafeEqual(digest, CONFIGURED_HASH.toLowerCase())) {
          writeUnlocked(true);
          setUnlocked(true);
          setPassword('');
        } else {
          setError('That password does not match.');
        }
      } catch {
        setError('This browser could not hash the password (a secure context is required).');
      } finally {
        setBusy(false);
      }
    },
    [configured, password],
  );

  const handleLock = useCallback(() => {
    writeUnlocked(false);
    setUnlocked(false);
    setPassword('');
    setError(null);
  }, []);

  const handleClose = useCallback(() => {
    onClose?.();
  }, [onClose]);

  /* ----------------------------------------------------------- data source */

  const usingLive = trades.length > 0;
  const sourceLabel = usingLive ? 'live engine' : 'simulation';

  const pivotTrades = useMemo<PivotableTrade[]>(
    () => (usingLive ? trades : adaptDemoTrades(demoTrades)),
    [usingLive, trades, demoTrades],
  );

  const summary = useMemo(() => summariseTrades(pivotTrades), [pivotTrades]);

  const baseEquity = useMemo(() => {
    if (typeof startingEquity === 'number' && Number.isFinite(startingEquity)) {
      return startingEquity;
    }
    if (portfolio) {
      return portfolio.equity_usd - portfolio.realized_pnl_usd;
    }
    return STARTING_CASH;
  }, [startingEquity, portfolio]);

  const equitySeries = useMemo(() => {
    if (equitySeriesProp && equitySeriesProp.length > 0) {
      return equitySeriesProp;
    }
    return equitySeriesFromTrades(pivotTrades, baseEquity);
  }, [equitySeriesProp, pivotTrades, baseEquity]);

  const rows = useMemo(() => pivotBy(pivotTrades, dimension), [pivotTrades, dimension]);

  const sortedRows = useMemo(() => {
    const copy = rows.slice();
    copy.sort((a, b) => {
      let delta = 0;
      if (sortKey === 'key') {
        delta = a.key.localeCompare(b.key);
      } else {
        delta = (a[sortKey] as number) - (b[sortKey] as number);
      }
      return sortAsc ? delta : -delta;
    });
    return copy;
  }, [rows, sortKey, sortAsc]);

  const barRows = useMemo(
    () =>
      rows
        .slice()
        .sort((a, b) => Math.abs(b.netPnl) - Math.abs(a.netPnl))
        .slice(0, 12),
    [rows],
  );

  const toggleSort = (key: PivotSortKey): void => {
    if (key === sortKey) {
      setSortAsc((current) => !current);
      return;
    }
    setSortKey(key);
    setSortAsc(key === 'key');
  };

  /* ------------------------------------------------------------ risk panel */

  const demoRisk = useMemo(() => {
    if (risk) {
      return null;
    }
    // Derived from the simulation only: peak-to-trough of the trade equity
    // curve and today's realised net PnL.
    let peak = -Infinity;
    let drawdown = 0;
    for (const value of equitySeries) {
      if (value > peak) peak = value;
      if (peak > 0) {
        const drop = ((peak - value) / peak) * 100;
        if (drop > drawdown) drawdown = drop;
      }
    }
    const today = new Date().toISOString().slice(0, 10);
    let dailyPnl = 0;
    for (const trade of pivotTrades) {
      if (new Date(trade.ts_ms).toISOString().slice(0, 10) === today) {
        dailyPnl += trade.realized_pnl - trade.fee;
      }
    }
    return { drawdown, dailyPnl };
  }, [risk, equitySeries, pivotTrades]);

  const grossExposure = useMemo(() => {
    if (risk) return risk.gross_exposure_usd;
    if (positions.length > 0) {
      return positions.reduce((total, position) => total + Math.abs(position.notional_usd), 0);
    }
    return null;
  }, [risk, positions]);

  const recentLogs = useMemo(() => logs.slice(0, 60), [logs]);

  /* -------------------------------------------------------------- lock view */

  if (!configured) {
    return (
      <div className="admin" role="dialog" aria-modal="true" aria-label="Private analytics">
        <header className="admin__hd">
          <span className="panel__tick" aria-hidden="true" />
          <h1 className="admin__title">Private analytics</h1>
          <div className="admin__actions">
            <button type="button" className="btn btn--tiny" onClick={handleClose}>
              close
            </button>
          </div>
        </header>

        <div className="admin__bd">
          <div className="admin__lock">
            <div className="admin__lock-card">
              <p className="admin__lock-title">Admin access is not configured</p>
              <p className="admin__lock-copy">
                This build has no <code className="num">VITE_ADMIN_PASSWORD_HASH</code> set, so the
                analytics view stays locked. There is no default password: set the variable to the
                SHA-256 hex digest of the admin password at build time and rebuild.
              </p>
            </div>
          </div>
        </div>
      </div>
    );
  }

  if (!unlocked) {
    return (
      <div className="admin" role="dialog" aria-modal="true" aria-label="Private analytics">
        <header className="admin__hd">
          <span className="panel__tick" aria-hidden="true" />
          <h1 className="admin__title">Private analytics</h1>
          <div className="admin__actions">
            <button type="button" className="btn btn--tiny" onClick={handleClose}>
              close
            </button>
          </div>
        </header>

        <div className="admin__bd">
          <div className="admin__lock">
            <form className="admin__lock-card" onSubmit={handleUnlock}>
              <p className="admin__lock-title">Admin password</p>
              <p className="admin__lock-copy">
                The analytics view is private. This is a static site, so the gate only keeps casual
                visitors out of these numbers — it is not a substitute for keeping the real engine on
                a private network.
              </p>
              <label className="admin__lock-label" htmlFor="admin-password">
                password
              </label>
              <input
                id="admin-password"
                className="input admin__lock-input"
                type="password"
                value={password}
                autoComplete="current-password"
                onChange={(event) => setPassword(event.target.value)}
              />
              {error ? (
                <p className="inline-msg inline-msg--err" role="alert">
                  {error}
                </p>
              ) : null}
              <button type="submit" className="btn btn--primary admin__lock-submit" disabled={busy}>
                {busy ? 'checking…' : 'unlock'}
              </button>
            </form>
          </div>
        </div>
      </div>
    );
  }

  /* ----------------------------------------------------------- unlocked view */

  const bestTrade = summary.bestTrade;
  const worstTrade = summary.worstTrade;

  return (
    <div className="admin" role="dialog" aria-modal="true" aria-label="Private analytics">
      <header className="admin__hd">
        <span className="panel__tick" aria-hidden="true" />
        <h1 className="admin__title">Private analytics</h1>
        <span className={`admin__source admin__source--${usingLive ? 'live' : 'demo'}`}>
          {sourceLabel}
        </span>
        <div className="admin__actions">
          <button type="button" className="btn btn--tiny" onClick={handleLock}>
            lock
          </button>
          <button type="button" className="btn btn--tiny" onClick={handleClose}>
            close
          </button>
        </div>
      </header>

      <div className="admin__bd">
        <div className="stats adm-stats">
          <div className="stat">
            <span className="stat__k">total trades</span>
            <span className="stat__v num">{summary.totalTrades}</span>
          </div>
          <div className="stat">
            <span className="stat__k">net p&amp;l</span>
            <span className={`stat__v num ${summary.netPnl > 0 ? 'pos' : summary.netPnl < 0 ? 'neg' : ''}`}>
              {fmtSignedUsd(summary.netPnl)}
            </span>
          </div>
          <div className="stat">
            <span className="stat__k">gross p&amp;l</span>
            <span className={`stat__v num ${summary.grossPnl > 0 ? 'pos' : summary.grossPnl < 0 ? 'neg' : ''}`}>
              {fmtSignedUsd(summary.grossPnl)}
            </span>
          </div>
          <div className="stat">
            <span className="stat__k">fees paid</span>
            <span className="stat__v num dim">${fmtUsd(summary.fees)}</span>
          </div>
          <div className="stat">
            <span className="stat__k">win rate</span>
            <span className="stat__v num">{fmtRatio(summary.winRate)}</span>
          </div>
          <div className="stat">
            <span className="stat__k">profit factor</span>
            <span className="stat__v num">{summary.profitFactor.toFixed(2)}</span>
          </div>
          <div className="stat">
            <span className="stat__k">best trade</span>
            <span className={`stat__v num ${bestTrade ? 'pos' : ''}`}>
              {bestTrade ? fmtSignedUsd(bestTrade.realized_pnl - bestTrade.fee) : '—'}
            </span>
            <span className="stat__sub dim">
              {bestTrade ? `${bestTrade.symbol} · ${bestTrade.side.toUpperCase()}` : 'no fills'}
            </span>
          </div>
          <div className="stat">
            <span className="stat__k">worst trade</span>
            <span className={`stat__v num ${worstTrade ? 'neg' : ''}`}>
              {worstTrade ? fmtSignedUsd(worstTrade.realized_pnl - worstTrade.fee) : '—'}
            </span>
            <span className="stat__sub dim">
              {worstTrade ? `${worstTrade.symbol} · ${worstTrade.side.toUpperCase()}` : 'no fills'}
            </span>
          </div>
        </div>

        <div className="admin__grid">
          <Panel
            title="Equity curve"
            hint={usingLive ? 'live fills' : 'simulated fills'}
            actions={<span className="panel__badge">{summary.totalTrades} fills</span>}
          >
            <AdminEquityChart points={equitySeries} />
          </Panel>

          <Panel title="Net p&l by bucket" hint="top 12 by absolute p&l">
            {barRows.length === 0 ? (
              <div className="empty empty--inline">
                <p className="empty__title">Nothing to chart yet</p>
                <p className="empty__hint">Bars appear once there are fills to group.</p>
              </div>
            ) : (
              <PnlBars rows={barRows} />
            )}
          </Panel>
        </div>

        <Panel
          title="Pivot"
          hint={`${rows.length} buckets`}
          actions={
            <div className="adm-seg" role="group" aria-label="Pivot dimension">
              {PIVOT_COLUMNS.map((column) => (
                <button
                  key={column.id}
                  type="button"
                  className={`adm-seg__btn ${dimension === column.id ? 'adm-seg__btn--on' : ''}`}
                  aria-pressed={dimension === column.id}
                  onClick={() => setDimension(column.id)}
                >
                  {column.label}
                </button>
              ))}
            </div>
          }
          bodyClassName="panel__bd--flush"
        >
          {rows.length === 0 ? (
            <div className="empty empty--inline">
              <p className="empty__title">No fills to pivot</p>
              <p className="empty__hint">The pivot table fills in as soon as the engine trades.</p>
            </div>
          ) : (
            <div className="tbl-wrap adm-pivot-wrap">
              <table className="tbl adm-pivot">
                <colgroup>
                  <col style={{ width: '20%' }} />
                  <col style={{ width: '10%' }} />
                  <col style={{ width: '15%' }} />
                  <col style={{ width: '13%' }} />
                  <col style={{ width: '15%' }} />
                  <col style={{ width: '12%' }} />
                  <col style={{ width: '15%' }} />
                </colgroup>
                <thead>
                  <tr>
                    {(
                      [
                        { key: 'key', label: PIVOT_COLUMNS.find((column) => column.id === dimension)?.label ?? 'key' },
                        { key: 'trades', label: 'trades' },
                        { key: 'grossPnl', label: 'gross p&l' },
                        { key: 'fees', label: 'fees' },
                        { key: 'netPnl', label: 'net p&l' },
                        { key: 'winRate', label: 'win rate' },
                        { key: 'volumeUsd', label: 'volume' },
                      ] as Array<{ key: PivotSortKey; label: string }>
                    ).map((column) => (
                      <th
                        key={column.key}
                        scope="col"
                        className={column.key === 'key' || column.key === 'trades' ? '' : 'ta-r'}
                        aria-sort={
                          sortKey === column.key ? (sortAsc ? 'ascending' : 'descending') : 'none'
                        }
                      >
                        <button
                          type="button"
                          className={`adm-th-btn ${sortKey === column.key ? 'adm-th-btn--on' : ''}`}
                          onClick={() => toggleSort(column.key)}
                          aria-label={`Sort by ${column.label.replace('&', 'and')}`}
                        >
                          {column.label}
                          <span className="adm-th-btn__dir" aria-hidden="true">
                            {sortKey === column.key ? (sortAsc ? '↑' : '↓') : '↕'}
                          </span>
                        </button>
                      </th>
                    ))}
                  </tr>
                </thead>
                <tbody>
                  {sortedRows.map((row) => (
                    <tr key={row.key}>
                      <td className="sym">{row.key}</td>
                      <td className="num dim">{row.trades}</td>
                      <td className={`ta-r num ${row.grossPnl > 0 ? 'pos' : row.grossPnl < 0 ? 'neg' : ''}`}>
                        {fmtSignedUsd(row.grossPnl)}
                      </td>
                      <td className="ta-r num dim">${fmtUsd(row.fees)}</td>
                      <td className={`ta-r num ${row.netPnl > 0 ? 'pos' : row.netPnl < 0 ? 'neg' : ''}`}>
                        {fmtSignedUsd(row.netPnl)}
                      </td>
                      <td className="ta-r num dim">{fmtRatio(row.winRate)}</td>
                      <td className="ta-r num dim">{fmtVolume(row.volumeUsd)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </Panel>

        <div className="admin__grid">
          <Panel
            title="Risk"
            hint={usingLive ? 'live engine' : 'simulation'}
            actions={
              <span className={`breaker breaker--${risk?.breaker_state ?? 'unknown'} breaker--sm`}>
                <span className="breaker__v">
                  {risk ? titleise(risk.breaker_state) : 'demo'}
                </span>
              </span>
            }
          >
            {risk ? (
              <>
                <div className={`risk__breaker risk__breaker--${risk.breaker_state}`}>
                  <span className="risk__breaker-k">circuit breaker</span>
                  <span className="risk__breaker-v">{titleise(risk.breaker_state)}</span>
                  <span className="risk__breaker-reason">
                    {risk.breaker_reason ?? 'no reason recorded'}
                  </span>
                </div>
                <div className="stats stats--risk adm-risk-stats">
                  <div className="stat">
                    <span className="stat__k">equity</span>
                    <span className="stat__v num">${fmtUsd(risk.equity_usd)}</span>
                  </div>
                  <div className="stat">
                    <span className="stat__k">drawdown</span>
                    <span className={`stat__v num ${risk.drawdown_pct > 0 ? 'neg' : ''}`}>
                      {fmtPct(risk.drawdown_pct)}
                    </span>
                  </div>
                  <div className="stat">
                    <span className="stat__k">daily p&amp;l</span>
                    <span
                      className={`stat__v num ${
                        risk.daily_pnl_usd > 0 ? 'pos' : risk.daily_pnl_usd < 0 ? 'neg' : ''
                      }`}
                    >
                      {fmtSignedUsd(risk.daily_pnl_usd)}
                    </span>
                  </div>
                  <div className="stat">
                    <span className="stat__k">gross exposure</span>
                    <span className="stat__v num">${fmtUsd(risk.gross_exposure_usd)}</span>
                  </div>
                  <div className="stat">
                    <span className="stat__k">open positions</span>
                    <span className="stat__v num">{risk.open_positions}</span>
                  </div>
                  <div className="stat">
                    <span className="stat__k">peak equity</span>
                    <span className="stat__v num">${fmtUsd(risk.peak_equity_usd)}</span>
                  </div>
                </div>
              </>
            ) : (
              <>
                <div className="risk__breaker risk__breaker--closed">
                  <span className="risk__breaker-k">circuit breaker</span>
                  <span className="risk__breaker-v">demo</span>
                  <span className="risk__breaker-reason">
                    the simulation engine does not publish a breaker state here
                  </span>
                </div>
                <div className="stats stats--risk adm-risk-stats">
                  <div className="stat">
                    <span className="stat__k">equity</span>
                    <span className="stat__v num">${fmtUsd(equitySeries.length ? equitySeries[equitySeries.length - 1] : baseEquity)}</span>
                  </div>
                  <div className="stat">
                    <span className="stat__k">drawdown</span>
                    <span className={`stat__v num ${(demoRisk?.drawdown ?? 0) > 0 ? 'neg' : ''}`}>
                      {fmtPct(demoRisk?.drawdown ?? 0)}
                    </span>
                  </div>
                  <div className="stat">
                    <span className="stat__k">daily p&amp;l</span>
                    <span
                      className={`stat__v num ${
                        (demoRisk?.dailyPnl ?? 0) > 0 ? 'pos' : (demoRisk?.dailyPnl ?? 0) < 0 ? 'neg' : ''
                      }`}
                    >
                      {fmtSignedUsd(demoRisk?.dailyPnl ?? 0)}
                    </span>
                  </div>
                  <div className="stat">
                    <span className="stat__k">gross exposure</span>
                    <span className="stat__v num">
                      {grossExposure === null ? '—' : `$${fmtUsd(grossExposure)}`}
                    </span>
                  </div>
                  <div className="stat">
                    <span className="stat__k">open positions</span>
                    <span className="stat__v num">{positions.length}</span>
                  </div>
                  <div className="stat">
                    <span className="stat__k">avg win / loss</span>
                    <span className="stat__v num">
                      {fmtSignedUsd(summary.avgWin)} / {fmtSignedUsd(summary.avgLoss)}
                    </span>
                  </div>
                </div>
              </>
            )}
          </Panel>

          <Panel title="Logs" hint={`${recentLogs.length} lines`} bodyClassName="panel__bd--flush">
            {recentLogs.length === 0 ? (
              <div className="empty empty--inline">
                <p className="empty__title">No log lines</p>
                <p className="empty__hint">The engine pushes its reasoning here as it runs.</p>
              </div>
            ) : (
              <div className="adm-logs">
                {recentLogs.map((entry) => (
                  <div
                    className="logrow"
                    key={entry.id || `${entry.ts_ms}-${entry.message.slice(0, 24)}`}
                  >
                    <span className="logrow__ts num">{fmtClock(entry.ts_ms)}</span>
                    <span className="logrow__src">{entry.source}</span>
                    <span className={`logrow__lvl ${levelClass(entry.level)}`}>{entry.level}</span>
                    <span className="logrow__msg" title={entry.message}>
                      {truncate(entry.message, 120)}
                    </span>
                  </div>
                ))}
              </div>
            )}
          </Panel>
        </div>

        <p className="admin__foot dim">
          Source: <span className="num">{sourceLabel}</span> · prices and fills shown here come from
          the {usingLive ? 'live core engine' : 'in-browser simulation'}. Nothing in this panel is
          real money.
        </p>
      </div>

      <footer className="admin__foot-bar">
        <span className="foot__item">
          source <span className="num">{sourceLabel}</span>
        </span>
        <span className="foot__item">
          fills <span className="num">{summary.totalTrades}</span>
        </span>
        <span className="foot__item">
          avg win <span className="num pos">{fmtSignedUsd(summary.avgWin)}</span>
        </span>
        <span className="foot__item">
          avg loss <span className="num neg">{fmtSignedUsd(summary.avgLoss)}</span>
        </span>
        <span className="foot__item">
          last price <span className="num">{fmtPrice(pivotTrades.length ? pivotTrades[0].price : null)}</span>
        </span>
      </footer>
    </div>
  );
}

export default AdminPanel;

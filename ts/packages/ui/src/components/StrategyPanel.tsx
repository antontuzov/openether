import { useEffect, useRef, useState } from 'react';
import type { BreakerState, StrategyInfo } from '../api/types';
import { fmtAgo, fmtDateTime, fmtInt, shortHash } from '../lib/format';
import { Panel } from './Panel';

/** Result of a server round-trip shown inline in the panel. */
export interface ActionOutcome {
  ok: boolean;
  message: string;
}

interface StrategyPanelProps {
  strategy: StrategyInfo | null;
  breakerState: BreakerState | null;
  onReload: (path: string) => Promise<ActionOutcome>;
  onResetBreaker: () => Promise<ActionOutcome>;
}

const RESET_ARM_WINDOW_MS = 6_000;

export function StrategyPanel({ strategy, breakerState, onReload, onResetBreaker }: StrategyPanelProps) {
  const [wasmPath, setWasmPath] = useState('');
  const [reloadBusy, setReloadBusy] = useState(false);
  const [reloadOutcome, setReloadOutcome] = useState<ActionOutcome | null>(null);
  const [resetBusy, setResetBusy] = useState(false);
  const [resetArmed, setResetArmed] = useState(false);
  const [resetOutcome, setResetOutcome] = useState<ActionOutcome | null>(null);
  const armTimer = useRef<number | undefined>(undefined);

  useEffect(() => {
    return () => {
      if (armTimer.current !== undefined) window.clearTimeout(armTimer.current);
    };
  }, []);

  const handleReload = async (): Promise<void> => {
    const path = wasmPath.trim();
    if (!path) {
      setReloadOutcome({ ok: false, message: 'enter an absolute path to the .wasm file first' });
      return;
    }
    setReloadBusy(true);
    setReloadOutcome(null);
    try {
      setReloadOutcome(await onReload(path));
    } finally {
      setReloadBusy(false);
    }
  };

  const disarm = (): void => {
    if (armTimer.current !== undefined) window.clearTimeout(armTimer.current);
    armTimer.current = undefined;
    setResetArmed(false);
  };

  const handleReset = async (): Promise<void> => {
    if (!resetArmed) {
      setResetArmed(true);
      setResetOutcome(null);
      armTimer.current = window.setTimeout(() => setResetArmed(false), RESET_ARM_WINDOW_MS);
      return;
    }
    disarm();
    setResetBusy(true);
    setResetOutcome(null);
    try {
      setResetOutcome(await onResetBreaker());
    } finally {
      setResetBusy(false);
    }
  };

  return (
    <Panel
      title="Strategy"
      hint={strategy ? strategy.name : 'no strategy loaded'}
      actions={<span className="panel__badge">wasm</span>}
    >
      <div className="kv">
        <span className="kv__k">name</span>
        <span className="kv__v">{strategy?.name ?? '—'}</span>
        <span className="kv__k">loaded</span>
        <span className="kv__v num" title={fmtDateTime(strategy?.loaded_at_ms)}>
          {strategy ? `${fmtAgo(strategy.loaded_at_ms)} · ${fmtDateTime(strategy.loaded_at_ms)}` : '—'}
        </span>
        <span className="kv__k">wasm sha256</span>
        <span className="kv__v num kv__v--mono" title={strategy?.wasm_sha256}>
          {shortHash(strategy?.wasm_sha256)}
        </span>
        <span className="kv__k">ticks processed</span>
        <span className="kv__v num">{fmtInt(strategy?.ticks_processed)}</span>
        <span className="kv__k">fuel used</span>
        <span className="kv__v num">{fmtInt(strategy?.fuel_used)}</span>
      </div>

      <div className="action">
        <label className="action__label" htmlFor="wasm-path">
          strategy wasm path
        </label>
        <div className="action__row">
          <input
            id="wasm-path"
            className="input"
            type="text"
            spellCheck={false}
            autoComplete="off"
            placeholder="/abs/path/strategy.wasm"
            value={wasmPath}
            onChange={(event) => setWasmPath(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === 'Enter') void handleReload();
            }}
          />
          <button
            type="button"
            className="btn btn--primary"
            onClick={() => void handleReload()}
            disabled={reloadBusy}
          >
            {reloadBusy ? 'reloading…' : 'reload'}
          </button>
        </div>
        {reloadOutcome ? (
          <p className={`inline-msg ${reloadOutcome.ok ? 'inline-msg--ok' : 'inline-msg--err'}`}>
            {reloadOutcome.message}
          </p>
        ) : null}
      </div>

      <div className="action action--split">
        <div className="action__meta">
          <span className="action__label">circuit breaker</span>
          <span className={`action__state action__state--${breakerState ?? 'unknown'}`}>
            {breakerState ? breakerState.replace('_', ' ') : 'unknown'}
          </span>
        </div>
        <button
          type="button"
          className={`btn btn--danger${resetArmed ? ' btn--armed' : ''}`}
          onClick={() => void handleReset()}
          disabled={resetBusy}
        >
          {resetBusy ? 'resetting…' : resetArmed ? 'Confirm reset?' : 'Reset breaker'}
        </button>
      </div>
      {resetOutcome ? (
        <p className={`inline-msg ${resetOutcome.ok ? 'inline-msg--ok' : 'inline-msg--err'}`}>
          {resetOutcome.message}
        </p>
      ) : null}
    </Panel>
  );
}

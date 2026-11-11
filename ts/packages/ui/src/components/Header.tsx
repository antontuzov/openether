import type { BreakerState } from '../api/types';
import type { ConnectionState } from '../hooks/useDashboard';
import type { Theme } from '../hooks/useTheme';
import { fmtSignedUsd, fmtUsdPrefixed, titleise } from '../lib/format';

interface HeaderProps {
  venueLabel: string;
  venueLive: boolean;
  connection: ConnectionState | 'sim';
  equity: number | null;
  dayPnl: number | null;
  breakerState: BreakerState | null;
  engineState: string | null;
  theme: Theme;
  onToggleTheme: () => void;
}

const CONNECTION_LABEL: Record<ConnectionState | 'sim', string> = {
  live: 'live stream',
  polling: 'polling only',
  down: 'core unreachable',
  sim: 'simulated',
};

const CONNECTION_TITLE: Record<ConnectionState | 'sim', string> = {
  live: 'WebSocket connected — frames are streaming',
  polling: 'WebSocket down — falling back to REST polling',
  down: 'Core engine is unreachable',
  sim: 'Browser simulation — no exchange is connected',
};

export function Header({
  venueLabel,
  venueLive,
  connection,
  equity,
  dayPnl,
  breakerState,
  engineState,
  theme,
  onToggleTheme,
}: HeaderProps) {
  const pnlTone = dayPnl === null ? 'flat' : dayPnl > 0 ? 'up' : dayPnl < 0 ? 'down' : 'flat';
  const breakerLabel = breakerState ? titleise(breakerState) : 'unknown';
  const breakerClass = breakerState ?? 'unknown';

  return (
    <header className="hdr">
      <div className="hdr__brand">
        <span className="hdr__mark" aria-hidden="true" />
        <h1 className="hdr__name">openEther</h1>
        <span className={`hdr__venue ${venueLive ? 'hdr__venue--live' : 'hdr__venue--paper'}`}>
          {venueLabel}
        </span>
        {engineState ? <span className="hdr__engine">engine: {engineState}</span> : null}
      </div>

      <div className="hdr__stats">
        <div className="hdr__stat">
          <span className="hdr__stat-k">equity</span>
          <span className="hdr__stat-v num">{fmtUsdPrefixed(equity)}</span>
        </div>
        <div className="hdr__stat">
          <span className="hdr__stat-k">day p&amp;l</span>
          <span className={`hdr__stat-v num ${pnlTone === 'up' ? 'pos' : pnlTone === 'down' ? 'neg' : ''}`}>
            {fmtSignedUsd(dayPnl)}
          </span>
        </div>
      </div>

      <div className="hdr__right">
        <button
          type="button"
          className="icon-btn"
          onClick={onToggleTheme}
          title={theme === 'dark' ? 'Switch to the light theme' : 'Switch to the dark theme'}
          aria-label={theme === 'dark' ? 'Switch to the light theme' : 'Switch to the dark theme'}
        >
          <span aria-hidden="true">{theme === 'dark' ? '☀' : '☾'}</span>
          <span className="icon-btn__text">{theme === 'dark' ? 'light' : 'dark'}</span>
        </button>
        <span className={`conn conn--${connection}`} title={CONNECTION_TITLE[connection]}>
          <span className="conn__dot" aria-hidden="true" />
          <span className="conn__label">{CONNECTION_LABEL[connection]}</span>
        </span>
        <span className={`breaker breaker--${breakerClass}`} title={`Circuit breaker: ${breakerLabel}`}>
          <span className="breaker__k">breaker</span>
          <span className="breaker__v">{breakerLabel}</span>
        </span>
      </div>
    </header>
  );
}

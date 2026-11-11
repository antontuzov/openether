import { useEffect, useMemo, useRef, useState } from 'react';
import type { LogEntry } from '../api/types';
import { fmtClock } from '../lib/format';
import { Panel } from './Panel';

interface LogPanelProps {
  logs: LogEntry[];
}

const SOURCE_CLASS: Record<string, string> = {
  agent: 'src--agent',
  risk: 'src--risk',
  strategy: 'src--strategy',
  engine: 'src--engine',
  mcp: 'src--mcp',
};

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

export function LogPanel({ logs }: LogPanelProps) {
  const [source, setSource] = useState<string>('all');
  const [paused, setPaused] = useState(false);
  const viewport = useRef<HTMLDivElement | null>(null);

  const sources = useMemo(() => {
    const found = new Set<string>();
    for (const entry of logs) found.add(entry.source);
    return ['all', ...Array.from(found).sort()];
  }, [logs]);

  // Newest first from the API; the tape reads top-down, so flip it.
  const ordered = useMemo(() => logs.slice().reverse(), [logs]);

  const visible = useMemo(
    () => (source === 'all' ? ordered : ordered.filter((entry) => entry.source === source)),
    [ordered, source],
  );

  useEffect(() => {
    const element = viewport.current;
    if (!element || paused) return;
    element.scrollTop = element.scrollHeight;
  }, [visible, paused]);

  const handleScroll = (): void => {
    const element = viewport.current;
    if (!element) return;
    const distanceToBottom = element.scrollHeight - element.scrollTop - element.clientHeight;
    setPaused(distanceToBottom > 24);
  };

  const jumpToLatest = (): void => {
    const element = viewport.current;
    if (!element) return;
    element.scrollTop = element.scrollHeight;
    setPaused(false);
  };

  return (
    <Panel
      title="Agent log"
      hint={`${visible.length} entries`}
      className="panel--log"
      actions={
        paused ? (
          <button type="button" className="btn btn--tiny" onClick={jumpToLatest}>
            jump to latest
          </button>
        ) : (
          <span className="panel__badge panel__badge--live">streaming</span>
        )
      }
      bodyClassName="panel__bd--flush"
    >
      <div className="logs__filters">
        {sources.map((entry) => (
          <button
            type="button"
            key={entry}
            className={`chip${source === entry ? ' chip--active' : ''}`}
            onClick={() => setSource(entry)}
          >
            {entry}
          </button>
        ))}
      </div>

      <div className="logs" ref={viewport} onScroll={handleScroll}>
        {visible.length === 0 ? (
          <div className="empty empty--inline">
            <p className="empty__title">No log entries</p>
            <p className="empty__hint">
              The engine and the AI agent push their reasoning here in real time.
            </p>
          </div>
        ) : (
          visible.map((entry) => (
            <div className="logrow" key={entry.id || `${entry.ts_ms}-${entry.message.slice(0, 24)}`}>
              <span className="logrow__ts num">{fmtClock(entry.ts_ms)}</span>
              <span className={`logrow__src ${SOURCE_CLASS[entry.source] ?? ''}`}>{entry.source}</span>
              <span className={`logrow__lvl ${levelClass(entry.level)}`}>{entry.level}</span>
              <span className="logrow__msg">{entry.message}</span>
            </div>
          ))
        )}
      </div>
    </Panel>
  );
}

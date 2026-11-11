import { useMemo } from 'react';

interface EquityChartProps {
  points: number[];
  height?: number;
}

const VIEW_W = 1000;
const VIEW_H = 220;
const PAD_Y = 10;
/** Only one equity chart is mounted at a time, so a static id is safe. */
const FILL_ID = 'openether-equity-fill';

interface Pt {
  x: number;
  y: number;
}

/** Hand-rolled SVG area chart of the equity curve. */
export function EquityChart({ points, height = 190 }: EquityChartProps) {
  const geometry = useMemo(() => {
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

    const coords: Pt[] = points.map((value, index) => ({
      x: (index / (points.length - 1)) * VIEW_W,
      y: PAD_Y + (1 - (value - low) / range) * (VIEW_H - PAD_Y * 2),
    }));

    let line = `M ${coords[0].x.toFixed(2)} ${coords[0].y.toFixed(2)}`;
    for (let i = 1; i < coords.length; i += 1) {
      const previous = coords[i - 1];
      const current = coords[i];
      const midX = (previous.x + current.x) / 2;
      const midY = (previous.y + current.y) / 2;
      line += ` Q ${previous.x.toFixed(2)} ${previous.y.toFixed(2)} ${midX.toFixed(2)} ${midY.toFixed(2)}`;
    }
    const last = coords[coords.length - 1];
    line += ` L ${last.x.toFixed(2)} ${last.y.toFixed(2)}`;

    const area = `${line} L ${last.x.toFixed(2)} ${VIEW_H} L ${coords[0].x.toFixed(2)} ${VIEW_H} Z`;

    return { coords, line, area, min, max, first: points[0], last: points[points.length - 1] };
  }, [points]);

  if (!geometry) {
    return (
      <div className="chart chart--empty" style={{ height }}>
        <svg className="chart__svg" viewBox={`0 0 ${VIEW_W} ${VIEW_H}`} preserveAspectRatio="none" aria-hidden="true">
          <line
            x1="0"
            y1={VIEW_H / 2}
            x2={VIEW_W}
            y2={VIEW_H / 2}
            className="chart__flat"
            vectorEffect="non-scaling-stroke"
          />
        </svg>
        <span className="chart__placeholder">collecting equity history…</span>
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
        viewBox={`0 0 ${VIEW_W} ${VIEW_H}`}
        preserveAspectRatio="none"
        role="img"
        aria-label={`Equity curve, ${points.length} samples, change ${delta.toFixed(2)}`}
      >
        <defs>
          <linearGradient id={FILL_ID} x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor={stroke} stopOpacity="0.2" />
            <stop offset="100%" stopColor={stroke} stopOpacity="0" />
          </linearGradient>
        </defs>

        {[0.25, 0.5, 0.75].map((ratio) => (
          <line
            key={ratio}
            className="chart__grid"
            x1="0"
            y1={VIEW_H * ratio}
            x2={VIEW_W}
            y2={VIEW_H * ratio}
            vectorEffect="non-scaling-stroke"
          />
        ))}

        <path d={geometry.area} fill={`url(#${FILL_ID})`} />
        <path d={geometry.line} className="chart__line" stroke={stroke} vectorEffect="non-scaling-stroke" />
      </svg>

      <div className="chart__overlay">
        <span className="chart__tag chart__tag--top num">{geometry.max.toFixed(2)}</span>
        <span className="chart__tag chart__tag--bottom num">{geometry.min.toFixed(2)}</span>
      </div>

      <div className="chart__legend">
        <span className="chart__legend-item">
          window <span className={`num ${delta >= 0 ? 'pos' : 'neg'}`}>{delta >= 0 ? '+' : '-'}${Math.abs(delta).toFixed(2)}</span>
        </span>
        <span className="chart__legend-item chart__legend-item--dim">{points.length} samples</span>
      </div>
    </div>
  );
}

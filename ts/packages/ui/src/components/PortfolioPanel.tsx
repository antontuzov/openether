import type { PortfolioSnapshot } from '../api/types';
import { fmtPct, fmtSignedUsd, fmtUsdPrefixed } from '../lib/format';
import { EquityChart } from './EquityChart';
import { Panel } from './Panel';

interface PortfolioPanelProps {
  portfolio: PortfolioSnapshot | null;
  equitySeries: number[];
}

export function PortfolioPanel({ portfolio, equitySeries }: PortfolioPanelProps) {
  const unrealized = portfolio?.unrealized_pnl_usd ?? null;
  const realized = portfolio?.realized_pnl_usd ?? null;
  const totalPct = portfolio?.total_pnl_pct ?? null;

  const tone = (value: number | null): string =>
    value === null ? '' : value > 0 ? 'pos' : value < 0 ? 'neg' : '';

  return (
    <Panel
      title="Portfolio"
      hint={portfolio ? `${portfolio.venue} venue` : 'awaiting snapshot'}
      actions={
        <span className="panel__badge num">
          {portfolio ? `${portfolio.positions.length} open` : 'no data'}
        </span>
      }
    >
      <div className="stats stats--portfolio">
        <div className="stat stat--hero">
          <span className="stat__k">equity</span>
          <span className="stat__v num">{fmtUsdPrefixed(portfolio?.equity_usd ?? null)}</span>
        </div>
        <div className="stat">
          <span className="stat__k">cash</span>
          <span className="stat__v num">{fmtUsdPrefixed(portfolio?.cash_usd ?? null)}</span>
        </div>
        <div className="stat">
          <span className="stat__k">realized p&amp;l</span>
          <span className={`stat__v num ${tone(realized)}`}>{fmtSignedUsd(realized)}</span>
        </div>
        <div className="stat">
          <span className="stat__k">unrealized p&amp;l</span>
          <span className={`stat__v num ${tone(unrealized)}`}>{fmtSignedUsd(unrealized)}</span>
        </div>
        <div className="stat">
          <span className="stat__k">total p&amp;l %</span>
          <span className={`stat__v num ${tone(totalPct)}`}>{fmtPct(totalPct)}</span>
        </div>
      </div>

      <EquityChart points={equitySeries} />
    </Panel>
  );
}

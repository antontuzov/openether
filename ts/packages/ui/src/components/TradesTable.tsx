import type { TradeRecord } from '../api/types';
import { fmtClock, fmtPrice, fmtQty, fmtSignedUsd, truncate } from '../lib/format';
import { Panel } from './Panel';

interface TradesTableProps {
  trades: TradeRecord[];
}

const RATIONALE_MAX = 68;

export function TradesTable({ trades }: TradesTableProps) {
  return (
    <Panel title="Trade history" hint="newest first" bodyClassName="panel__bd--flush">
      {trades.length === 0 ? (
        <div className="empty">
          <p className="empty__title">No fills yet</p>
          <p className="empty__hint">
            Every executed order lands here with the agent&apos;s rationale, fee and realised P&amp;L.
          </p>
        </div>
      ) : (
        <div className="tbl-wrap tbl-wrap--tall">
          <table className="tbl tbl--trades">
            <colgroup>
              <col style={{ width: '12%' }} />
              <col style={{ width: '15%' }} />
              <col style={{ width: '9%' }} />
              <col style={{ width: '9%' }} />
              <col style={{ width: '12%' }} />
              <col style={{ width: '9%' }} />
              <col style={{ width: '12%' }} />
              <col style={{ width: '22%' }} />
            </colgroup>
            <thead>
              <tr>
                <th scope="col">time</th>
                <th scope="col">symbol</th>
                <th scope="col">side</th>
                <th scope="col" className="ta-r">
                  qty
                </th>
                <th scope="col" className="ta-r">
                  price
                </th>
                <th scope="col" className="ta-r">
                  fee
                </th>
                <th scope="col" className="ta-r">
                  r. p&amp;l
                </th>
                <th scope="col">rationale</th>
              </tr>
            </thead>
            <tbody>
              {trades.map((trade) => (
                <tr key={trade.id || `${trade.ts_ms}-${trade.symbol}-${trade.price}`}>
                  <td className="num dim">{fmtClock(trade.ts_ms)}</td>
                  <td className="sym">{trade.symbol}</td>
                  <td>
                    <span className={`side side--${trade.side === 'buy' ? 'long' : 'short'}`}>
                      {trade.side.toUpperCase()}
                    </span>
                  </td>
                  <td className="ta-r num">{fmtQty(trade.qty)}</td>
                  <td className="ta-r num">{fmtPrice(trade.price)}</td>
                  <td className="ta-r num dim">{fmtQty(trade.fee)}</td>
                  <td
                    className={`ta-r num ${
                      trade.realized_pnl > 0 ? 'pos' : trade.realized_pnl < 0 ? 'neg' : ''
                    }`}
                  >
                    {fmtSignedUsd(trade.realized_pnl)}
                  </td>
                  <td className="rationale" title={trade.rationale || undefined}>
                    {trade.rationale ? truncate(trade.rationale, RATIONALE_MAX) : '—'}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </Panel>
  );
}

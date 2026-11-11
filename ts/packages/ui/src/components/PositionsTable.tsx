import type { Position } from '../api/types';
import { fmtPrice, fmtQty, fmtSignedUsd } from '../lib/format';
import { Panel } from './Panel';

interface PositionsTableProps {
  positions: Position[];
}

function sideOf(qty: number): 'long' | 'short' | 'flat' {
  if (qty > 0) return 'long';
  if (qty < 0) return 'short';
  return 'flat';
}

export function PositionsTable({ positions }: PositionsTableProps) {
  return (
    <Panel title="Positions" hint={`${positions.length} open`} bodyClassName="panel__bd--flush">
      {positions.length === 0 ? (
        <div className="empty">
          <p className="empty__title">No open positions</p>
          <p className="empty__hint">
            The engine has no exposure right now — positions appear here as soon as the agent or
            strategy opens one.
          </p>
        </div>
      ) : (
        <div className="tbl-wrap">
          <table className="tbl tbl--positions">
            <colgroup>
              <col style={{ width: '20%' }} />
              <col style={{ width: '11%' }} />
              <col style={{ width: '13%' }} />
              <col style={{ width: '14%' }} />
              <col style={{ width: '14%' }} />
              <col style={{ width: '14%' }} />
              <col style={{ width: '14%' }} />
            </colgroup>
            <thead>
              <tr>
                <th scope="col">symbol</th>
                <th scope="col">side</th>
                <th scope="col" className="ta-r">
                  size
                </th>
                <th scope="col" className="ta-r">
                  entry
                </th>
                <th scope="col" className="ta-r">
                  mark
                </th>
                <th scope="col" className="ta-r">
                  notional
                </th>
                <th scope="col" className="ta-r">
                  u. p&amp;l
                </th>
              </tr>
            </thead>
            <tbody>
              {positions.map((position) => {
                const side = sideOf(position.qty);
                return (
                  <tr key={`${position.venue}-${position.symbol}`}>
                    <td className="sym">{position.symbol}</td>
                    <td>
                      <span className={`side side--${side}`}>{side.toUpperCase()}</span>
                    </td>
                    <td className="ta-r num">{fmtQty(position.qty)}</td>
                    <td className="ta-r num dim">{fmtPrice(position.avg_entry)}</td>
                    <td className="ta-r num">{fmtPrice(position.mark_price)}</td>
                    <td className="ta-r num dim">${fmtPrice(position.notional_usd)}</td>
                    <td className={`ta-r num ${position.unrealized_pnl > 0 ? 'pos' : position.unrealized_pnl < 0 ? 'neg' : ''}`}>
                      {fmtSignedUsd(position.unrealized_pnl)}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </Panel>
  );
}

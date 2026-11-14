import { useState } from 'react';
import type { TrackedOrder } from '../api/types';
import { fmtClock, fmtPrice, fmtQty } from '../lib/format';
import { avgFillPrice, filledQty, isActive, remainingQty, totalFee } from '../lib/orders';
import { Panel } from './Panel';
import type { ActionOutcome } from './StrategyPanel';

interface OrdersPanelProps {
  orders: TrackedOrder[];
  onCancel: (orderId: string) => Promise<ActionOutcome>;
}

const STATUS_LABEL: Record<TrackedOrder['status'], string> = {
  accepted: 'resting',
  partially_filled: 'partial',
  filled: 'filled',
  cancelled: 'cancelled',
  rejected: 'rejected',
};

/**
 * What the engine asked the venue to do, and what the venue confirmed.
 *
 * The filled column is derived from the venue's own fills, never from the
 * quantity requested: a resting order shows 0 filled and is not a position.
 * Cancelling is a two-step action because it is a real instruction to a real
 * venue, and the venue has to confirm it before the order leaves this list.
 */
export function OrdersPanel({ orders, onCancel }: OrdersPanelProps) {
  const [armedId, setArmedId] = useState<string | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [outcome, setOutcome] = useState<ActionOutcome | null>(null);

  const resting = orders.filter(isActive).length;

  const handleCancel = async (orderId: string): Promise<void> => {
    if (armedId !== orderId) {
      setArmedId(orderId);
      setOutcome(null);
      return;
    }
    setArmedId(null);
    setBusyId(orderId);
    setOutcome(null);
    try {
      setOutcome(await onCancel(orderId));
    } finally {
      setBusyId(null);
    }
  };

  return (
    <Panel
      title="Orders"
      hint={orders.length === 0 ? 'newest first' : `${resting} resting · newest first`}
      bodyClassName={orders.length > 0 ? 'panel__bd--flush' : undefined}
      actions={<span className="panel__badge num">{orders.length}</span>}
    >
      {orders.length === 0 ? (
        <div className="empty">
          <p className="empty__title">No orders yet</p>
          <p className="empty__hint">
            Every order the engine submits lands here with the fills the venue confirms. A resting
            order is not a position — it only becomes one when the venue reports a fill.
          </p>
        </div>
      ) : (
        <>
          <div className="tbl-wrap tbl-wrap--tall">
            <table className="tbl tbl--orders">
              <colgroup>
                <col style={{ width: '11%' }} />
                <col style={{ width: '14%' }} />
                <col style={{ width: '8%' }} />
                <col style={{ width: '9%' }} />
                <col style={{ width: '15%' }} />
                <col style={{ width: '12%' }} />
                <col style={{ width: '12%' }} />
                <col style={{ width: '19%' }} />
              </colgroup>
              <thead>
                <tr>
                  <th scope="col">time</th>
                  <th scope="col">symbol</th>
                  <th scope="col">side</th>
                  <th scope="col">type</th>
                  <th scope="col" className="ta-r">
                    filled / qty
                  </th>
                  <th scope="col" className="ta-r">
                    price
                  </th>
                  <th scope="col">status</th>
                  <th scope="col" className="ta-r">
                    action
                  </th>
                </tr>
              </thead>
              <tbody>
                {orders.map((order) => {
                  const filled = filledQty(order);
                  const active = isActive(order);
                  const average = avgFillPrice(order);
                  const price = filled > 0 ? average : (order.trigger_price ?? order.limit_price);
                  return (
                    <tr key={order.order_id}>
                      <td className="num dim" title={order.order_id}>
                        {fmtClock(order.submitted_ts_ms)}
                      </td>
                      <td className="sym">{order.symbol}</td>
                      <td>
                        <span className={`side side--${order.side === 'buy' ? 'long' : 'short'}`}>
                          {order.side.toUpperCase()}
                        </span>
                      </td>
                      <td className="dim">
                        {order.order_type}
                        {active && order.oco_group !== null ? (
                          <span className="orders__rest" title={`OCO group ${order.oco_group}`}>
                            {' '}
                            · oco
                          </span>
                        ) : null}
                      </td>
                      <td className="ta-r num">
                        {fmtQty(filled)} / {fmtQty(order.qty)}
                        {filled > 0 && active ? (
                          <span className="orders__rest"> · {fmtQty(remainingQty(order))} left</span>
                        ) : null}
                      </td>
                      <td className="ta-r num dim">
                        {price === null ? 'market' : `$${fmtPrice(price)}`}
                        {filled === 0 && order.trigger_price !== null ? (
                          <span className="orders__rest"> · trigger</span>
                        ) : null}
                        {filled > 0 ? (
                          <span className="orders__rest"> · fee {fmtQty(totalFee(order))}</span>
                        ) : null}
                      </td>
                      <td>
                        <span className={`order-state order-state--${order.status}`}>
                          {STATUS_LABEL[order.status]}
                        </span>
                      </td>
                      <td className="ta-r">
                        {active ? (
                          <button
                            type="button"
                            className={`btn btn--tiny${armedId === order.order_id ? ' btn--danger btn--armed' : ''}`}
                            onClick={() => void handleCancel(order.order_id)}
                            disabled={busyId === order.order_id}
                          >
                            {busyId === order.order_id
                              ? 'cancelling…'
                              : armedId === order.order_id
                                ? 'confirm?'
                                : 'cancel'}
                          </button>
                        ) : (
                          <span className="dim">—</span>
                        )}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
          {outcome ? (
            <p className={`inline-msg orders__msg ${outcome.ok ? 'inline-msg--ok' : 'inline-msg--err'}`}>
              {outcome.message}
            </p>
          ) : null}
        </>
      )}
    </Panel>
  );
}

export default OrdersPanel;
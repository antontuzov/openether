/**
 * Order arithmetic, mirroring `TrackedOrder` in `rust/crates/core/src/types.rs`.
 *
 * The engine books only fills the venue confirmed, so everything here is
 * derived from `fills` — never from the quantity the order asked for. A resting
 * order has filled nothing, and saying otherwise would show a position that
 * does not exist.
 */

import type { TrackedOrder } from '../api/types';

/** Quantity the venue has confirmed. */
export function filledQty(order: TrackedOrder): number {
  return order.fills.reduce((total, fill) => total + fill.qty, 0);
}

/** Quantity still resting at the venue. */
export function remainingQty(order: TrackedOrder): number {
  return Math.max(0, order.qty - filledQty(order));
}

/** Volume-weighted average of the confirmed fills, or 0 when none. */
export function avgFillPrice(order: TrackedOrder): number {
  const qty = filledQty(order);
  if (qty <= 0) return 0;
  return order.fills.reduce((total, fill) => total + fill.qty * fill.price, 0) / qty;
}

/** Fees the venue charged across every confirmed fill. */
export function totalFee(order: TrackedOrder): number {
  return order.fills.reduce((total, fill) => total + fill.fee, 0);
}

/** Still waiting on the venue: neither fully filled nor retired. */
export function isActive(order: TrackedOrder): boolean {
  return order.status === 'accepted' || order.status === 'partially_filled';
}
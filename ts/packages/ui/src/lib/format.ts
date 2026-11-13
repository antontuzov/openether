/** Small formatting helpers shared by the panels. All of them are total. */

const USD_PRECISE = new Intl.NumberFormat('en-US', {
  minimumFractionDigits: 4,
  maximumFractionDigits: 6,
});

const INT = new Intl.NumberFormat('en-US', { maximumFractionDigits: 0 });

/** True when the value is a usable finite number. */
export function isNum(value: number | null | undefined): value is number {
  return typeof value === 'number' && Number.isFinite(value);
}

/** `1234.5` → `1,234.50`; non-numbers become an em dash. */
export function fmtUsd(value: number | null | undefined, decimals = 2): string {
  if (!isNum(value)) return '—';
  return value.toLocaleString('en-US', {
    minimumFractionDigits: decimals,
    maximumFractionDigits: decimals,
  });
}

/** Signed money, e.g. `+$120.50` / `-$4.10`. */
export function fmtSignedUsd(value: number | null | undefined, decimals = 2): string {
  if (!isNum(value)) return '—';
  const sign = value >= 0 ? '+' : '-';
  return `${sign}$${fmtUsd(Math.abs(value), decimals)}`;
}

/** Money with a currency sign, e.g. `$1,234.50`; an em dash when unknown. */
export function fmtUsdPrefixed(value: number | null | undefined, decimals = 2): string {
  if (!isNum(value)) return '—';
  return `$${fmtUsd(value, decimals)}`;
}

/** Signed plain number, e.g. `+12.40%`. */
export function fmtSignedPct(value: number | null | undefined, decimals = 2): string {
  if (!isNum(value)) return '—';
  const sign = value >= 0 ? '+' : '-';
  return `${sign}${Math.abs(value).toFixed(decimals)}%`;
}

/** Unsigned percentage, e.g. `2.44%`. */
export function fmtPct(value: number | null | undefined, decimals = 2): string {
  if (!isNum(value)) return '—';
  return `${value.toFixed(decimals)}%`;
}

/** Quantity with up to 6 decimals, trailing zeros trimmed. */
export function fmtQty(value: number | null | undefined): string {
  if (!isNum(value)) return '—';
  const text = USD_PRECISE.format(value);
  return text.includes('.') ? text.replace(/0+$/, '').replace(/\.$/, '') : text;
}

/** Signed quantity, e.g. `+1.5` / `-1.5`. */
export function fmtSignedQty(value: number | null | undefined): string {
  if (!isNum(value)) return '—';
  const sign = value >= 0 ? '+' : '-';
  return `${sign}${fmtQty(Math.abs(value))}`;
}

/** Integer with thousands separators. */
export function fmtInt(value: number | null | undefined): string {
  if (!isNum(value)) return '—';
  return INT.format(value);
}

/** Price with enough precision for small caps. */
export function fmtPrice(value: number | null | undefined): string {
  if (!isNum(value)) return '—';
  const decimals = Math.abs(value) >= 1000 ? 2 : Math.abs(value) >= 1 ? 4 : 6;
  return value.toLocaleString('en-US', {
    minimumFractionDigits: decimals,
    maximumFractionDigits: decimals,
  });
}

/** `HH:MM:SS` in local time. */
export function fmtClock(tsMs: number | null | undefined): string {
  if (!isNum(tsMs) || tsMs <= 0) return '—';
  const date = new Date(tsMs);
  return date.toLocaleTimeString('en-GB', { hour12: false });
}

/** `YYYY-MM-DD HH:MM:SS` in local time. */
export function fmtDateTime(tsMs: number | null | undefined): string {
  if (!isNum(tsMs) || tsMs <= 0) return '—';
  const date = new Date(tsMs);
  const day = date.toLocaleDateString('en-CA');
  const clock = date.toLocaleTimeString('en-GB', { hour12: false });
  return `${day} ${clock}`;
}

/** Relative age, e.g. `3s ago`, `12m ago`. */
export function fmtAgo(tsMs: number | null | undefined, now = Date.now()): string {
  if (!isNum(tsMs) || tsMs <= 0) return '—';
  const seconds = Math.max(0, Math.round((now - tsMs) / 1000));
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  return `${Math.round(hours / 24)}d ago`;
}

/** Shorten a long string and add an ellipsis. */
export function truncate(text: string, max: number): string {
  if (text.length <= max) return text;
  return `${text.slice(0, Math.max(0, max - 1))}…`;
}

/** Shorten a sha256 digest to the conventional 12 hex characters. */
export function shortHash(hash: string | null | undefined): string {
  if (!hash) return '—';
  return hash.length > 12 ? hash.slice(0, 12) : hash;
}

/** Shorten an address the way the engine does: `0xA0b8…eB48`. */
export function shortAddress(address: string): string {
  const trimmed = address.trim();
  if (trimmed.length <= 10) return trimmed;
  return `${trimmed.slice(0, 6)}…${trimmed.slice(-4)}`;
}

/** `open` → `OPEN`, `half_open` → `HALF OPEN`. */
export function titleise(value: string): string {
  return value.replace(/_/g, ' ').toUpperCase();
}

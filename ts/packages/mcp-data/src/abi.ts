/**
 * A very small subset of the Ethereum ABI.
 *
 * Only the view functions this server needs are supported: `balanceOf`,
 * `decimals`, `name`, `symbol` and `totalSupply`. The selectors are the
 * well-known constants, so no keccak implementation is required.
 */

export const SELECTORS = {
  balanceOf: '0x70a08231',
  decimals: '0x313ce567',
  name: '0x06fdde03',
  symbol: '0x95d89b41',
  totalSupply: '0x18160ddd',
} as const;

function stripHex(value: string): string {
  return value.startsWith('0x') ? value.slice(2) : value;
}

/** Encodes a call with a single address argument, e.g. balanceOf(address). */
export function encodeAddressCall(selector: string, address: string): string {
  const padded = stripHex(address).toLowerCase().padStart(64, '0');
  return `${selector}${padded}`;
}

/** Encodes a call with no arguments, e.g. decimals(). */
export function encodeCall(selector: string): string {
  return selector;
}

/** Decodes a uint (uint8, uint256, ...) into a bigint. */
export function decodeUint(hex: string): bigint | null {
  const body = stripHex(hex);
  if (!/^[0-9a-fA-F]+$/.test(body)) {
    return null;
  }
  try {
    return BigInt(`0x${body}`);
  } catch {
    return null;
  }
}

/** Decodes a uint8 return value. */
export function decodeUint8(hex: string): number | null {
  const value = decodeUint(hex);
  if (value === null) {
    return null;
  }
  return Number(value & 0xffn);
}

/**
 * Decodes an ABI encoded dynamic string (`string` return type). Returns null
 * when the payload is not a string (for example an empty revert).
 */
export function decodeString(hex: string): string | null {
  const body = stripHex(hex);
  if (body.length < 64) {
    return null;
  }
  const offset = Number.parseInt(body.slice(0, 64), 16);
  if (!Number.isFinite(offset) || offset % 32 !== 0 || offset + 64 > body.length) {
    return null;
  }
  const length = Number.parseInt(body.slice(offset, offset + 64), 16);
  if (!Number.isFinite(length) || length < 0 || length > 1024) {
    return null;
  }
  const start = offset + 64;
  const bytes = body.slice(start, start + length * 2);
  if (bytes.length !== length * 2) {
    return null;
  }
  try {
    const decoded = Buffer.from(bytes, 'hex').toString('utf8').replace(/\0+$/, '');
    return decoded.length > 0 ? decoded : null;
  } catch {
    return null;
  }
}

/** Converts a wei amount into a floating point native amount. */
export function weiToNative(wei: bigint, decimals = 18): number {
  const scale = 10n ** BigInt(decimals);
  const whole = wei / scale;
  const fraction = wei % scale;
  return Number(whole) + Number(fraction) / Number(scale);
}

/** Converts a wei amount into a decimal string, without losing precision. */
export function weiToDecimalString(wei: bigint, decimals = 18): string {
  const scale = 10n ** BigInt(decimals);
  const whole = wei / scale;
  const fraction = (wei % scale).toString().padStart(decimals, '0').replace(/0+$/, '');
  return fraction.length > 0 ? `${whole}.${fraction}` : whole.toString();
}

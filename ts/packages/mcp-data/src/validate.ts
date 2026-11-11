/**
 * Input validation.
 *
 * The agent supplies identifiers that end up inside upstream URLs, so every
 * one of them is checked against a strict allowlist before a request is built.
 * There is no generic "fetch this URL" tool: URLs are always assembled from
 * validated parts and a fixed table of endpoints.
 */

export const COIN_ID_PATTERN = /^[a-z0-9-]{1,64}$/;
export const SYMBOL_PATTERN = /^[A-Za-z0-9]{1,20}$/;
export const EVM_ADDRESS_PATTERN = /^0x[a-fA-F0-9]{40}$/;
export const SOLANA_ADDRESS_PATTERN = /^[1-9A-HJ-NP-Za-km-z]{32,44}$/;
export const DEX_QUERY_PATTERN = /^[A-Za-z0-9 ._/-]{1,64}$/;
export const CURRENCY_PATTERN = /^[a-z]{3}$/;

export const CHAIN_IDS = [
  'solana',
  'ethereum',
  'bsc',
  'polygon',
  'arbitrum',
  'optimism',
  'base',
  'avalanche',
] as const;

export type ChainId = (typeof CHAIN_IDS)[number];

export const EVM_CHAIN_IDS = [
  'ethereum',
  'bsc',
  'polygon',
  'arbitrum',
  'optimism',
  'base',
  'avalanche',
] as const;

export type EvmChainId = (typeof EVM_CHAIN_IDS)[number];

export const CURRENCIES = [
  'usd',
  'eur',
  'gbp',
  'jpy',
  'aud',
  'cad',
  'chf',
  'cny',
  'inr',
  'krw',
] as const;

export type Currency = (typeof CURRENCIES)[number];

/** Thrown when a caller passes something we refuse to put in a URL. */
export class ValidationError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'ValidationError';
  }
}

export function isEvmAddress(value: string): boolean {
  return EVM_ADDRESS_PATTERN.test(value);
}

export function isSolanaAddress(value: string): boolean {
  return SOLANA_ADDRESS_PATTERN.test(value);
}

export function isCoinId(value: string): boolean {
  return COIN_ID_PATTERN.test(value);
}

/**
 * Chain-aware address check. Solana uses base58, the EVM chains use a 0x
 * prefixed 20 byte hex address.
 */
export function assertAddress(chain: ChainId, value: string, label = 'address'): string {
  const valid = chain === 'solana' ? isSolanaAddress(value) : isEvmAddress(value);
  if (!valid) {
    const expected = chain === 'solana' ? 'a base58 Solana address' : 'a 0x-prefixed EVM address';
    throw new ValidationError(`${label} is not ${expected}`);
  }
  return value;
}

/** CoinGecko coin ids are lowercase slugs; nothing else is allowed. */
export function assertCoinId(value: string): string {
  if (!isCoinId(value)) {
    throw new ValidationError('coin id must match ^[a-z0-9-]{1,64}$');
  }
  return value;
}

/** Symbols are short alphanumeric tickers. */
export function assertSymbol(value: string): string {
  if (!SYMBOL_PATTERN.test(value)) {
    throw new ValidationError('symbol must match ^[A-Za-z0-9]{1,20}$');
  }
  return value;
}

/** Free-text DEX search terms: no slashes beyond one, no URLs, no spaces at the edges. */
export function assertDexQuery(value: string): string {
  if (!DEX_QUERY_PATTERN.test(value) || value.trim() !== value) {
    throw new ValidationError('query must match ^[A-Za-z0-9 ._/-]{1,64}$');
  }
  return value;
}

export function assertCurrency(value: string): string {
  if (!CURRENCY_PATTERN.test(value)) {
    throw new ValidationError('currency must be a 3 letter code');
  }
  return value;
}

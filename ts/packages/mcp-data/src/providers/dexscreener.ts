/**
 * DexScreener: DEX pairs, prices and liquidity.
 */

import type { ToolContext } from '../context.js';
import { DEXSCREENER_HOST } from '../config.js';
import { HttpError } from '../http.js';
import { sanitizeText, sanitizeUrl } from '../sanitize.js';
import type { ChainId } from '../validate.js';

export interface DexPair {
  chain: string;
  dex: string;
  pair_address: string;
  url: string | null;
  base: { symbol: string; name: string; address: string };
  quote: { symbol: string; name: string; address: string };
  price_usd: number | null;
  liquidity_usd: number | null;
  volume_24h: number | null;
  price_change_1h_pct: number | null;
  price_change_6h_pct: number | null;
  price_change_24h_pct: number | null;
  txns_24h: { buys: number | null; sells: number | null };
  market_cap: number | null;
  fdv: number | null;
  pair_created_at: number | null;
}

function asRecord(value: unknown): Record<string, unknown> {
  return value && typeof value === 'object' ? (value as Record<string, unknown>) : {};
}

function num(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function str(value: unknown, max = 200): string {
  if (typeof value !== 'string') {
    return '';
  }
  return sanitizeText(value, max);
}

export function compactPair(raw: unknown): DexPair | null {
  const pair = asRecord(raw);
  const pairAddress = str(pair.pairAddress, 80);
  if (!pairAddress) {
    return null;
  }
  const base = asRecord(pair.baseToken);
  const quote = asRecord(pair.quoteToken);
  const liquidity = asRecord(pair.liquidity);
  const volume = asRecord(pair.volume);
  const priceChange = asRecord(pair.priceChange);
  const txns = asRecord(asRecord(pair.txns).h24);

  return {
    chain: str(pair.chainId, 32),
    dex: str(pair.dexId, 40),
    pair_address: pairAddress,
    url: sanitizeUrl(pair.url),
    base: {
      symbol: str(base.symbol, 20),
      name: str(base.name, 60),
      address: str(base.address, 80),
    },
    quote: {
      symbol: str(quote.symbol, 20),
      name: str(quote.name, 60),
      address: str(quote.address, 80),
    },
    price_usd: num(pair.priceUsd),
    liquidity_usd: num(liquidity.usd),
    volume_24h: num(volume.h24),
    price_change_1h_pct: num(priceChange.h1),
    price_change_6h_pct: num(priceChange.h6),
    price_change_24h_pct: num(priceChange.h24),
    txns_24h: { buys: num(txns.buys), sells: num(txns.sells) },
    market_cap: num(pair.marketCap),
    fdv: num(pair.fdv),
    pair_created_at: num(pair.pairCreatedAt),
  };
}

function pairsOf(raw: unknown): unknown[] {
  const record = asRecord(raw);
  const pairs = record.pairs;
  return Array.isArray(pairs) ? pairs : [];
}

export async function searchPairs(
  ctx: ToolContext,
  query: string,
  limit: number,
): Promise<DexPair[]> {
  const url = `${ctx.config.dexscreenerBaseUrl}/latest/dex/search?q=${encodeURIComponent(query)}`;
  const raw = await ctx.http.getJson<unknown>(url, {
    host: DEXSCREENER_HOST,
    ttlMs: ctx.config.cacheTtlMs.dex,
  });
  return pairsOf(raw)
    .map(compactPair)
    .filter((pair): pair is DexPair => pair !== null)
    .sort((a, b) => (b.liquidity_usd ?? 0) - (a.liquidity_usd ?? 0))
    .slice(0, limit);
}

export async function fetchPair(
  ctx: ToolContext,
  chain: ChainId,
  pairAddress: string,
): Promise<DexPair | null> {
  const descriptor = ctx.config.chains[chain];
  const url = `${ctx.config.dexscreenerBaseUrl}/latest/dex/pairs/${descriptor.dexChainId}/${encodeURIComponent(pairAddress)}`;
  const raw = await ctx.http.getJson<unknown>(url, {
    host: DEXSCREENER_HOST,
    ttlMs: ctx.config.cacheTtlMs.dex,
  });
  const pairs = pairsOf(raw);
  if (pairs.length === 0) {
    throw new HttpError('no DEX pair found for that address');
  }
  return compactPair(pairs[0]);
}

export async function fetchTokenPairs(
  ctx: ToolContext,
  chain: ChainId,
  tokenAddress: string,
): Promise<DexPair[]> {
  const descriptor = ctx.config.chains[chain];
  const url = `${ctx.config.dexscreenerBaseUrl}/tokens/v1/${descriptor.dexChainId}/${encodeURIComponent(tokenAddress)}`;
  const raw = await ctx.http.getJson<unknown>(url, {
    host: DEXSCREENER_HOST,
    ttlMs: ctx.config.cacheTtlMs.dex,
  });
  const rows = Array.isArray(raw) ? raw : [];
  return rows
    .map(compactPair)
    .filter((pair): pair is DexPair => pair !== null)
    .sort((a, b) => (b.liquidity_usd ?? 0) - (a.liquidity_usd ?? 0));
}

/**
 * CoinGecko: market caps, prices and coin metadata.
 *
 * Only the fields the agent actually reasons about are kept; the raw upstream
 * payload is never handed on.
 */

import type { ToolContext } from '../context.js';
import { COINGECKO_HOST } from '../config.js';
import { sanitizeText, sanitizeUrl } from '../sanitize.js';
import { HttpError } from '../http.js';

export interface CoinSummary {
  rank: number | null;
  id: string;
  symbol: string;
  name: string;
  price: number | null;
  market_cap: number | null;
  volume_24h: number | null;
  high_24h: number | null;
  low_24h: number | null;
  change_24h_pct: number | null;
  last_updated: string | null;
}

export interface CoinDetails {
  id: string;
  symbol: string;
  name: string;
  categories: string[];
  description: string | null;
  homepage: string | null;
  rank: number | null;
  price: number | null;
  market_cap: number | null;
  volume_24h: number | null;
  high_24h: number | null;
  low_24h: number | null;
  change_24h_pct: number | null;
  circulating_supply: number | null;
  total_supply: number | null;
  max_supply: number | null;
  ath: number | null;
  atl: number | null;
  genesis_date: string | null;
  last_updated: string | null;
}

function asRecord(value: unknown): Record<string, unknown> {
  return value && typeof value === 'object' ? (value as Record<string, unknown>) : {};
}

function num(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function str(value: unknown, max = 200): string | null {
  if (typeof value !== 'string' || value.trim() === '') {
    return null;
  }
  const cleaned = sanitizeText(value, max);
  return cleaned.length > 0 ? cleaned : null;
}

function pick(value: unknown, key: string): unknown {
  return asRecord(value)[key];
}

export function compactCoin(raw: unknown): CoinSummary | null {
  const coin = asRecord(raw);
  const id = str(coin.id, 64);
  if (!id) {
    return null;
  }
  return {
    rank: num(coin.market_cap_rank),
    id,
    symbol: str(coin.symbol, 20) ?? '',
    name: str(coin.name, 80) ?? '',
    price: num(coin.current_price),
    market_cap: num(coin.market_cap),
    volume_24h: num(coin.total_volume),
    high_24h: num(coin.high_24h),
    low_24h: num(coin.low_24h),
    change_24h_pct: num(coin.price_change_percentage_24h),
    last_updated: str(coin.last_updated, 40),
  };
}

export function compactDetails(raw: unknown): CoinDetails | null {
  const coin = asRecord(raw);
  const id = str(coin.id, 64);
  if (!id) {
    return null;
  }
  const market = asRecord(coin.market_data);
  const description = asRecord(coin.description);
  const links = asRecord(coin.links);
  const homepages = links.homepage;
  const homepage = Array.isArray(homepages) ? sanitizeUrl(homepages[0]) : null;
  const categories = Array.isArray(coin.categories)
    ? coin.categories
        .slice(0, 8)
        .map((item) => str(item, 40))
        .filter((item): item is string => item !== null)
    : [];

  return {
    id,
    symbol: str(coin.symbol, 20) ?? '',
    name: str(coin.name, 80) ?? '',
    categories,
    description: str(description.en, 200),
    homepage,
    rank: num(coin.market_cap_rank),
    price: num(pick(market.current_price, 'usd')),
    market_cap: num(pick(market.market_cap, 'usd')),
    volume_24h: num(pick(market.total_volume, 'usd')),
    high_24h: num(pick(market.high_24h, 'usd')),
    low_24h: num(pick(market.low_24h, 'usd')),
    change_24h_pct: num(market.price_change_percentage_24h),
    circulating_supply: num(market.circulating_supply),
    total_supply: num(market.total_supply),
    max_supply: num(market.max_supply),
    ath: num(pick(market.ath, 'usd')),
    atl: num(pick(market.atl, 'usd')),
    genesis_date: str(coin.genesis_date, 40),
    last_updated: str(coin.last_updated, 40),
  };
}

export async function listTopCoins(
  ctx: ToolContext,
  options: { limit: number; vsCurrency: string; page: number },
): Promise<CoinSummary[]> {
  const url =
    `${ctx.config.coingeckoBaseUrl}/coins/markets` +
    `?vs_currency=${encodeURIComponent(options.vsCurrency)}` +
    '&order=market_cap_desc' +
    `&per_page=${options.limit}` +
    `&page=${options.page}` +
    '&sparkline=false' +
    '&price_change_percentage=24h';

  const rows = await ctx.http.getJson<unknown>(url, {
    host: COINGECKO_HOST,
    ttlMs: ctx.config.cacheTtlMs.markets,
  });
  if (!Array.isArray(rows)) {
    throw new HttpError('coingecko returned an unexpected market list');
  }
  return rows
    .map(compactCoin)
    .filter((coin): coin is CoinSummary => coin !== null);
}

export async function fetchSimplePrices(
  ctx: ToolContext,
  ids: string[],
  vsCurrency: string,
): Promise<Record<string, Record<string, number | null>>> {
  const url =
    `${ctx.config.coingeckoBaseUrl}/simple/price` +
    `?ids=${encodeURIComponent(ids.join(','))}` +
    `&vs_currencies=${encodeURIComponent(vsCurrency)}` +
    '&include_market_cap=true' +
    '&include_24hr_vol=true' +
    '&include_24hr_change=true';

  const raw = await ctx.http.getJson<unknown>(url, {
    host: COINGECKO_HOST,
    ttlMs: ctx.config.cacheTtlMs.markets,
  });
  const prices = asRecord(raw);
  const output: Record<string, Record<string, number | null>> = {};
  for (const id of ids) {
    const entry = asRecord(prices[id]);
    const value = num(pick(entry, vsCurrency));
    output[id] = {
      price: value,
      market_cap: num(pick(entry, `${vsCurrency}_market_cap`)),
      volume_24h: num(pick(entry, `${vsCurrency}_24h_vol`)),
      change_24h_pct: num(pick(entry, `${vsCurrency}_24h_change`)),
    };
  }
  return output;
}

/** Resolves tickers to CoinGecko ids using the markets endpoint. */
export async function resolveSymbols(
  ctx: ToolContext,
  symbols: string[],
  vsCurrency: string,
): Promise<Record<string, string>> {
  const url =
    `${ctx.config.coingeckoBaseUrl}/coins/markets` +
    `?vs_currency=${encodeURIComponent(vsCurrency)}` +
    `&symbols=${encodeURIComponent(symbols.join(','))}` +
    '&order=market_cap_desc' +
    `&per_page=${Math.min(Math.max(symbols.length, 1), 250)}` +
    '&page=1' +
    '&sparkline=false';

  const rows = await ctx.http.getJson<unknown>(url, {
    host: COINGECKO_HOST,
    ttlMs: ctx.config.cacheTtlMs.markets,
  });
  const mapping: Record<string, string> = {};
  if (!Array.isArray(rows)) {
    return mapping;
  }
  for (const row of rows) {
    const coin = asRecord(row);
    const symbol = str(coin.symbol, 20);
    const id = str(coin.id, 64);
    if (symbol && id && !mapping[symbol.toUpperCase()]) {
      mapping[symbol.toUpperCase()] = id;
    }
  }
  return mapping;
}

/** Maps ids to their tickers. */
export async function resolveIds(
  ctx: ToolContext,
  ids: string[],
  vsCurrency: string,
): Promise<Record<string, string>> {
  if (ids.length === 0) {
    return {};
  }
  const url =
    `${ctx.config.coingeckoBaseUrl}/coins/markets` +
    `?vs_currency=${encodeURIComponent(vsCurrency)}` +
    `&ids=${encodeURIComponent(ids.join(','))}` +
    `&order=market_cap_desc` +
    `&per_page=${Math.min(Math.max(ids.length, 1), 250)}` +
    '&page=1' +
    '&sparkline=false';

  const rows = await ctx.http.getJson<unknown>(url, {
    host: COINGECKO_HOST,
    ttlMs: ctx.config.cacheTtlMs.markets,
  });
  const mapping: Record<string, string> = {};
  if (!Array.isArray(rows)) {
    return mapping;
  }
  for (const row of rows) {
    const coin = asRecord(row);
    const id = str(coin.id, 64);
    const symbol = str(coin.symbol, 20);
    if (id && symbol) {
      mapping[id] = symbol;
    }
  }
  return mapping;
}

export async function fetchCoinDetails(
  ctx: ToolContext,
  id: string,
): Promise<CoinDetails | null> {
  const url =
    `${ctx.config.coingeckoBaseUrl}/coins/${encodeURIComponent(id)}` +
    '?localization=false' +
    '&tickers=false' +
    '&market_data=true' +
    '&community_data=false' +
    '&developer_data=false' +
    '&sparkline=false';

  const raw = await ctx.http.getJson<unknown>(url, {
    host: COINGECKO_HOST,
    ttlMs: ctx.config.cacheTtlMs.details,
  });
  return compactDetails(raw);
}

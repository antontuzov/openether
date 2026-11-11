/**
 * Free market data for the dashboard.
 *
 * CoinGecko's public endpoints need no API key and send
 * `access-control-allow-origin: *`, so the browser can call them directly —
 * which is what lets the static GitHub Pages build show live prices without
 * a backend.
 */

export interface Coin {
  id: string;
  symbol: string;
  name: string;
  price: number;
  marketCap: number;
  marketCapRank: number;
  totalVolume: number;
  change24h: number;
  high24h: number;
  low24h: number;
}

const COINGECKO_MARKETS =
  'https://api.coingecko.com/api/v3/coins/markets?vs_currency=usd&order=market_cap_desc&per_page=50&page=1&sparkline=false&price_change_percentage=24h';

/** How long a fetched list stays fresh, in milliseconds. */
export const COIN_CACHE_MS = 60_000;

interface CacheEntry {
  coins: Coin[];
  fetchedAt: number;
}

let cache: CacheEntry | null = null;
let inflight: Promise<Coin[]> | null = null;

/**
 * The top coins by market cap.
 *
 * A failed fetch falls back to the last good list so a rate-limited free
 * tier never blanks the screen.
 */
export async function fetchTopCoins(limit = 50, force = false): Promise<Coin[]> {
  const now = Date.now();
  if (!force && cache && now - cache.fetchedAt < COIN_CACHE_MS) {
    return cache.coins.slice(0, limit);
  }
  if (inflight) {
    return inflight.then((coins) => coins.slice(0, limit));
  }

  inflight = (async () => {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 12_000);
    try {
      const response = await fetch(COINGECKO_MARKETS, {
        signal: controller.signal,
        headers: { accept: 'application/json' },
      });
      if (!response.ok) {
        throw new Error(`CoinGecko responded ${response.status}`);
      }
      const payload: unknown = await response.json();
      if (!Array.isArray(payload)) {
        throw new Error('CoinGecko returned an unexpected payload');
      }
      const coins = payload.map(normaliseCoin).filter((coin): coin is Coin => coin !== null);
      if (coins.length === 0) {
        throw new Error('CoinGecko returned no usable coins');
      }
      cache = { coins, fetchedAt: Date.now() };
      return coins;
    } finally {
      clearTimeout(timer);
      inflight = null;
    }
  })();

  try {
    const coins = await inflight;
    return coins.slice(0, limit);
  } catch {
    return cache ? cache.coins.slice(0, limit) : [];
  }
}

function normaliseCoin(raw: unknown): Coin | null {
  if (!raw || typeof raw !== 'object') {
    return null;
  }
  const item = raw as Record<string, unknown>;
  const price = Number(item['current_price']);
  if (!Number.isFinite(price)) {
    return null;
  }
  return {
    id: String(item['id'] ?? ''),
    symbol: String(item['symbol'] ?? '').toUpperCase(),
    name: String(item['name'] ?? ''),
    price,
    marketCap: Number(item['market_cap'] ?? 0),
    marketCapRank: Number(item['market_cap_rank'] ?? 0),
    totalVolume: Number(item['total_volume'] ?? 0),
    change24h: Number(item['price_change_percentage_24h'] ?? 0),
    high24h: Number(item['high_24h'] ?? price),
    low24h: Number(item['low_24h'] ?? price),
  };
}

/** The trading symbol used by the engine for a coin, e.g. `SOL/USDT`. */
export function symbolForCoin(coin: Coin): string {
  return `${coin.symbol}/USDT`;
}

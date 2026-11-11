import { useMemo, useState } from 'react';
import type { Coin } from '../lib/coins';
import { symbolForCoin } from '../lib/coins';
import { fmtPrice, fmtSignedPct } from '../lib/format';
import { Panel } from './Panel';
import './admin.css';

interface CoinTableProps {
  coins: Coin[];
  /** Called with the trading symbol (`SOL/USDT`) and the current price. */
  onTrade: (symbol: string, price: number) => void;
  loading?: boolean;
  error?: string | null;
}

type SortKey = 'marketCap' | 'change24h';
type SortDir = 'asc' | 'desc';

const COMPACT_USD = new Intl.NumberFormat('en-US', {
  style: 'currency',
  currency: 'USD',
  notation: 'compact',
  maximumFractionDigits: 2,
});

/** `$1.49T`, `$842.3M`. */
function fmtCompactUsd(value: number): string {
  if (!Number.isFinite(value)) {
    return '—';
  }
  return COMPACT_USD.format(value);
}

/**
 * The market picker: the top coins by market cap, ready to trade.
 *
 * The parent owns the fetching — this component only renders what it is
 * handed, so the free CoinGecko endpoint can rate-limit without the table
 * ever blocking the page.
 */
export function CoinTable({ coins, onTrade, loading = false, error = null }: CoinTableProps) {
  const [query, setQuery] = useState('');
  const [sortKey, setSortKey] = useState<SortKey>('marketCap');
  const [sortDir, setSortDir] = useState<SortDir>('desc');

  const filtered = useMemo(() => {
    const needle = query.trim().toLowerCase();
    const matching = needle
      ? coins.filter(
          (coin) =>
            coin.name.toLowerCase().includes(needle) || coin.symbol.toLowerCase().includes(needle),
        )
      : coins;

    const direction = sortDir === 'asc' ? 1 : -1;
    return matching
      .slice()
      .sort((a, b) => {
        if (sortKey === 'change24h') {
          return (a.change24h - b.change24h) * direction;
        }
        return (a.marketCap - b.marketCap) * direction;
      });
  }, [coins, query, sortKey, sortDir]);

  const toggleSort = (key: SortKey): void => {
    if (key === sortKey) {
      setSortDir((current) => (current === 'asc' ? 'desc' : 'asc'));
      return;
    }
    setSortKey(key);
    setSortDir('desc');
  };

  const sortMark = (key: SortKey): string => {
    if (key !== sortKey) {
      return '↕';
    }
    return sortDir === 'asc' ? '↑' : '↓';
  };

  const ariaSort = (key: SortKey): 'ascending' | 'descending' | 'none' => {
    if (key !== sortKey) {
      return 'none';
    }
    return sortDir === 'asc' ? 'ascending' : 'descending';
  };

  return (
    <Panel
      title="Markets"
      hint={`${filtered.length} of ${coins.length} coins`}
      className="panel--coins"
      bodyClassName="panel__bd--flush"
    >
      <div className="ct-toolbar">
        <input
          className="input ct-filter"
          type="search"
          value={query}
          placeholder="filter by name or symbol"
          aria-label="Filter coins by name or symbol"
          onChange={(event) => setQuery(event.target.value)}
        />
        {loading ? (
          <span className="ct-status" role="status">
            loading…
          </span>
        ) : null}
      </div>

      {error ? (
        <p className="inline-msg inline-msg--err ct-error" role="status">
          CoinGecko&apos;s free endpoint is rate-limiting anonymous requests, so prices may be stale
          or missing — retry in a minute. ({error})
        </p>
      ) : null}

      {loading && coins.length === 0 ? (
        <div className="empty empty--inline">
          <p className="empty__title">Loading market data…</p>
          <p className="empty__hint">Fetching the top coins by market cap from the free API.</p>
        </div>
      ) : coins.length === 0 ? (
        <div className="empty empty--inline">
          <p className="empty__title">No market data</p>
          <p className="empty__hint">
            The free price feed did not answer. Check the connection and retry — the table fills in as
            soon as the API responds.
          </p>
        </div>
      ) : (
        <div className="tbl-wrap ct-scroll">
          <table className="tbl tbl--coins">
            <colgroup>
              <col style={{ width: '7%' }} />
              <col style={{ width: '22%' }} />
              <col style={{ width: '16%' }} />
              <col style={{ width: '13%' }} />
              <col style={{ width: '16%' }} />
              <col style={{ width: '16%' }} />
              <col style={{ width: '10%' }} />
            </colgroup>
            <thead>
              <tr>
                <th scope="col">rank</th>
                <th scope="col">name</th>
                <th scope="col" className="ta-r">
                  price
                </th>
                <th scope="col" className="ta-r" aria-sort={ariaSort('change24h')}>
                  <button
                    type="button"
                    className="adm-th-btn"
                    onClick={() => toggleSort('change24h')}
                    aria-label={`Sort by 24 hour change, currently ${sortDir === 'asc' ? 'ascending' : 'descending'}`}
                  >
                    24h % <span className="adm-th-btn__dir">{sortMark('change24h')}</span>
                  </button>
                </th>
                <th scope="col" className="ta-r" aria-sort={ariaSort('marketCap')}>
                  <button
                    type="button"
                    className="adm-th-btn"
                    onClick={() => toggleSort('marketCap')}
                    aria-label={`Sort by market cap, currently ${sortDir === 'asc' ? 'ascending' : 'descending'}`}
                  >
                    market cap <span className="adm-th-btn__dir">{sortMark('marketCap')}</span>
                  </button>
                </th>
                <th scope="col" className="ta-r">
                  volume
                </th>
                <th scope="col" className="ta-r">
                  trade
                </th>
              </tr>
            </thead>
            <tbody>
              {filtered.map((coin, index) => (
                <tr key={coin.id || `${coin.symbol}-${index}`}>
                  <td className="num dim">{coin.marketCapRank || index + 1}</td>
                  <td>
                    <span className="ct-name">
                      <span className="ct-name__label">{coin.name}</span>
                      <span className="ct-name__sym">{coin.symbol}</span>
                    </span>
                  </td>
                  <td className="ta-r num">${fmtPrice(coin.price)}</td>
                  <td className={`ta-r num ${coin.change24h > 0 ? 'pos' : coin.change24h < 0 ? 'neg' : ''}`}>
                    {fmtSignedPct(coin.change24h)}
                  </td>
                  <td className="ta-r num dim">{fmtCompactUsd(coin.marketCap)}</td>
                  <td className="ta-r num dim">{fmtCompactUsd(coin.totalVolume)}</td>
                  <td className="ta-r">
                    <button
                      type="button"
                      className="btn btn--tiny"
                      onClick={() => onTrade(symbolForCoin(coin), coin.price)}
                      aria-label={`Trade ${coin.name} (${coin.symbol})`}
                    >
                      trade
                    </button>
                  </td>
                </tr>
              ))}
              {filtered.length === 0 ? (
                <tr>
                  <td colSpan={7} className="dim ct-none">
                    No coin matches “{query}”.
                  </td>
                </tr>
              ) : null}
            </tbody>
          </table>
        </div>
      )}
    </Panel>
  );
}

export default CoinTable;

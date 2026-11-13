import type { ChainHoldings, WalletHoldings } from '../api/types';
import { fmtAgo, fmtPrice, fmtQty, fmtUsdPrefixed, shortAddress } from '../lib/format';
import { Panel } from './Panel';

interface WalletPanelProps {
  wallet: WalletHoldings | null;
}

interface HoldingRow {
  key: string;
  symbol: string;
  asset: string;
  balance: number;
  price: number | null;
  value: number;
}

/** The gas coin first, then every token the feed could price. */
function rowsOf(chain: ChainHoldings): HoldingRow[] {
  return [
    {
      key: `${chain.chain}-native`,
      symbol: chain.native_symbol,
      asset: chain.address,
      balance: chain.native_balance,
      price: chain.native_price_usd,
      value: chain.native_value_usd,
    },
    ...chain.tokens.map((token) => ({
      key: `${chain.chain}-${token.asset}`,
      symbol: token.symbol,
      asset: token.asset,
      balance: token.balance,
      price: token.priced ? token.price_usd : null,
      value: token.value_usd,
    })),
  ];
}

function ChainBlock({ chain }: { chain: ChainHoldings }) {
  return (
    <div className="wallet-chain">
      <div className="wallet-chain__hd">
        <span className="wallet-chain__name">{chain.chain}</span>
        <span className="wallet-chain__addr num" title={chain.address}>
          {shortAddress(chain.address)}
        </span>
        <span className="wallet-chain__value num">{fmtUsdPrefixed(chain.value_usd)}</span>
      </div>

      <div className="tbl-wrap">
        <table className="tbl tbl--wallet">
          <colgroup>
            <col style={{ width: '28%' }} />
            <col style={{ width: '26%' }} />
            <col style={{ width: '23%' }} />
            <col style={{ width: '23%' }} />
          </colgroup>
          <thead>
            <tr>
              <th scope="col">asset</th>
              <th scope="col" className="ta-r">
                balance
              </th>
              <th scope="col" className="ta-r">
                price
              </th>
              <th scope="col" className="ta-r">
                value
              </th>
            </tr>
          </thead>
          <tbody>
            {rowsOf(chain).map((row) => (
              <tr key={row.key}>
                <td className="sym" title={row.asset}>
                  {row.symbol}
                </td>
                <td className="ta-r num">{fmtQty(row.balance)}</td>
                <td className="ta-r num dim">
                  {row.price === null ? 'unpriced' : `$${fmtPrice(row.price)}`}
                </td>
                <td className="ta-r num">
                  {row.price === null ? '—' : fmtUsdPrefixed(row.value)}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {chain.tokens_skipped > 0 ? (
        <p className="wallet-chain__note">
          {chain.tokens_skipped} more tokens left unpriced — the read is bounded so the free feed&apos;s
          rate limit survives a busy address.
        </p>
      ) : null}
    </div>
  );
}

/**
 * The on-chain wallet, valued but not traded.
 *
 * The engine reads these balances from the chain and prices them from the DEX
 * feed without ever unlocking the keystore. There is no broadcast path, so the
 * risk layer keeps measuring equity on the trading account alone — this panel
 * says so rather than letting the two numbers look like one pot of money.
 */
export function WalletPanel({ wallet }: WalletPanelProps) {
  const hint = !wallet
    ? 'awaiting first read'
    : wallet.configured
      ? `${wallet.chains.length} chain${wallet.chains.length === 1 ? '' : 's'} · read only`
      : 'no keystore';

  return (
    <Panel
      title="On-chain wallet"
      hint={hint}
      bodyClassName={wallet?.configured ? 'panel__bd--flush' : undefined}
      actions={
        <span className="panel__badge num">
          {wallet?.configured ? fmtUsdPrefixed(wallet.value_usd) : 'not spendable'}
        </span>
      }
    >
      {!wallet ? (
        <div className="empty">
          <p className="empty__title">Awaiting the first wallet read</p>
          <p className="empty__hint">
            The engine values the keystore&apos;s addresses every 30 seconds, straight from the chain.
          </p>
        </div>
      ) : !wallet.configured ? (
        <div className="empty">
          <p className="empty__title">No keystore configured</p>
          <p className="empty__hint">
            Create one with <code>openether-wallet create-hd</code> and point{' '}
            <code>OPENETHER_WALLET_KEYSTORE</code> at it. Reading balances never needs the passphrase —
            the engine only uses public addresses.
          </p>
        </div>
      ) : (
        <>
          <p className="wallet-note">
            Valued {fmtAgo(wallet.fetched_at_ms)} from the chain and the DEX feed. These balances are{' '}
            <strong>not trading equity</strong>: the engine cannot spend them, so the risk layer ignores
            them.
          </p>
          {wallet.chains.length === 0 ? (
            <div className="empty empty--inline">
              <p className="empty__title">No chain answered</p>
              <p className="empty__hint">
                The keystore is loaded but no RPC read succeeded yet. The next refresh retries.
              </p>
            </div>
          ) : (
            wallet.chains.map((chain) => <ChainBlock key={`${chain.chain}-${chain.address}`} chain={chain} />)
          )}
        </>
      )}
    </Panel>
  );
}

export default WalletPanel;
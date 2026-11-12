import { useEffect, useMemo, useState } from 'react';
import { DEFAULT_LIMITS, type RuleLimits } from '../lib/riskRules';

export interface TradeTicketProps {
  /** Symbols the visitor may trade, with the price to quote against. */
  market: Array<{ symbol: string; price: number; name?: string }>;
  limits?: RuleLimits;
  /** Controlled symbol. When omitted the ticket keeps its own selection. */
  symbol?: string;
  onSymbolChange?: (symbol: string) => void;
  /** Called when the visitor submits a valid order. */
  onSubmit: (order: { symbol: string; side: 'buy' | 'sell'; qty: number; rationale: string }) => void;
  /** Outcome of the last submission, shown inline. */
  outcome: { approved: boolean; reason: string; detail: string } | null;
  onDismissOutcome?: () => void;
}

const QUICK_QTY = [0.1, 0.5, 1, 5];

/**
 * The paper-trading ticket.
 *
 * It only ever proposes an order: the risk check and the fill happen in the
 * engine (or, on the static site, in the browser simulation). The visitor
 * cannot bypass either one.
 */
export function TradeTicket({
  market,
  limits = DEFAULT_LIMITS,
  symbol: controlledSymbol,
  onSymbolChange,
  onSubmit,
  outcome,
  onDismissOutcome,
}: TradeTicketProps) {
  const [localSymbol, setLocalSymbol] = useState<string>(market[0]?.symbol ?? '');
  const [side, setSide] = useState<'buy' | 'sell'>('buy');
  const [qty, setQty] = useState('1');
  const [rationale, setRationale] = useState('');

  const symbol = controlledSymbol ?? localSymbol;

  const setSymbol = (next: string): void => {
    if (onSymbolChange) {
      onSymbolChange(next);
      return;
    }
    setLocalSymbol(next);
  };

  const selected = useMemo(
    () => market.find((entry) => entry.symbol === symbol) ?? market[0],
    [market, symbol],
  );

  // A symbol can disappear when the market refreshes; fall back to the first.
  useEffect(() => {
    if (!market.some((entry) => entry.symbol === symbol) && market[0]) {
      setSymbol(market[0].symbol);
    }
  }, [market, symbol]);

  const qtyNumber = Number(qty);
  const notional = selected && Number.isFinite(qtyNumber) ? qtyNumber * selected.price : 0;

  const submit = (): void => {
    if (!selected || !(qtyNumber > 0) || !Number.isFinite(qtyNumber)) {
      return;
    }
    onSubmit({
      symbol: selected.symbol,
      side,
      qty: qtyNumber,
      rationale: rationale.trim() || 'manual trade from the dashboard',
    });
  };

  return (
    <section className="ticket" aria-label="paper trading ticket">
      <header className="ticket__head">
        <h2 className="panel__title">trade</h2>
        <span className="ticket__badge" title="Simulated fills. No real money, no exchange credentials.">
          simulation
        </span>
      </header>

      <div className="ticket__grid">
        <label className="ticket__field ticket__field--wide">
          <span className="ticket__label">market</span>
          <select
            className="input"
            value={symbol}
            onChange={(event) => setSymbol(event.target.value)}
            aria-label="Market"
          >
            {market.map((entry) => (
              <option key={entry.symbol} value={entry.symbol}>
                {entry.name ? `${entry.name} · ${entry.symbol}` : entry.symbol}
              </option>
            ))}
          </select>
        </label>

        <label className="ticket__field">
          <span className="ticket__label">side</span>
          <div className="ticket__sides" role="group" aria-label="Side">
            <button
              type="button"
              className={`btn ticket__side ${side === 'buy' ? 'btn--primary is-active' : ''}`}
              aria-pressed={side === 'buy'}
              onClick={() => setSide('buy')}
            >
              buy
            </button>
            <button
              type="button"
              className={`btn ticket__side ${side === 'sell' ? 'btn--danger is-active' : ''}`}
              aria-pressed={side === 'sell'}
              onClick={() => setSide('sell')}
            >
              sell
            </button>
          </div>
        </label>

        <label className="ticket__field">
          <span className="ticket__label">size</span>
          <input
            className="input num"
            type="number"
            min="0"
            step="0.0001"
            value={qty}
            onChange={(event) => setQty(event.target.value)}
            aria-label="Order size"
          />
        </label>
      </div>

      <div className="ticket__quick">
        {QUICK_QTY.map((amount) => (
          <button
            key={amount}
            type="button"
            className="chip"
            onClick={() => setQty(String(amount))}
            aria-label={`Set size to ${amount}`}
          >
            {amount}
          </button>
        ))}
      </div>

      <label className="ticket__field ticket__field--wide">
        <span className="ticket__label">rationale</span>
        <input
          className="input"
          type="text"
          maxLength={200}
          placeholder="why this trade (optional)"
          value={rationale}
          onChange={(event) => setRationale(event.target.value)}
          aria-label="Trade rationale"
        />
      </label>

      <div className="ticket__summary">
        <span className="ticket__summary-k">price</span>
        <span className="num">${selected ? selected.price.toFixed(2) : '—'}</span>
        <span className="ticket__summary-k">notional</span>
        <span className="num">${notional.toFixed(2)}</span>
        <span className="ticket__summary-k">cap</span>
        <span className="num">${limits.max_order_notional_usd.toLocaleString()}</span>
      </div>

      <button type="button" className="btn btn--primary ticket__submit" onClick={submit}>
        {side === 'buy' ? 'buy' : 'sell'} {selected ? selected.symbol : ''}
      </button>

      {outcome ? (
        <div className={`inline-msg ${outcome.approved ? 'inline-msg--ok' : 'inline-msg--err'}`} role="status">
          <span className="inline-msg__text">{outcome.detail}</span>
          {onDismissOutcome ? (
            <button type="button" className="inline-msg__close" onClick={onDismissOutcome} aria-label="Dismiss">
              ×
            </button>
          ) : null}
        </div>
      ) : null}
    </section>
  );
}

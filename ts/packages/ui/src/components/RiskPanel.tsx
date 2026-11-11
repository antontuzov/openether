import type { Position, RiskState, TradeRecord } from '../api/types';
import { fmtPct, fmtSignedUsd, fmtUsd, titleise } from '../lib/format';
import { Panel } from './Panel';

interface RiskPanelProps {
  risk: RiskState | null;
  positions: Position[];
  trades: TradeRecord[];
}

interface MeterProps {
  label: string;
  used: number;
  limit: number;
  format: (value: number) => string;
  detail?: string;
}

function Meter({ label, used, limit, format, detail }: MeterProps) {
  const safeLimit = limit > 0 ? limit : 0;
  const ratio = safeLimit > 0 ? used / safeLimit : 0;
  const clamped = Math.max(0, Math.min(1, ratio));
  const over = ratio > 1;
  const near = !over && ratio >= 0.8;

  return (
    <div className="meter">
      <div className="meter__row">
        <span className="meter__label">{label}</span>
        <span className="meter__value num">
          {format(used)} <span className="meter__limit">/ {format(safeLimit)}</span>
        </span>
      </div>
      <div className="meter__track">
        <div
          className={`meter__fill${over ? ' meter__fill--over' : near ? ' meter__fill--near' : ''}`}
          style={{ width: `${clamped * 100}%` }}
        />
      </div>
      {detail ? <div className="meter__detail">{detail}</div> : null}
    </div>
  );
}

export function RiskPanel({ risk, positions, trades }: RiskPanelProps) {
  const equity = risk?.equity_usd ?? 0;
  const limits = risk?.limits;
  const largestPosition = positions.reduce(
    (max, position) => Math.max(max, Math.abs(position.notional_usd)),
    0,
  );
  const largestOrder = trades.reduce((max, trade) => Math.max(max, Math.abs(trade.qty) * trade.price), 0);

  const grossLimit = limits ? (equity * limits.max_gross_exposure_pct) / 100 : 0;
  const positionLimit = limits ? (equity * limits.max_position_pct_of_equity) / 100 : 0;
  const dailyLossLimit = limits ? (equity * limits.max_daily_loss_pct) / 100 : 0;

  const dailyLossUsed = Math.max(0, -(risk?.daily_pnl_usd ?? 0));

  return (
    <Panel
      title="Risk"
      actions={
        <span className={`breaker breaker--${risk?.breaker_state ?? 'unknown'} breaker--sm`}>
          <span className="breaker__v">{risk ? titleise(risk.breaker_state) : 'unknown'}</span>
        </span>
      }
    >
      {!risk ? (
        <div className="empty empty--inline">
          <p className="empty__title">No risk state</p>
          <p className="empty__hint">Waiting for the engine to publish its risk snapshot.</p>
        </div>
      ) : (
        <>
          <div className={`risk__breaker risk__breaker--${risk.breaker_state}`}>
            <span className="risk__breaker-k">circuit breaker</span>
            <span className="risk__breaker-v">{titleise(risk.breaker_state)}</span>
            <span className="risk__breaker-reason">
              {risk.breaker_reason ? risk.breaker_reason : 'no reason recorded'}
            </span>
          </div>

          <div className="stats stats--risk">
            <div className="stat">
              <span className="stat__k">drawdown</span>
              <span className={`stat__v num ${risk.drawdown_pct > 0 ? 'neg' : ''}`}>
                {fmtPct(risk.drawdown_pct)}
              </span>
            </div>
            <div className="stat">
              <span className="stat__k">daily p&amp;l</span>
              <span
                className={`stat__v num ${
                  risk.daily_pnl_usd > 0 ? 'pos' : risk.daily_pnl_usd < 0 ? 'neg' : ''
                }`}
              >
                {fmtSignedUsd(risk.daily_pnl_usd)}
              </span>
            </div>
            <div className="stat">
              <span className="stat__k">open positions</span>
              <span className="stat__v num">
                {risk.open_positions}
                {limits ? <span className="stat__of"> / {limits.max_open_positions}</span> : null}
              </span>
            </div>
            <div className="stat">
              <span className="stat__k">gross exposure</span>
              <span className="stat__v num">${fmtUsd(risk.gross_exposure_usd)}</span>
            </div>
          </div>

          <div className="meters">
            <Meter
              label="gross exposure"
              used={risk.gross_exposure_usd}
              limit={grossLimit}
              format={(value) => `$${fmtUsd(value, 0)}`}
              detail={limits ? `${limits.max_gross_exposure_pct}% of equity` : undefined}
            />
            <Meter
              label="largest position"
              used={largestPosition}
              limit={positionLimit}
              format={(value) => `$${fmtUsd(value, 0)}`}
              detail={limits ? `${limits.max_position_pct_of_equity}% of equity` : undefined}
            />
            <Meter
              label="daily loss"
              used={dailyLossUsed}
              limit={dailyLossLimit}
              format={(value) => `$${fmtUsd(value, 0)}`}
              detail={limits ? `${limits.max_daily_loss_pct}% of equity` : undefined}
            />
            <Meter
              label="drawdown"
              used={risk.drawdown_pct}
              limit={limits?.max_drawdown_pct ?? 0}
              format={(value) => `${value.toFixed(2)}%`}
              detail={limits ? `max ${limits.max_drawdown_pct}%` : undefined}
            />
            <Meter
              label="open positions"
              used={risk.open_positions}
              limit={limits?.max_open_positions ?? 0}
              format={(value) => String(value)}
              detail={limits ? `max ${limits.max_open_positions}` : undefined}
            />
            <Meter
              label="largest recent order"
              used={largestOrder}
              limit={limits?.max_order_notional_usd ?? 0}
              format={(value) => `$${fmtUsd(value, 0)}`}
              detail={limits ? `max $${fmtUsd(limits.max_order_notional_usd, 0)}` : undefined}
            />
          </div>
        </>
      )}
    </Panel>
  );
}

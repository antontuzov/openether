/**
 * The canonical risk contract.
 *
 * `risk-rules.json` is generated from the Rust risk layer
 * (`cargo run -p openether-core --bin openether-rules`), so the limits, the
 * order the checks run in and the reason codes all come from the engine. The
 * browser simulator walks the same checks in the same order, so the dashboard
 * refuses exactly what the engine refuses, with the same words.
 *
 * When a core is running, the dashboard fetches the engine's own contract from
 * `GET /api/risk/rules` and uses that instead, so the simulator mirrors the
 * engine it is standing in for.
 */

import generated from './risk-rules.json';

export interface RuleLimits {
  max_position_pct_of_equity: number;
  max_gross_exposure_pct: number;
  max_daily_loss_pct: number;
  max_drawdown_pct: number;
  max_open_positions: number;
  max_order_notional_usd: number;
  min_order_notional_usd: number;
  breaker_cooldown_ms: number;
  hard_halt_on_oversized_order: boolean;
  max_correlated_exposure_pct: number;
  correlation_threshold: number;
  volatility_target_pct: number;
  volatility_window: number;
  min_size_scale: number;
}

export interface RuleAllowlists {
  symbols: string[];
  venues: string[];
  contracts: string[];
}

export interface RiskContract {
  version: number;
  limits: RuleLimits;
  allowlists: RuleAllowlists;
  /** The risk-layer checks, in the order they run. */
  check_order: string[];
  /**
   * Checks that judge the whole book rather than one order.
   *
   * They live outside `check_order` because they need history the browser
   * simulator does not keep: the correlation cap compares the return series of
   * every position. The engine runs them; the simulator is documented as
   * running only the per-order checks.
   */
  portfolio_checks: string[];
  /** The checks a venue applies on top of the risk layer. */
  venue_checks: string[];
  /** Every reason code an order can be refused with. */
  reasons: string[];
}

/** The contract generated from the Rust risk layer. */
export const RISK_CONTRACT: RiskContract = generated as RiskContract;

export const DEFAULT_LIMITS: RuleLimits = RISK_CONTRACT.limits;

/** The venue the browser simulator trades on. */
export const PAPER_VENUE = 'paper';

/** Every reason code the engine can refuse an order with. */
export const REASONS: ReadonlySet<string> = new Set(RISK_CONTRACT.reasons);

/**
 * The contract the browser simulator trades under.
 *
 * The engine's money limits, the order its checks run in and the words it
 * refuses with are copied verbatim: the simulator must refuse exactly what the
 * engine refuses.
 *
 * The allowlists are cleared. They describe the *engine's* venue — which
 * symbols its own connectors may trade on a live account — while the
 * simulator is a separate venue whose market universe is every coin the free
 * feed prices. Narrowing it to the engine's three live pairs would take the
 * top-50 market list away from the visitor for no safety gain: no exchange
 * account and no keys are involved.
 *
 * Portfolio checks stay in the contract, but the simulator does not run them:
 * the correlation cap compares each position's return series over time, which
 * the browser does not keep. The engine remains the authority for the book.
 */
export function toBrowserContract(engine: RiskContract): RiskContract {
  return {
    ...engine,
    allowlists: { symbols: [], venues: [], contracts: [] },
  };
}

/**
 * Whether the contract looks like a contract.
 *
 * The dashboard accepts a contract from the core over HTTP, so the shape has
 * to be checked before it is trusted: a malformed answer must not silently
 * widen the limits.
 */
export function isRiskContract(value: unknown): value is RiskContract {
  if (!value || typeof value !== 'object') return false;
  const candidate = value as Partial<RiskContract>;
  const limits = candidate.limits as Partial<RuleLimits> | undefined;
  if (!limits) return false;
  const numeric = [
    'max_position_pct_of_equity',
    'max_gross_exposure_pct',
    'max_daily_loss_pct',
    'max_drawdown_pct',
    'max_open_positions',
    'max_order_notional_usd',
    'min_order_notional_usd',
    'max_correlated_exposure_pct',
    'correlation_threshold',
    'volatility_target_pct',
    'volatility_window',
    'min_size_scale',
  ] as const;
  for (const key of numeric) {
    if (typeof limits[key] !== 'number' || !Number.isFinite(limits[key] as number)) return false;
  }
  if (limits.max_open_positions !== undefined && limits.max_open_positions < 0) return false;
  if (!Array.isArray(candidate.check_order)) return false;
  if (!Array.isArray(candidate.portfolio_checks)) return false;
  if (!Array.isArray(candidate.reasons)) return false;
  const allowlists = candidate.allowlists as Partial<RuleAllowlists> | undefined;
  if (!allowlists) return false;
  for (const key of ['symbols', 'venues', 'contracts'] as const) {
    if (!Array.isArray(allowlists[key])) return false;
  }
  return true;
}

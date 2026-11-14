//! Replaying a strategy over historical bars through the real risk layer.
//!
//! The point of this module is not a fancy fill model. It is that a backtest
//! asks the same `RiskEngine` the live engine asks: the per-order cap, the
//! position cap, gross exposure, the daily-loss limit, drawdown and the
//! breaker all judge the replay. If a strategy would have been stopped in
//! production, it is stopped here too, with the same reason code.
//!
//! Two rules keep the replay honest:
//!
//! * **No lookahead.** A signal is produced from a bar's close and filled at
//!   the *next* bar's open. The strategy never sees a price that a live run
//!   could not have seen at that instant.
//! * **Simulated time.** The risk layer is judged at the bar's timestamp, so a
//!   breaker cooldown expires after the simulated minutes pass, and a new
//!   trading day resets the daily-loss baseline as it would in production.

use crate::metrics::Metrics;
use crate::risk::{BreakerState, RiskConfig, RiskContext, RiskEngine, RiskLimits};
use crate::strategy::StrategySignal;
use crate::types::{OrderFill, OrderRequest, OrderType, Side, Venue};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// How many bars one request may carry. A replay is CPU work inside the API
/// process, so the input is bounded rather than trusted.
pub const MAX_BARS: usize = 5_000;

fn default_equity() -> f64 {
    10_000.0
}

fn default_fee_bps() -> f64 {
    5.0
}

fn default_slippage_bps() -> f64 {
    5.0
}

/// One OHLC bar. Volume is carried for completeness and not used by the fill
/// model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bar {
    pub ts_ms: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    #[serde(default)]
    pub volume: f64,
}

/// What to replay and how expensive a fill is.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestParams {
    pub symbol: String,
    #[serde(default = "default_equity")]
    pub initial_equity_usd: f64,
    /// Commission per side, in basis points of the fill notional.
    #[serde(default = "default_fee_bps")]
    pub fee_bps: f64,
    /// Price concession per side, in basis points. A buy fills above the open
    /// and a sell below it.
    #[serde(default = "default_slippage_bps")]
    pub slippage_bps: f64,
    /// Passed to the strategy as `params`, exactly as the live engine does.
    #[serde(default)]
    pub params: serde_json::Value,
}

/// One executed fill in the replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestTrade {
    pub ts_ms: i64,
    pub side: Side,
    pub qty: f64,
    pub price: f64,
    pub fee_usd: f64,
    /// The part of this fill that closed a position, after both fees. Zero for
    /// a pure entry.
    pub realized_pnl_usd: f64,
    pub rationale: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EquityPoint {
    pub ts_ms: i64,
    pub equity_usd: f64,
    pub drawdown_pct: f64,
}

/// The full result of a replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestReport {
    pub symbol: String,
    /// Filled in by the caller that owns the strategy; empty for a bare run.
    pub strategy: String,
    pub bars: usize,
    pub first_ts_ms: i64,
    pub last_ts_ms: i64,
    pub initial_equity_usd: f64,
    pub final_equity_usd: f64,
    pub total_return_pct: f64,
    pub max_drawdown_pct: f64,
    /// Per-bar Sharpe, annualised from the median bar spacing. Zero when the
    /// curve is flat or there are too few bars.
    pub sharpe: f64,
    pub win_rate: f64,
    /// Gross wins over gross losses. `None` when there were no losing closing
    /// fills, because the ratio would be infinite.
    pub profit_factor: Option<f64>,
    pub trades: Vec<BacktestTrade>,
    /// Fills that closed (part of) a position.
    pub closed_trades: usize,
    pub fees_paid_usd: f64,
    pub final_position_qty: f64,
    pub final_cash_usd: f64,
    /// Refusals by the risk layer, keyed by its reason code.
    pub rejected: BTreeMap<String, usize>,
    pub breaker_trips: usize,
    pub final_breaker: BreakerState,
    pub limits: RiskLimits,
    pub equity_curve: Vec<EquityPoint>,
}

#[derive(Debug, thiserror::Error)]
pub enum BacktestError {
    #[error("invalid bars: {0}")]
    InvalidBars(String),
    #[error("invalid backtest parameters: {0}")]
    InvalidParams(String),
    #[error("no strategy is loaded to replay")]
    NoStrategy,
    #[error("strategy failed on bar {bar} ({ts_ms}): {message}")]
    Strategy {
        bar: usize,
        ts_ms: i64,
        message: String,
    },
}

/// Replays `bars` through `source`, filling at the next open.
///
/// `source` is the strategy: it receives the same context a live tick gets and
/// returns proposals. Every proposal goes through `risk`, which owns the
/// decision.
pub fn run<F>(
    params: &BacktestParams,
    bars: &[Bar],
    risk_config: RiskConfig,
    mut source: F,
) -> Result<BacktestReport, BacktestError>
where
    F: FnMut(&serde_json::Value) -> Result<Vec<StrategySignal>, String>,
{
    validate(params, bars)?;

    let risk = RiskEngine::new(risk_config.clone(), std::sync::Arc::new(Metrics::new()));
    let mut sim = Simulation::new(params);

    let mut breaker_state = BreakerState::Closed;
    let mut breaker_trips = 0usize;
    let mut curve: Vec<EquityPoint> = Vec::with_capacity(bars.len());
    let mut peak = params.initial_equity_usd;

    // Signals approved at bar i-1, filled at bar i's open.
    let mut pending: Vec<(OrderRequest, String)> = Vec::new();

    for (i, bar) in bars.iter().enumerate() {
        // The close is known at this bar's close, so it may feed the sizing
        // model before the next open fills anything: no lookahead is gained.
        risk.observe_price(&params.symbol, bar.close, bar.ts_ms);
        let mut fills = Vec::new();
        for (request, rationale) in pending.drain(..) {
            fills.push(sim.fill(bar.ts_ms, bar.open, &request, &rationale));
        }

        let equity = sim.equity(bar.close);
        peak = peak.max(equity);
        // Mark equity before booking the fills so a trip caused by this bar is
        // seen here. `on_fill` re-marks internally, but its result is not
        // reported back.
        if risk.update_equity(equity, bar.ts_ms).is_some() {
            if breaker_state != BreakerState::Open {
                breaker_trips += 1;
            }
            breaker_state = BreakerState::Open;
        }
        for (k, fill) in fills.into_iter().enumerate() {
            let order_fill = OrderFill {
                venue: Venue::Paper,
                order_id: format!("bt-{i}-{k}"),
                symbol: params.symbol.clone(),
                side: fill.side,
                qty: fill.qty,
                price: fill.price,
                fee: fill.fee_usd,
                ts_ms: fill.ts_ms,
            };
            risk.on_fill(&order_fill, equity);
        }
        curve.push(EquityPoint {
            ts_ms: bar.ts_ms,
            equity_usd: equity,
            drawdown_pct: drawdown(peak, equity),
        });

        if i + 1 == bars.len() {
            break;
        }

        let ctx = context(params, bar, equity, &sim);
        let signals = source(&ctx).map_err(|message| BacktestError::Strategy {
            bar: i,
            ts_ms: bar.ts_ms,
            message,
        })?;

        // Approved signals are not filled yet, but they are committed orders:
        // the next signal in the same bar must be judged against them, or two
        // orders that each fit under a cap could pass together.
        let mut shadow_qty = sim.qty;
        for signal in signals {
            if signal.symbol != params.symbol {
                *sim.rejected.entry("symbol_mismatch".into()).or_insert(0) += 1;
                continue;
            }
            let (qty, scale) =
                risk.scale_for_volatility(signal.side, signal.qty, shadow_qty, &signal.symbol);
            if qty <= 0.0 {
                continue;
            }
            let rationale = if scale < 1.0 {
                format!("backtest: {} (volatility scale {scale:.2})", signal.reason)
            } else {
                format!("backtest: {}", signal.reason)
            };
            let request = OrderRequest {
                venue: Venue::Paper,
                symbol: signal.symbol.clone(),
                side: signal.side,
                qty,
                order_type: OrderType::Market,
                limit_price: None,
                trigger_price: None,
                oco_group: None,
                reduce_only: false,
                client_id: format!("bt-{i}-{}", sim.proposals),
                rationale: rationale.clone(),
                reduce_only_guard: false,
            };
            sim.proposals += 1;
            let risk_ctx = RiskContext {
                equity_usd: sim.equity(bar.close),
                gross_exposure_usd: shadow_qty.abs() * bar.close,
                open_positions: usize::from(shadow_qty.abs() > 1e-9),
                current_position_qty: shadow_qty,
                price: bar.close,
                contracts: Vec::new(),
            };
            let verdict = risk.evaluate_at(&request, &risk_ctx, bar.ts_ms);
            if verdict.breaker_state == BreakerState::Open && breaker_state != BreakerState::Open {
                breaker_trips += 1;
            }
            breaker_state = verdict.breaker_state;
            if verdict.approved {
                shadow_qty += signal.side.sign() * qty;
                pending.push((request, rationale));
            } else {
                *sim.rejected.entry(verdict.reason.clone()).or_insert(0) += 1;
            }
        }
    }

    let last = bars.last().expect("validated non-empty");
    let final_equity = sim.equity(last.close);
    let snapshot = risk.snapshot(final_equity, last.ts_ms);

    Ok(BacktestReport {
        symbol: params.symbol.clone(),
        strategy: String::new(),
        bars: bars.len(),
        first_ts_ms: bars[0].ts_ms,
        last_ts_ms: last.ts_ms,
        initial_equity_usd: params.initial_equity_usd,
        final_equity_usd: final_equity,
        total_return_pct: (final_equity / params.initial_equity_usd - 1.0) * 100.0,
        max_drawdown_pct: curve
            .iter()
            .map(|point| point.drawdown_pct)
            .fold(0.0, f64::max),
        sharpe: sharpe(&curve, bars),
        win_rate: sim.win_rate(),
        profit_factor: sim.profit_factor(),
        closed_trades: sim.closed.len(),
        fees_paid_usd: sim.fees_paid,
        final_position_qty: sim.qty,
        final_cash_usd: sim.cash,
        trades: sim.trades,
        rejected: sim.rejected,
        breaker_trips,
        final_breaker: snapshot.breaker_state,
        limits: RiskLimits::from(&risk_config),
        equity_curve: curve,
    })
}

fn validate(params: &BacktestParams, bars: &[Bar]) -> Result<(), BacktestError> {
    if params.symbol.trim().is_empty() {
        return Err(BacktestError::InvalidParams(
            "the symbol must not be empty".into(),
        ));
    }
    if !params.initial_equity_usd.is_finite() || params.initial_equity_usd <= 0.0 {
        return Err(BacktestError::InvalidParams(
            "initial equity must be a positive finite number".into(),
        ));
    }
    if !params.fee_bps.is_finite() || !(0.0..=1_000.0).contains(&params.fee_bps) {
        return Err(BacktestError::InvalidParams(
            "fee_bps must be between 0 and 1000".into(),
        ));
    }
    if !params.slippage_bps.is_finite() || !(0.0..=1_000.0).contains(&params.slippage_bps) {
        return Err(BacktestError::InvalidParams(
            "slippage_bps must be between 0 and 1000".into(),
        ));
    }
    if bars.len() < 2 {
        return Err(BacktestError::InvalidBars(
            "at least two bars are needed to fill a signal".into(),
        ));
    }
    if bars.len() > MAX_BARS {
        return Err(BacktestError::InvalidBars(format!(
            "{} bars is more than the {MAX_BARS} one replay may carry",
            bars.len()
        )));
    }
    for (i, bar) in bars.iter().enumerate() {
        let prices = [bar.open, bar.high, bar.low, bar.close];
        if prices.iter().any(|p| !p.is_finite() || *p <= 0.0) {
            return Err(BacktestError::InvalidBars(format!(
                "bar {i} has a non-positive or non-finite price"
            )));
        }
        if !bar.volume.is_finite() || bar.volume < 0.0 {
            return Err(BacktestError::InvalidBars(format!(
                "bar {i} has a negative or non-finite volume"
            )));
        }
        if bar.high < bar.open.max(bar.close) || bar.low > bar.open.min(bar.close) {
            return Err(BacktestError::InvalidBars(format!(
                "bar {i} has a high below its open/close or a low above them"
            )));
        }
        if i > 0 && bar.ts_ms <= bars[i - 1].ts_ms {
            return Err(BacktestError::InvalidBars(format!(
                "bar {i} does not come after bar {}",
                i - 1
            )));
        }
    }
    Ok(())
}

fn context(params: &BacktestParams, bar: &Bar, equity: f64, sim: &Simulation) -> serde_json::Value {
    let positions = if sim.qty.abs() > 1e-9 {
        vec![serde_json::json!({
            "symbol": params.symbol,
            "qty": sim.qty,
            "avg_entry": sim.avg_entry,
            "mark": bar.close,
        })]
    } else {
        Vec::new()
    };
    serde_json::json!({
        "ts_ms": bar.ts_ms,
        "symbol": params.symbol,
        "price": bar.close,
        "equity_usd": equity,
        "positions": positions,
        "params": params.params,
    })
}

/// Cash, position and the fee book of the replay.
struct Simulation {
    cash: f64,
    qty: f64,
    avg_entry: f64,
    /// Entry fees already paid on the open position, released as it closes.
    fees_carried: f64,
    fees_paid: f64,
    closed: Vec<f64>,
    trades: Vec<BacktestTrade>,
    rejected: BTreeMap<String, usize>,
    proposals: usize,
    fee_bps: f64,
    slippage_bps: f64,
}

impl Simulation {
    fn new(params: &BacktestParams) -> Self {
        Self {
            cash: params.initial_equity_usd,
            qty: 0.0,
            avg_entry: 0.0,
            fees_carried: 0.0,
            fees_paid: 0.0,
            closed: Vec::new(),
            trades: Vec::new(),
            rejected: BTreeMap::new(),
            proposals: 0,
            fee_bps: params.fee_bps,
            slippage_bps: params.slippage_bps,
        }
    }

    fn equity(&self, price: f64) -> f64 {
        self.cash + self.qty * price
    }

    /// Executes a market request at `open`, with slippage and commission.
    fn fill(
        &mut self,
        ts_ms: i64,
        open: f64,
        request: &OrderRequest,
        rationale: &str,
    ) -> BacktestTrade {
        let slip = self.slippage_bps / 10_000.0;
        let price = match request.side {
            Side::Buy => open * (1.0 + slip),
            Side::Sell => open * (1.0 - slip),
        };
        let notional = request.qty * price;
        let fee = notional * self.fee_bps / 10_000.0;
        self.fees_paid += fee;

        let signed = request.side.sign() * request.qty;
        let before = self.qty;
        let abs_before = before.abs();
        let after = before + signed;
        let mut realized = 0.0;

        let closing = if before.abs() > 1e-12 && before.signum() != signed.signum() {
            abs_before.min(request.qty)
        } else {
            0.0
        };
        if closing > 0.0 {
            let gross = if before > 0.0 {
                (price - self.avg_entry) * closing
            } else {
                (self.avg_entry - price) * closing
            };
            let entry_fee = self.fees_carried * (closing / abs_before);
            let closing_fee = fee * (closing / request.qty);
            realized = gross - entry_fee - closing_fee;
            self.fees_carried -= entry_fee;
            self.closed.push(realized);
        }

        if after.abs() < 1e-12 {
            self.qty = 0.0;
            self.avg_entry = 0.0;
            self.fees_carried = 0.0;
        } else if before.abs() < 1e-12 {
            self.qty = after;
            self.avg_entry = price;
            self.fees_carried = fee;
        } else if before.signum() == after.signum() {
            let abs_after = after.abs();
            self.avg_entry = (self.avg_entry * abs_before + price * request.qty) / abs_after;
            self.fees_carried += fee;
            self.qty = after;
        } else {
            // A flip closes the old position and opens the rest the other way.
            let remaining = request.qty - closing;
            self.qty = after;
            self.avg_entry = price;
            self.fees_carried = fee * (remaining / request.qty);
        }

        self.cash -= signed * price + fee;
        let trade = BacktestTrade {
            ts_ms,
            side: request.side,
            qty: request.qty,
            price,
            fee_usd: fee,
            realized_pnl_usd: realized,
            rationale: rationale.to_string(),
        };
        self.trades.push(trade.clone());
        trade
    }

    fn win_rate(&self) -> f64 {
        if self.closed.is_empty() {
            return 0.0;
        }
        let wins = self.closed.iter().filter(|pnl| **pnl > 0.0).count();
        wins as f64 / self.closed.len() as f64
    }

    fn profit_factor(&self) -> Option<f64> {
        if self.closed.is_empty() {
            return None;
        }
        let wins: f64 = self.closed.iter().filter(|p| **p > 0.0).sum();
        let losses: f64 = self.closed.iter().filter(|p| **p < 0.0).map(|p| -p).sum();
        if losses <= 0.0 {
            return None;
        }
        Some(wins / losses)
    }
}

fn drawdown(peak: f64, equity: f64) -> f64 {
    if peak <= 0.0 {
        return 0.0;
    }
    ((peak - equity) / peak * 100.0).max(0.0)
}

/// Per-bar Sharpe, scaled by the square root of bars per year.
fn sharpe(curve: &[EquityPoint], bars: &[Bar]) -> f64 {
    if curve.len() < 3 {
        return 0.0;
    }
    let mut returns = Vec::with_capacity(curve.len() - 1);
    for pair in curve.windows(2) {
        if pair[0].equity_usd > 0.0 {
            returns.push(pair[1].equity_usd / pair[0].equity_usd - 1.0);
        }
    }
    if returns.len() < 2 {
        return 0.0;
    }
    let mean = returns.iter().sum::<f64>() / returns.len() as f64;
    let variance =
        returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (returns.len() - 1) as f64;
    let std = variance.sqrt();
    if !std.is_finite() || std <= f64::EPSILON {
        return 0.0;
    }
    let mut gaps: Vec<i64> = bars
        .windows(2)
        .map(|pair| pair[1].ts_ms - pair[0].ts_ms)
        .collect();
    gaps.sort_unstable();
    let median = gaps[gaps.len() / 2];
    if median <= 0 {
        return 0.0;
    }
    const YEAR_MS: f64 = 365.25 * 24.0 * 60.0 * 60.0 * 1_000.0;
    let per_year = YEAR_MS / median as f64;
    mean / std * per_year.sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3_600_000;

    fn bar(ts_ms: i64, open: f64, close: f64) -> Bar {
        Bar {
            ts_ms,
            open,
            high: open.max(close) * 1.001,
            low: open.min(close) * 0.999,
            close,
            volume: 1.0,
        }
    }

    fn params() -> BacktestParams {
        BacktestParams {
            symbol: "SOL/USDT".into(),
            initial_equity_usd: 1_000.0,
            fee_bps: 10.0,
            slippage_bps: 0.0,
            params: serde_json::Value::Null,
        }
    }

    fn permissive() -> RiskConfig {
        RiskConfig {
            max_position_pct_of_equity: 100.0,
            max_gross_exposure_pct: 100.0,
            max_order_notional_usd: 1_000_000.0,
            min_order_notional_usd: 0.0,
            ..RiskConfig::default()
        }
    }

    fn signal(side: Side, qty: f64) -> StrategySignal {
        StrategySignal {
            symbol: "SOL/USDT".into(),
            side,
            qty,
            confidence: 1.0,
            reason: "test".into(),
        }
    }

    /// Emits the given signals on the bars whose timestamp is a key.
    fn scripted(
        script: Vec<(i64, Vec<StrategySignal>)>,
    ) -> impl FnMut(&serde_json::Value) -> Result<Vec<StrategySignal>, String> {
        move |ctx| {
            let ts = ctx["ts_ms"].as_i64().unwrap_or_default();
            Ok(script
                .iter()
                .find(|(when, _)| *when == ts)
                .map(|(_, signals)| signals.clone())
                .unwrap_or_default())
        }
    }

    #[test]
    fn a_signal_fills_at_the_next_open_and_the_fees_are_charged() {
        let bars = vec![
            bar(0, 100.0, 100.0),
            bar(HOUR, 100.0, 110.0),
            bar(2 * HOUR, 110.0, 120.0),
            bar(3 * HOUR, 120.0, 130.0),
            bar(4 * HOUR, 130.0, 130.0),
        ];
        let source = scripted(vec![
            (0, vec![signal(Side::Buy, 1.0)]),
            (HOUR, vec![signal(Side::Buy, 1.0)]),
            (3 * HOUR, vec![signal(Side::Sell, 2.0)]),
        ]);
        let report = run(&params(), &bars, permissive(), source).unwrap();

        assert_eq!(report.trades.len(), 3);
        assert_eq!(report.closed_trades, 1);
        assert_eq!(report.trades[0].price, 100.0);
        assert_eq!(report.trades[1].price, 110.0);
        assert_eq!(report.trades[2].price, 130.0);
        assert!((report.trades[0].fee_usd - 0.1).abs() < 1e-9);
        // (130 − 105) × 2, less the 0.21 of entry fees and the 0.26 exit fee.
        assert!((report.trades[2].realized_pnl_usd - 49.53).abs() < 1e-9);
        assert!((report.final_equity_usd - 1_049.53).abs() < 1e-9);
        assert!((report.total_return_pct - 4.953).abs() < 1e-9);
        assert_eq!(report.final_position_qty, 0.0);
        assert_eq!(report.win_rate, 1.0);
        assert_eq!(report.profit_factor, None);
        assert_eq!(report.final_breaker, BreakerState::Closed);
        assert_eq!(report.equity_curve.len(), bars.len());
        assert_eq!(report.first_ts_ms, 0);
        assert_eq!(report.last_ts_ms, 4 * HOUR);
    }

    #[test]
    fn the_real_risk_layer_refuses_an_oversized_position() {
        let bars = vec![bar(0, 100.0, 100.0), bar(60_000, 100.0, 100.0)];
        // $500 of a $1,000 account is 50%: the default 20% position cap trips
        // the breaker and halts the replay instead of filling.
        let report = run(
            &params(),
            &bars,
            RiskConfig::default(),
            scripted(vec![(0, vec![signal(Side::Buy, 5.0)])]),
        )
        .unwrap();

        assert!(report.trades.is_empty());
        assert_eq!(report.rejected["position_size_exceeds_max_pct"], 1);
        assert_eq!(report.breaker_trips, 1);
        assert_eq!(report.final_breaker, BreakerState::Open);
        assert!((report.final_equity_usd - 1_000.0).abs() < 1e-9);
    }

    #[test]
    fn an_open_breaker_keeps_refusing_later_signals() {
        let bars = vec![
            bar(0, 100.0, 100.0),
            bar(60_000, 100.0, 100.0),
            bar(120_000, 100.0, 100.0),
            bar(180_000, 100.0, 100.0),
        ];
        let report = run(
            &params(),
            &bars,
            RiskConfig::default(),
            scripted(vec![
                (0, vec![signal(Side::Buy, 5.0)]),
                (60_000, vec![signal(Side::Buy, 1.0)]),
                (120_000, vec![signal(Side::Buy, 1.0)]),
            ]),
        )
        .unwrap();

        assert!(report.trades.is_empty());
        assert_eq!(report.rejected["position_size_exceeds_max_pct"], 1);
        assert_eq!(report.rejected["breaker_open"], 2);
        assert_eq!(report.breaker_trips, 1);
        assert_eq!(report.final_breaker, BreakerState::Open);
    }

    #[test]
    fn the_daily_loss_limit_uses_the_simulated_clock() {
        let mut risk = permissive();
        risk.max_daily_loss_pct = 3.0;
        risk.max_drawdown_pct = 90.0;
        risk.breaker_cooldown_ms = 300_000;
        let bars = vec![
            bar(0, 100.0, 100.0),
            bar(12 * HOUR, 100.0, 60.0),
            bar(13 * HOUR, 60.0, 60.0),
        ];
        let report = run(
            &params(),
            &bars,
            risk,
            scripted(vec![(0, vec![signal(Side::Buy, 1.0)])]),
        )
        .unwrap();

        // One unit bought at 100 and marked at 60 is a 4% loss on the simulated
        // day, so the 3% daily limit halts trading. If the replay used the wall
        // clock, every bar would look like a new day and the loss would reset.
        assert_eq!(report.trades.len(), 1);
        assert_eq!(report.breaker_trips, 1);
        assert_eq!(report.final_breaker, BreakerState::HalfOpen);
    }

    #[test]
    fn a_short_round_trip_books_its_profit_on_the_cover() {
        let bars = vec![
            bar(0, 100.0, 100.0),
            bar(HOUR, 100.0, 100.0),
            bar(2 * HOUR, 90.0, 90.0),
            bar(3 * HOUR, 90.0, 90.0),
        ];
        let report = run(
            &params(),
            &bars,
            permissive(),
            scripted(vec![
                (0, vec![signal(Side::Sell, 1.0)]),
                (2 * HOUR, vec![signal(Side::Buy, 1.0)]),
            ]),
        )
        .unwrap();

        assert_eq!(report.trades.len(), 2);
        assert_eq!(report.trades[0].price, 100.0);
        assert_eq!(report.trades[1].price, 90.0);
        assert!((report.trades[1].realized_pnl_usd - 9.81).abs() < 1e-9);
        assert!((report.final_equity_usd - 1_009.81).abs() < 1e-9);
        assert_eq!(report.final_position_qty, 0.0);
    }

    #[test]
    fn a_signal_for_another_symbol_never_reaches_the_risk_layer() {
        let bars = vec![bar(0, 100.0, 100.0), bar(HOUR, 100.0, 100.0)];
        let mut other = signal(Side::Buy, 1.0);
        other.symbol = "ETH/USDT".into();
        let report = run(
            &params(),
            &bars,
            permissive(),
            scripted(vec![(0, vec![other])]),
        )
        .unwrap();

        assert!(report.trades.is_empty());
        assert_eq!(report.rejected["symbol_mismatch"], 1);
        assert_eq!(report.breaker_trips, 0);
    }

    #[test]
    fn the_strategy_only_sees_closed_bars() {
        let bars = vec![
            bar(0, 100.0, 101.0),
            bar(HOUR, 101.0, 102.0),
            bar(2 * HOUR, 102.0, 103.0),
        ];
        let mut seen = Vec::new();
        let report = run(&params(), &bars, permissive(), |ctx| {
            seen.push((
                ctx["ts_ms"].as_i64().unwrap_or_default(),
                ctx["price"].as_f64().unwrap_or_default(),
            ));
            Ok(Vec::new())
        })
        .unwrap();

        assert_eq!(report.trades.len(), 0);
        assert_eq!(
            seen,
            vec![(0, 101.0), (HOUR, 102.0)],
            "one call per bar that still has a next open to fill into"
        );
    }

    #[test]
    fn malformed_bars_are_refused_before_the_strategy_runs() {
        let params = params();
        let mut called = false;

        let mut unsorted = vec![bar(HOUR, 100.0, 100.0), bar(0, 100.0, 100.0)];
        let error = run(&params, &unsorted, permissive(), |_| {
            called = true;
            Ok(Vec::new())
        })
        .unwrap_err();
        assert!(matches!(error, BacktestError::InvalidBars(_)));
        assert!(!called);

        unsorted[0].ts_ms = 0;
        unsorted[1].ts_ms = HOUR;
        unsorted[0].close = 0.0;
        let error = run(&params, &unsorted, permissive(), |_| Ok(Vec::new())).unwrap_err();
        assert!(matches!(error, BacktestError::InvalidBars(_)));

        let one = vec![bar(0, 100.0, 100.0)];
        let error = run(&params, &one, permissive(), |_| Ok(Vec::new())).unwrap_err();
        assert!(matches!(error, BacktestError::InvalidBars(_)));

        let mut negative_fee = params.clone();
        negative_fee.fee_bps = -1.0;
        let error = run(
            &negative_fee,
            &[bar(0, 100.0, 100.0), bar(HOUR, 100.0, 100.0)],
            permissive(),
            |_| Ok(Vec::new()),
        )
        .unwrap_err();
        assert!(matches!(error, BacktestError::InvalidParams(_)));
    }

    #[test]
    fn a_strategy_error_names_the_bar_that_caused_it() {
        let bars = vec![bar(0, 100.0, 100.0), bar(HOUR, 100.0, 100.0)];
        let error = run(&params(), &bars, permissive(), |_| {
            Err("the guest trapped".into())
        })
        .unwrap_err();
        match error {
            BacktestError::Strategy {
                bar,
                ts_ms,
                message,
            } => {
                assert_eq!(bar, 0);
                assert_eq!(ts_ms, 0);
                assert_eq!(message, "the guest trapped");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn a_volatile_series_is_sized_down_and_a_flat_one_is_not() {
        let signal_at = 12;
        let script = scripted(vec![(signal_at * HOUR, vec![signal(Side::Buy, 1.0)])]);

        let flat: Vec<Bar> = (0..16).map(|i| bar(i * HOUR, 100.0, 100.0)).collect();
        let calm = run(&params(), &flat, permissive(), script).unwrap();
        assert_eq!(calm.trades[0].qty, 1.0);

        let mut volatile = Vec::new();
        let mut prev = 100.0;
        for i in 0..16 {
            let close = if i == 0 {
                100.0
            } else if i % 2 == 1 {
                prev * 1.05
            } else {
                prev * 0.95
            };
            volatile.push(bar(i * HOUR, prev, close));
            prev = close;
        }
        let mut script = scripted(vec![(signal_at * HOUR, vec![signal(Side::Buy, 1.0)])]);
        let hot = run(&params(), &volatile, permissive(), &mut script).unwrap();

        assert!(hot.trades[0].qty < 1.0, "qty was {}", hot.trades[0].qty);
        assert!(hot.trades[0].qty >= 0.25, "qty was {}", hot.trades[0].qty);
    }
}

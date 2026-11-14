use crate::metrics::Metrics;
use crate::types::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BreakerState {
    Closed,
    HalfOpen,
    Open,
}

impl BreakerState {
    pub fn as_str(self) -> &'static str {
        match self {
            BreakerState::Closed => "closed",
            BreakerState::HalfOpen => "half_open",
            BreakerState::Open => "open",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "closed" => Some(BreakerState::Closed),
            "half_open" | "halfopen" => Some(BreakerState::HalfOpen),
            "open" => Some(BreakerState::Open),
            _ => None,
        }
    }

    fn gauge(self) -> f64 {
        match self {
            BreakerState::Closed => 0.0,
            BreakerState::HalfOpen => 1.0,
            BreakerState::Open => 2.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RiskConfig {
    /// A single position may not exceed this share of the portfolio.
    pub max_position_pct_of_equity: f64,
    pub max_gross_exposure_pct: f64,
    pub max_daily_loss_pct: f64,
    pub max_drawdown_pct: f64,
    pub max_open_positions: usize,
    pub max_order_notional_usd: f64,
    pub min_order_notional_usd: f64,
    pub symbol_allowlist: HashSet<String>,
    pub venue_allowlist: HashSet<Venue>,
    pub contract_allowlist: HashSet<String>,
    pub breaker_cooldown_ms: i64,
    /// Per the operating rules: a trade larger than the position cap halts trading
    /// entirely instead of only rejecting that single order.
    pub hard_halt_on_oversized_order: bool,
    /// Positions whose recent returns move together may not add up to more
    /// than this share of equity. `0` disables the check.
    pub max_correlated_exposure_pct: f64,
    /// |Pearson r| at or above this counts two markets as moving together.
    pub correlation_threshold: f64,
    /// The per-sample volatility a position is sized for, in percent. `0`
    /// disables volatility targeting.
    pub volatility_target_pct: f64,
    /// How many recent returns feed the volatility and correlation estimates.
    pub volatility_window: usize,
    /// The smallest multiplier volatility targeting may apply. A cap on how
    /// far a calm target may shrink a high-volatility trade.
    pub min_size_scale: f64,
}

impl Default for RiskConfig {
    fn default() -> Self {
        let mut symbols = HashSet::new();
        symbols.insert("SOL/USDT".to_string());
        let mut venues = HashSet::new();
        venues.insert(Venue::Paper);
        Self {
            max_position_pct_of_equity: 20.0,
            max_gross_exposure_pct: 60.0,
            max_daily_loss_pct: 3.0,
            max_drawdown_pct: 10.0,
            max_open_positions: 5,
            max_order_notional_usd: 5000.0,
            min_order_notional_usd: 10.0,
            symbol_allowlist: symbols,
            venue_allowlist: venues,
            contract_allowlist: HashSet::new(),
            breaker_cooldown_ms: 300_000,
            hard_halt_on_oversized_order: true,
            max_correlated_exposure_pct: 40.0,
            correlation_threshold: 0.7,
            volatility_target_pct: 2.0,
            volatility_window: 60,
            min_size_scale: 0.25,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskLimits {
    pub max_position_pct_of_equity: f64,
    pub max_gross_exposure_pct: f64,
    pub max_daily_loss_pct: f64,
    pub max_drawdown_pct: f64,
    pub max_open_positions: usize,
    pub max_order_notional_usd: f64,
    pub min_order_notional_usd: f64,
    pub max_correlated_exposure_pct: f64,
    pub correlation_threshold: f64,
    pub volatility_target_pct: f64,
    pub volatility_window: usize,
    pub min_size_scale: f64,
}

impl From<&RiskConfig> for RiskLimits {
    fn from(c: &RiskConfig) -> Self {
        Self {
            max_position_pct_of_equity: c.max_position_pct_of_equity,
            max_gross_exposure_pct: c.max_gross_exposure_pct,
            max_daily_loss_pct: c.max_daily_loss_pct,
            max_drawdown_pct: c.max_drawdown_pct,
            max_open_positions: c.max_open_positions,
            max_order_notional_usd: c.max_order_notional_usd,
            min_order_notional_usd: c.min_order_notional_usd,
            max_correlated_exposure_pct: c.max_correlated_exposure_pct,
            correlation_threshold: c.correlation_threshold,
            volatility_target_pct: c.volatility_target_pct,
            volatility_window: c.volatility_window,
            min_size_scale: c.min_size_scale,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskState {
    pub breaker_state: BreakerState,
    pub breaker_reason: Option<String>,
    pub equity_usd: f64,
    pub peak_equity_usd: f64,
    pub drawdown_pct: f64,
    pub daily_pnl_usd: f64,
    pub open_positions: usize,
    pub gross_exposure_usd: f64,
    pub limits: RiskLimits,
    pub ts_ms: i64,
}

/// The risk engine's mutable state in the shape it is persisted in.
///
/// This is the part of the risk layer that must survive a restart: the peak
/// equity the drawdown limit is measured from, the day's opening equity the
/// daily-loss limit is measured from, and whether the breaker is open.
#[derive(Debug, Clone, PartialEq)]
pub struct RiskRow {
    pub breaker: BreakerState,
    pub breaker_reason: Option<String>,
    pub breaker_opened_at_ms: i64,
    pub peak_equity: f64,
    pub day_start_equity: f64,
    pub day_start_ms: i64,
    pub daily_realized_pnl: f64,
}

impl Default for RiskRow {
    fn default() -> Self {
        Self {
            breaker: BreakerState::Closed,
            breaker_reason: None,
            breaker_opened_at_ms: 0,
            peak_equity: 0.0,
            day_start_equity: 0.0,
            day_start_ms: 0,
            daily_realized_pnl: 0.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskVerdict {
    pub approved: bool,
    pub reason: String,
    pub detail: String,
    pub breaker_state: BreakerState,
    pub checked_at_ms: i64,
}

impl RiskVerdict {
    fn approve(detail: impl Into<String>) -> Self {
        Self {
            approved: true,
            reason: "approved".into(),
            detail: detail.into(),
            breaker_state: BreakerState::Closed,
            checked_at_ms: now_ms(),
        }
    }

    fn reject(reason: &str, detail: impl Into<String>, state: BreakerState) -> Self {
        Self {
            approved: false,
            reason: reason.to_string(),
            detail: detail.into(),
            breaker_state: state,
            checked_at_ms: now_ms(),
        }
    }
}

/// Everything the risk layer needs to judge an order.
#[derive(Debug, Clone, Default)]
pub struct RiskContext {
    pub equity_usd: f64,
    pub gross_exposure_usd: f64,
    pub open_positions: usize,
    /// Signed size already held on this symbol.
    pub current_position_qty: f64,
    pub price: f64,
    /// Contracts / protocols the order would touch (on-chain venues).
    pub contracts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskDecision {
    pub ts_ms: i64,
    pub symbol: String,
    pub side: Side,
    pub qty: f64,
    pub notional_usd: f64,
    pub approved: bool,
    pub reason: String,
    pub detail: String,
    pub breaker_state: BreakerState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BreakerTrip {
    pub ts_ms: i64,
    pub reason: String,
    pub detail: String,
}

#[derive(Debug, Default)]
struct RiskStateInner {
    breaker: Option<BreakerState>,
    breaker_reason: Option<String>,
    breaker_opened_at_ms: i64,
    peak_equity: f64,
    day_start_equity: f64,
    day_start_ms: i64,
    daily_realized_pnl: f64,
    last_equity: f64,
    open_positions: usize,
    gross_exposure_usd: f64,
    position_notional: HashMap<String, f64>,
    position_qty: HashMap<String, f64>,
    /// Recent prices per symbol, used for the volatility and correlation
    /// estimates. Kept in the risk layer because both are risk inputs.
    prices: HashMap<String, VecDeque<f64>>,
    probe_in_flight: bool,
}

/// How many prices are kept per symbol. Enough for a 60-sample volatility
/// window with room for a longer configured one, without unbounded growth.
const PRICE_HISTORY: usize = 512;
/// Fewer returns than this cannot describe volatility.
const MIN_VOLATILITY_SAMPLES: usize = 10;
/// Fewer paired returns than this cannot describe a correlation.
const MIN_CORRELATION_SAMPLES: usize = 20;

/// The non-bypassable risk layer.
///
/// Every order that reaches a venue passes through `evaluate` first. The engine
/// exposes no path that skips it, and the AI layer has no way to alter the
/// configuration: it can only ask for an order, which is then judged here.
pub struct RiskEngine {
    config: RiskConfig,
    state: Mutex<RiskStateInner>,
    audit: Mutex<VecDeque<RiskDecision>>,
    metrics: Arc<Metrics>,
}

impl RiskEngine {
    pub fn new(config: RiskConfig, metrics: Arc<Metrics>) -> Self {
        Self {
            config,
            state: Mutex::new(RiskStateInner {
                breaker: Some(BreakerState::Closed),
                breaker_reason: None,
                breaker_opened_at_ms: 0,
                peak_equity: 0.0,
                day_start_equity: 0.0,
                day_start_ms: now_ms(),
                daily_realized_pnl: 0.0,
                last_equity: 0.0,
                open_positions: 0,
                gross_exposure_usd: 0.0,
                position_notional: HashMap::new(),
                position_qty: HashMap::new(),
                prices: HashMap::new(),
                probe_in_flight: false,
            }),
            audit: Mutex::new(VecDeque::with_capacity(512)),
            metrics,
        }
    }

    pub fn config(&self) -> &RiskConfig {
        &self.config
    }

    pub fn breaker_state(&self) -> BreakerState {
        let mut state = self.lock();
        self.refresh_breaker(&mut state, now_ms());
        state.breaker.unwrap_or(BreakerState::Closed)
    }

    /// Refreshes equity-derived limits. Returns a trip when the breaker opened.
    pub fn update_equity(&self, equity_usd: f64, ts_ms: i64) -> Option<BreakerTrip> {
        if !equity_usd.is_finite() || equity_usd <= 0.0 {
            return None;
        }
        let mut state = self.lock();
        state.last_equity = equity_usd;
        if state.peak_equity <= 0.0 {
            state.peak_equity = equity_usd;
        }
        if equity_usd > state.peak_equity {
            state.peak_equity = equity_usd;
        }
        if state.day_start_equity <= 0.0 || day_changed(state.day_start_ms, ts_ms) {
            state.day_start_equity = equity_usd;
            state.day_start_ms = ts_ms;
            state.daily_realized_pnl = 0.0;
        }

        let drawdown_pct = drawdown_pct(state.peak_equity, equity_usd);
        self.metrics
            .gauge_set("openether_drawdown_pct", &[], drawdown_pct);
        self.metrics
            .gauge_set("openether_equity_usd", &[], equity_usd);

        let daily_loss_pct = if state.day_start_equity > 0.0 {
            ((state.day_start_equity - equity_usd) / state.day_start_equity) * 100.0
        } else {
            0.0
        };

        if drawdown_pct >= self.config.max_drawdown_pct {
            return self.trip_locked(
                &mut state,
                "drawdown_limit",
                format!(
                    "drawdown {:.2}% reached the {:.2}% limit",
                    drawdown_pct, self.config.max_drawdown_pct
                ),
                ts_ms,
            );
        }
        if daily_loss_pct >= self.config.max_daily_loss_pct {
            return self.trip_locked(
                &mut state,
                "daily_loss_limit",
                format!(
                    "daily loss {:.2}% reached the {:.2}% limit",
                    daily_loss_pct, self.config.max_daily_loss_pct
                ),
                ts_ms,
            );
        }
        None
    }

    /// Records a fill so exposure and position counts stay in sync with the venue.
    pub fn on_fill(&self, fill: &OrderFill, equity_after: f64) {
        let ts_ms = fill.ts_ms;
        let mut state = self.lock();
        let symbol = fill.symbol.clone();
        let signed_qty = fill.side.sign() * fill.qty;
        let prev_qty = state.position_qty.get(&symbol).copied().unwrap_or(0.0);
        let new_qty = prev_qty + signed_qty;
        state.position_qty.insert(symbol.clone(), new_qty);
        let notional = new_qty.abs() * fill.price;
        state.position_notional.insert(symbol.clone(), notional);
        state.gross_exposure_usd = state.position_notional.values().sum();
        state.open_positions = state
            .position_notional
            .values()
            .filter(|n| **n > 0.01)
            .count();
        if state.probe_in_flight {
            state.probe_in_flight = false;
            self.close_breaker_locked(&mut state, ts_ms);
        }
        drop(state);
        self.metrics
            .gauge_set("openether_gross_exposure_usd", &[], self.gross_exposure());
        self.metrics
            .gauge_set("openether_pnl_usd", &[], self.daily_realized_pnl());
        if equity_after > 0.0 {
            let _ = self.update_equity(equity_after, ts_ms);
        }
    }

    pub fn gross_exposure(&self) -> f64 {
        self.lock().gross_exposure_usd
    }

    pub fn daily_realized_pnl(&self) -> f64 {
        self.lock().daily_realized_pnl
    }

    pub fn position_qty(&self, symbol: &str) -> f64 {
        self.lock().position_qty.get(symbol).copied().unwrap_or(0.0)
    }

    pub fn open_positions(&self) -> usize {
        self.lock().open_positions
    }

    /// Records a market price.
    ///
    /// Volatility targeting and the correlation cap both read this history, so
    /// neither has to guess from a single quote. Non-positive and non-finite
    /// prices are ignored rather than poisoning the estimates.
    pub fn observe_price(&self, symbol: &str, price: f64, _ts_ms: i64) {
        if symbol.trim().is_empty() || !price.is_finite() || price <= 0.0 {
            return;
        }
        let mut state = self.lock();
        let history = state.prices.entry(symbol.to_string()).or_default();
        history.push_back(price);
        if history.len() > PRICE_HISTORY {
            history.pop_front();
        }
    }

    /// Standard deviation of recent simple returns, in percent.
    pub fn realized_volatility_pct(&self, symbol: &str) -> Option<f64> {
        let state = self.lock();
        let window = self.config.volatility_window;
        let returns = recent_returns(state.prices.get(symbol)?, window);
        if returns.len() < MIN_VOLATILITY_SAMPLES {
            return None;
        }
        Some(stdev(&returns) * 100.0)
    }

    /// The multiplier volatility targeting applies to a new position.
    ///
    /// `1.0` until there is enough history or when the market is calmer than
    /// the target. A hot market is sized down towards the target, never below
    /// `min_size_scale`, so the trade still exists.
    pub fn volatility_scale(&self, symbol: &str) -> f64 {
        let target = self.config.volatility_target_pct;
        if target <= 0.0 {
            return 1.0;
        }
        let Some(volatility) = self.realized_volatility_pct(symbol) else {
            return 1.0;
        };
        if volatility <= target {
            return 1.0;
        }
        (target / volatility).clamp(self.config.min_size_scale.clamp(0.01, 1.0), 1.0)
    }

    /// The order size after volatility targeting, and the multiplier used.
    ///
    /// Only an order that increases absolute exposure is scaled: shrinking an
    /// exit would leave risk on the book, which is the opposite of the point.
    pub fn scale_for_volatility(
        &self,
        side: Side,
        qty: f64,
        current_qty: f64,
        symbol: &str,
    ) -> (f64, f64) {
        let resulting = (current_qty + side.sign() * qty.abs()).abs();
        if resulting <= current_qty.abs() {
            return (qty, 1.0);
        }
        let scale = self.volatility_scale(symbol);
        if scale >= 1.0 {
            return (qty, 1.0);
        }
        let scaled = (qty * scale * 10_000.0).floor() / 10_000.0;
        (scaled, scale)
    }

    /// Pearson correlation of the two symbols' recent returns.
    pub fn correlation(&self, a: &str, b: &str) -> Option<f64> {
        let state = self.lock();
        pair_correlation(&state, a, b, self.config.volatility_window)
    }

    /// The exposure an order would add to a cluster of positions that move
    /// with `symbol`, as `(total_usd, |rho|, other_symbol)`.
    ///
    /// `None` when the symbol has no correlated sibling, so the cap only
    /// speaks once there is a real cluster to measure.
    pub fn correlated_exposure(
        &self,
        symbol: &str,
        order_notional: f64,
    ) -> Option<(f64, f64, String)> {
        let state = self.lock();
        correlated_exposure_locked(&self.config, &state, symbol, order_notional)
    }

    /// The single gate every order must pass.
    pub fn evaluate(&self, req: &OrderRequest, ctx: &RiskContext) -> RiskVerdict {
        self.evaluate_at(req, ctx, now_ms())
    }

    /// The same gate, judged at a caller-supplied time.
    ///
    /// A backtest replays months of bars in seconds, so the wall clock would
    /// never let a breaker cooldown expire or a trading day roll over. The
    /// simulated timestamp is what keeps those rules honest during a replay.
    pub fn evaluate_at(&self, req: &OrderRequest, ctx: &RiskContext, ts_ms: i64) -> RiskVerdict {
        let mut state = self.lock();
        self.refresh_breaker(&mut state, ts_ms);
        let breaker_state = state.breaker.unwrap_or(BreakerState::Closed);

        let verdict = self.evaluate_locked(req, ctx, &mut state, breaker_state, ts_ms);
        self.audit_decision(req, ctx, &verdict);
        verdict
    }

    fn evaluate_locked(
        &self,
        req: &OrderRequest,
        ctx: &RiskContext,
        state: &mut RiskStateInner,
        breaker_state: BreakerState,
        ts_ms: i64,
    ) -> RiskVerdict {
        if breaker_state == BreakerState::Open {
            let detail = format!(
                "trading halted: {}",
                state
                    .breaker_reason
                    .clone()
                    .unwrap_or_else(|| "unknown".into())
            );
            return RiskVerdict::reject("breaker_open", detail, breaker_state);
        }

        if !req.qty.is_finite() || req.qty <= 0.0 {
            return RiskVerdict::reject(
                "invalid_quantity",
                format!("quantity must be a positive finite number, got {}", req.qty),
                breaker_state,
            );
        }
        if !ctx.price.is_finite() || ctx.price <= 0.0 {
            return RiskVerdict::reject(
                "invalid_price",
                format!("price must be a positive finite number, got {}", ctx.price),
                breaker_state,
            );
        }
        // A conditional order is an exit the venue holds for later, so it has to
        // be a real price, still ahead of the market, and unable to open new
        // exposure: a pending entry would be a position the limits never sized.
        if req.order_type.is_conditional() {
            let Some(trigger) = req.trigger_price.filter(|t| t.is_finite() && *t > 0.0) else {
                return RiskVerdict::reject(
                    "invalid_trigger",
                    format!(
                        "a {} order needs a positive finite trigger price, got {}",
                        req.order_type.as_str(),
                        req.trigger_price
                            .map(|t| t.to_string())
                            .unwrap_or_else(|| "none".into())
                    ),
                    breaker_state,
                );
            };
            if req.order_type.trigger_hit(req.side, trigger, ctx.price) {
                return RiskVerdict::reject(
                    "trigger_already_crossed",
                    format!(
                        "the {} trigger {trigger} is already crossed by the market price {}; a conditional order that fires immediately is a market order",
                        req.order_type.as_str(),
                        ctx.price
                    ),
                    breaker_state,
                );
            }
            if !req.reduce_only {
                return RiskVerdict::reject(
                    "conditional_not_reduce_only",
                    "a stop or take-profit must be reduce_only: it closes a position and may never open one".to_string(),
                    breaker_state,
                );
            }
        }
        if !ctx.equity_usd.is_finite() || ctx.equity_usd <= 0.0 {
            return RiskVerdict::reject(
                "invalid_equity",
                "equity is unknown; refusing to size the order".to_string(),
                breaker_state,
            );
        }
        if req.symbol.trim().is_empty() {
            return RiskVerdict::reject(
                "invalid_symbol",
                "symbol must not be empty".to_string(),
                breaker_state,
            );
        }
        if !self.config.symbol_allowlist.is_empty()
            && !self.config.symbol_allowlist.contains(&req.symbol)
        {
            return RiskVerdict::reject(
                "symbol_not_allowed",
                format!("{} is not on the symbol allowlist", req.symbol),
                breaker_state,
            );
        }
        if !self.config.venue_allowlist.is_empty()
            && !self.config.venue_allowlist.contains(&req.venue)
        {
            return RiskVerdict::reject(
                "venue_not_allowed",
                format!("{} is not on the venue allowlist", req.venue),
                breaker_state,
            );
        }
        for contract in &ctx.contracts {
            if !self.config.contract_allowlist.is_empty()
                && !self.config.contract_allowlist.contains(contract)
            {
                return RiskVerdict::reject(
                    "contract_not_allowed",
                    format!("contract {contract} is not on the contract allowlist"),
                    breaker_state,
                );
            }
        }

        let notional = req.qty.abs() * ctx.price;
        if notional < self.config.min_order_notional_usd {
            return RiskVerdict::reject(
                "notional_too_small",
                format!(
                    "notional ${notional:.2} is below the ${:.2} minimum",
                    self.config.min_order_notional_usd
                ),
                breaker_state,
            );
        }
        if notional > self.config.max_order_notional_usd {
            return RiskVerdict::reject(
                "notional_too_large",
                format!(
                    "notional ${notional:.2} exceeds the ${:.2} per-order cap",
                    self.config.max_order_notional_usd
                ),
                breaker_state,
            );
        }

        // Position sizing: the resulting position may not exceed the per-symbol cap.
        let current_qty = ctx.current_position_qty;
        let signed_qty = req.side.sign() * req.qty.abs();
        let resulting_qty = current_qty + signed_qty;
        let position_cap = ctx.equity_usd * self.config.max_position_pct_of_equity / 100.0;
        let resulting_notional = resulting_qty.abs() * ctx.price;

        if req.reduce_only && resulting_qty.abs() > current_qty.abs() {
            return RiskVerdict::reject(
                "reduce_only_violation",
                "a reduce-only order may not increase the position size".to_string(),
                breaker_state,
            );
        }

        if resulting_notional > position_cap {
            let detail = format!(
                "position would be ${resulting_notional:.2} ({:.2}% of equity), cap is ${position_cap:.2} ({:.2}%)",
                resulting_notional / ctx.equity_usd * 100.0,
                self.config.max_position_pct_of_equity
            );
            if self.config.hard_halt_on_oversized_order {
                self.trip_locked(
                    state,
                    "position_size_exceeds_max_pct",
                    detail.clone(),
                    ts_ms,
                );
                return RiskVerdict::reject(
                    "position_size_exceeds_max_pct",
                    format!("{detail}. Trading halted by the circuit breaker"),
                    BreakerState::Open,
                );
            }
            return RiskVerdict::reject("position_size_exceeds_max_pct", detail, breaker_state);
        }

        // Gross exposure cap.
        let projected_exposure = ctx.gross_exposure_usd + notional;
        let exposure_cap = ctx.equity_usd * self.config.max_gross_exposure_pct / 100.0;
        if projected_exposure > exposure_cap {
            return RiskVerdict::reject(
                "gross_exposure_exceeded",
                format!(
                    "projected exposure ${projected_exposure:.2} exceeds ${exposure_cap:.2} ({:.2}% of equity)",
                    self.config.max_gross_exposure_pct
                ),
                breaker_state,
            );
        }

        // Correlation cap: two markets that move together are one bet, so
        // their notionals share a budget the gross-exposure cap cannot see.
        if !req.reduce_only
            && resulting_qty.abs() > current_qty.abs()
            && self.config.max_correlated_exposure_pct > 0.0
        {
            let cap = ctx.equity_usd * self.config.max_correlated_exposure_pct / 100.0;
            if let Some((exposure, rho, other)) =
                correlated_exposure_locked(&self.config, state, &req.symbol, notional)
            {
                if exposure > cap {
                    return RiskVerdict::reject(
                        "correlated_exposure_exceeded",
                        format!(
                            "{} moves with {other} (rho {rho:.2}); the correlated cluster would be ${exposure:.2}, cap is ${cap:.2} ({:.1}% of equity)",
                            req.symbol, self.config.max_correlated_exposure_pct
                        ),
                        breaker_state,
                    );
                }
            }
        }

        // Open position count cap (a position flip is still one symbol).
        let is_new_position = current_qty.abs() < 0.000_001;
        if is_new_position && ctx.open_positions >= self.config.max_open_positions {
            return RiskVerdict::reject(
                "max_open_positions",
                format!(
                    "{} open positions already (limit {})",
                    ctx.open_positions, self.config.max_open_positions
                ),
                breaker_state,
            );
        }

        if breaker_state == BreakerState::HalfOpen {
            // Only a small probe order may re-open the door.
            let probe_cap = ctx.equity_usd * (self.config.max_position_pct_of_equity / 4.0) / 100.0;
            if notional > probe_cap {
                return RiskVerdict::reject(
                    "half_open_probe_too_large",
                    format!(
                        "half-open breaker allows a probe of at most ${probe_cap:.2}, got ${notional:.2}"
                    ),
                    breaker_state,
                );
            }
            state.probe_in_flight = true;
            let mut verdict = RiskVerdict::approve(format!(
                "probe order approved while the breaker is half-open (${notional:.2})"
            ));
            verdict.breaker_state = breaker_state;
            return verdict;
        }

        RiskVerdict::approve(format!(
            "within limits: ${notional:.2} order, {resulting_notional:.2} position ({:.2}% of equity)",
            resulting_notional / ctx.equity_usd * 100.0
        ))
    }

    /// Called by the execution service when a probe order fails to execute.
    pub fn note_probe_failure(&self, detail: impl Into<String>) {
        let mut state = self.lock();
        if state.breaker == Some(BreakerState::HalfOpen) {
            state.probe_in_flight = false;
            self.trip_locked(
                &mut state,
                "half_open_probe_failed",
                format!("probe order failed: {}", detail.into()),
                now_ms(),
            );
        }
    }

    /// Manual reset. Requires an explicit confirmation flag so an agent cannot
    /// silently clear a halt.
    pub fn reset_breaker(&self, confirm: bool) -> Result<(), String> {
        if !confirm {
            return Err("breaker reset requires an explicit confirmation".into());
        }
        let mut state = self.lock();
        state.breaker = Some(BreakerState::Closed);
        state.breaker_reason = None;
        state.breaker_opened_at_ms = 0;
        state.probe_in_flight = false;
        self.metrics
            .gauge_set("openether_breaker_state", &[], BreakerState::Closed.gauge());
        Ok(())
    }

    pub fn snapshot(&self, equity_usd: f64, ts_ms: i64) -> RiskState {
        let mut state = self.lock();
        self.refresh_breaker(&mut state, ts_ms);
        let breaker_state = state.breaker.unwrap_or(BreakerState::Closed);
        let peak = if state.peak_equity > 0.0 {
            state.peak_equity
        } else {
            equity_usd
        };
        RiskState {
            breaker_state,
            breaker_reason: state.breaker_reason.clone(),
            equity_usd,
            peak_equity_usd: peak,
            drawdown_pct: drawdown_pct(peak, equity_usd),
            daily_pnl_usd: equity_usd - state.day_start_equity,
            open_positions: state.open_positions,
            gross_exposure_usd: state.gross_exposure_usd,
            limits: RiskLimits::from(&self.config),
            ts_ms,
        }
    }

    /// Export the mutable state for persistence.
    pub fn export(&self) -> RiskRow {
        let state = self.lock();
        RiskRow {
            breaker: state.breaker.unwrap_or(BreakerState::Closed),
            breaker_reason: state.breaker_reason.clone(),
            breaker_opened_at_ms: state.breaker_opened_at_ms,
            peak_equity: state.peak_equity,
            day_start_equity: state.day_start_equity,
            day_start_ms: state.day_start_ms,
            daily_realized_pnl: state.daily_realized_pnl,
        }
    }

    /// Restore the mutable state after a restart.
    ///
    /// A breaker that was open when the process died stays open: restarting
    /// must not be a way around the risk layer. The position map is restored
    /// as well, because the gross-exposure cap is measured from it — without
    /// it a restarted process would believe it holds no positions and would
    /// happily open a fresh full-size position.
    pub fn restore(&self, row: &RiskRow, positions: &[(String, f64, f64)]) {
        let mut state = self.lock();
        state.breaker = Some(row.breaker);
        state.breaker_reason = row.breaker_reason.clone();
        state.breaker_opened_at_ms = row.breaker_opened_at_ms;
        state.peak_equity = row.peak_equity;
        state.day_start_equity = row.day_start_equity;
        state.day_start_ms = row.day_start_ms;
        state.daily_realized_pnl = row.daily_realized_pnl;

        state.position_qty.clear();
        state.position_notional.clear();
        for (symbol, qty, avg_entry) in positions {
            state.position_qty.insert(symbol.clone(), *qty);
            state
                .position_notional
                .insert(symbol.clone(), qty.abs() * avg_entry);
        }
        state.gross_exposure_usd = state.position_notional.values().sum();
        state.open_positions = state
            .position_notional
            .values()
            .filter(|n| **n > 0.01)
            .count();

        self.metrics
            .gauge_set("openether_breaker_state", &[], row.breaker.gauge());
        self.metrics.gauge_set(
            "openether_gross_exposure_usd",
            &[],
            state.gross_exposure_usd,
        );
    }

    pub fn decisions(&self, limit: usize) -> Vec<RiskDecision> {
        let audit = self.lock_audit();
        let skip = audit.len().saturating_sub(limit);
        audit.iter().skip(skip).cloned().collect()
    }

    fn refresh_breaker(&self, state: &mut RiskStateInner, ts_ms: i64) {
        if state.breaker == Some(BreakerState::Open)
            && ts_ms - state.breaker_opened_at_ms >= self.config.breaker_cooldown_ms
        {
            state.breaker = Some(BreakerState::HalfOpen);
            state.probe_in_flight = false;
            self.metrics.gauge_set(
                "openether_breaker_state",
                &[],
                BreakerState::HalfOpen.gauge(),
            );
            tracing::warn!(
                cooldown_ms = self.config.breaker_cooldown_ms,
                "circuit breaker moved to half-open; a probe order is required"
            );
        }
    }

    fn close_breaker_locked(&self, state: &mut RiskStateInner, ts_ms: i64) {
        state.breaker = Some(BreakerState::Closed);
        state.breaker_reason = None;
        state.breaker_opened_at_ms = 0;
        self.metrics
            .gauge_set("openether_breaker_state", &[], BreakerState::Closed.gauge());
        tracing::info!(ts_ms, "circuit breaker closed after a successful probe");
    }

    fn trip_locked(
        &self,
        state: &mut RiskStateInner,
        reason: &str,
        detail: String,
        ts_ms: i64,
    ) -> Option<BreakerTrip> {
        if state.breaker == Some(BreakerState::Open) {
            return None;
        }
        state.breaker = Some(BreakerState::Open);
        state.breaker_reason = Some(reason.to_string());
        state.breaker_opened_at_ms = ts_ms;
        state.probe_in_flight = false;
        self.metrics
            .gauge_set("openether_breaker_state", &[], BreakerState::Open.gauge());
        self.metrics
            .counter_add("openether_breaker_trips_total", &[("reason", reason)], 1.0);
        tracing::error!(reason, detail, "circuit breaker opened; trading halted");
        Some(BreakerTrip {
            ts_ms,
            reason: reason.to_string(),
            detail,
        })
    }

    fn audit_decision(&self, req: &OrderRequest, ctx: &RiskContext, verdict: &RiskVerdict) {
        let decision = RiskDecision {
            ts_ms: verdict.checked_at_ms,
            symbol: req.symbol.clone(),
            side: req.side,
            qty: req.qty,
            notional_usd: req.qty.abs() * ctx.price,
            approved: verdict.approved,
            reason: verdict.reason.clone(),
            detail: verdict.detail.clone(),
            breaker_state: verdict.breaker_state,
        };
        let mut audit = self.lock_audit();
        if audit.len() >= 512 {
            audit.pop_front();
        }
        audit.push_back(decision);
        self.metrics.counter_add(
            "openether_risk_checks_total",
            &[(
                "result",
                if verdict.approved {
                    "approved"
                } else {
                    "rejected"
                },
            )],
            1.0,
        );
        if !verdict.approved {
            self.metrics.counter_add(
                "openether_orders_rejected_total",
                &[("reason", verdict.reason.as_str())],
                1.0,
            );
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RiskStateInner> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_audit(&self) -> std::sync::MutexGuard<'_, VecDeque<RiskDecision>> {
        self.audit.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Simple returns from the most recent `window + 1` prices.
fn recent_returns(prices: &VecDeque<f64>, window: usize) -> Vec<f64> {
    let take = (window.saturating_add(1)).min(prices.len());
    if take < 2 {
        return Vec::new();
    }
    let start = prices.len() - take;
    let sample: Vec<f64> = prices.iter().skip(start).copied().collect();
    sample
        .windows(2)
        .filter(|pair| pair[0] > 0.0)
        .map(|pair| pair[1] / pair[0] - 1.0)
        .collect()
}

fn stdev(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let variance = values
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>()
        / (values.len() - 1) as f64;
    variance.max(0.0).sqrt()
}

fn pearson(a: &[f64], b: &[f64]) -> Option<f64> {
    let n = a.len().min(b.len());
    if n < 2 {
        return None;
    }
    let a = &a[a.len() - n..];
    let b = &b[b.len() - n..];
    let mean_a = a.iter().sum::<f64>() / n as f64;
    let mean_b = b.iter().sum::<f64>() / n as f64;
    let mut covariance = 0.0;
    let mut var_a = 0.0;
    let mut var_b = 0.0;
    for (x, y) in a.iter().zip(b) {
        let dx = x - mean_a;
        let dy = y - mean_b;
        covariance += dx * dy;
        var_a += dx * dx;
        var_b += dy * dy;
    }
    if var_a <= f64::EPSILON || var_b <= f64::EPSILON {
        return None;
    }
    Some((covariance / (var_a.sqrt() * var_b.sqrt())).clamp(-1.0, 1.0))
}

fn pair_correlation(state: &RiskStateInner, a: &str, b: &str, window: usize) -> Option<f64> {
    if a == b {
        return Some(1.0);
    }
    let ra = recent_returns(state.prices.get(a)?, window);
    let rb = recent_returns(state.prices.get(b)?, window);
    let n = ra.len().min(rb.len()).min(window);
    if n < MIN_CORRELATION_SAMPLES {
        return None;
    }
    pearson(&ra[ra.len() - n..], &rb[rb.len() - n..])
}

/// The locked body of [`RiskEngine::correlated_exposure`].
fn correlated_exposure_locked(
    config: &RiskConfig,
    state: &RiskStateInner,
    symbol: &str,
    order_notional: f64,
) -> Option<(f64, f64, String)> {
    if config.max_correlated_exposure_pct <= 0.0 || config.correlation_threshold <= 0.0 {
        return None;
    }
    let mut total = state
        .position_notional
        .get(symbol)
        .copied()
        .unwrap_or(0.0)
        .abs();
    let mut strongest: Option<(f64, String)> = None;
    for (other, notional) in &state.position_notional {
        if other == symbol || notional.abs() <= 0.01 {
            continue;
        }
        let Some(rho) = pair_correlation(state, symbol, other, config.volatility_window) else {
            continue;
        };
        if rho.abs() < config.correlation_threshold {
            continue;
        }
        total += notional.abs();
        let stronger = strongest
            .as_ref()
            .map(|(best, _)| rho.abs() > best.abs())
            .unwrap_or(true);
        if stronger {
            strongest = Some((rho, other.clone()));
        }
    }
    let (rho, other) = strongest?;
    Some((total + order_notional, rho, other))
}

fn drawdown_pct(peak: f64, equity: f64) -> f64 {
    if peak <= 0.0 {
        return 0.0;
    }
    ((peak - equity) / peak * 100.0).max(0.0)
}

fn day_changed(start_ms: i64, now_ms: i64) -> bool {
    const DAY_MS: i64 = 86_400_000;
    now_ms / DAY_MS != start_ms / DAY_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(cfg: RiskConfig) -> (RiskEngine, Arc<Metrics>) {
        let metrics = Arc::new(Metrics::new());
        (RiskEngine::new(cfg, metrics.clone()), metrics)
    }

    fn cfg() -> RiskConfig {
        let mut c = RiskConfig::default();
        c.symbol_allowlist.insert("SOL/USDT".into());
        c.venue_allowlist.insert(Venue::Paper);
        c
    }

    fn ctx(equity: f64) -> RiskContext {
        RiskContext {
            equity_usd: equity,
            gross_exposure_usd: 0.0,
            open_positions: 0,
            current_position_qty: 0.0,
            price: 100.0,
            contracts: vec![],
        }
    }

    #[test]
    fn approves_order_within_limits() {
        let (risk, _) = engine(cfg());
        let req = OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Buy, 10.0);
        let verdict = risk.evaluate(&req, &ctx(10_000.0));
        assert!(
            verdict.approved,
            "expected approval, got {:?}",
            verdict.detail
        );
    }

    #[test]
    fn rejects_symbol_outside_allowlist() {
        let (risk, _) = engine(cfg());
        let req = OrderRequest::market(Venue::Paper, "DOGE/USDT", Side::Buy, 1.0);
        let verdict = risk.evaluate(&req, &ctx(10_000.0));
        assert!(!verdict.approved);
        assert_eq!(verdict.reason, "symbol_not_allowed");
    }

    #[test]
    fn rejects_venue_outside_allowlist() {
        let (risk, _) = engine(cfg());
        let req = OrderRequest::market(Venue::Binance, "SOL/USDT", Side::Buy, 1.0);
        let verdict = risk.evaluate(&req, &ctx(10_000.0));
        assert!(!verdict.approved);
        assert_eq!(verdict.reason, "venue_not_allowed");
    }

    #[test]
    fn rejects_contract_outside_allowlist() {
        let mut c = cfg();
        c.contract_allowlist.insert("0xallowed".into());
        let (risk, _) = engine(c);
        let mut context = ctx(10_000.0);
        context.contracts.push("0xnotallowed".into());
        let req = OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Buy, 1.0);
        let verdict = risk.evaluate(&req, &context);
        assert!(!verdict.approved);
        assert_eq!(verdict.reason, "contract_not_allowed");
    }

    #[test]
    fn rejects_non_finite_and_non_positive_quantity() {
        let (risk, _) = engine(cfg());
        for qty in [f64::NAN, f64::INFINITY, 0.0, -5.0] {
            let req = OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Buy, qty);
            let verdict = risk.evaluate(&req, &ctx(10_000.0));
            assert!(!verdict.approved);
            assert_eq!(verdict.reason, "invalid_quantity");
        }
    }

    #[test]
    fn oversized_trade_rejects_and_halts_trading() {
        let (risk, _) = engine(cfg());
        // 20% of 10k = 2000; 30 units at 100 = 3000.
        let req = OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Buy, 30.0);
        let verdict = risk.evaluate(&req, &ctx(10_000.0));
        assert!(!verdict.approved);
        assert_eq!(verdict.reason, "position_size_exceeds_max_pct");
        assert_eq!(risk.breaker_state(), BreakerState::Open);

        // Everything is halted afterwards.
        let small = OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Buy, 1.0);
        let second = risk.evaluate(&small, &ctx(10_000.0));
        assert!(!second.approved);
        assert_eq!(second.reason, "breaker_open");
    }

    #[test]
    fn drawdown_trips_the_breaker() {
        let (risk, _) = engine(cfg());
        risk.update_equity(10_000.0, now_ms());
        assert!(risk.update_equity(8_900.0, now_ms()).is_some());
        assert_eq!(risk.breaker_state(), BreakerState::Open);
    }

    #[test]
    fn daily_loss_trips_the_breaker() {
        let mut c = cfg();
        c.max_drawdown_pct = 50.0;
        c.max_daily_loss_pct = 3.0;
        let (risk, _) = engine(c);
        risk.update_equity(10_000.0, now_ms());
        let trip = risk.update_equity(9_600.0, now_ms());
        assert!(trip.is_some());
        assert_eq!(trip.unwrap().reason, "daily_loss_limit");
    }

    #[test]
    fn gross_exposure_cap_is_enforced() {
        let mut c = cfg();
        c.max_gross_exposure_pct = 25.0; // 2500 on 10k equity
        let (risk, _) = engine(c);
        let mut context = ctx(10_000.0);
        context.gross_exposure_usd = 2_000.0;
        let req = OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Buy, 10.0);
        let verdict = risk.evaluate(&req, &context);
        assert!(!verdict.approved);
        assert_eq!(verdict.reason, "gross_exposure_exceeded");
    }

    #[test]
    fn open_position_cap_is_enforced() {
        let mut c = cfg();
        c.max_open_positions = 1;
        let (risk, _) = engine(c);
        let mut context = ctx(10_000.0);
        context.open_positions = 1;
        let req = OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Buy, 1.0);
        let verdict = risk.evaluate(&req, &context);
        assert!(!verdict.approved);
        assert_eq!(verdict.reason, "max_open_positions");
    }

    #[test]
    fn reduce_only_may_not_increase_a_position() {
        let (risk, _) = engine(cfg());
        let mut context = ctx(10_000.0);
        context.current_position_qty = 1.0;
        let mut req = OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Buy, 5.0);
        req.reduce_only = true;
        let verdict = risk.evaluate(&req, &context);
        assert!(!verdict.approved);
        assert_eq!(verdict.reason, "reduce_only_violation");
    }

    fn conditional(side: Side, trigger: f64) -> OrderRequest {
        let mut req = OrderRequest::market(Venue::Paper, "SOL/USDT", side, 10.0);
        req.order_type = OrderType::Stop;
        req.trigger_price = Some(trigger);
        req.reduce_only = true;
        req
    }

    #[test]
    fn a_conditional_order_needs_a_real_trigger() {
        let (risk, _) = engine(cfg());
        let mut context = ctx(10_000.0);
        context.current_position_qty = 10.0;
        for trigger in [None, Some(f64::NAN), Some(0.0), Some(-1.0)] {
            let mut req = conditional(Side::Sell, 90.0);
            req.trigger_price = trigger;
            let verdict = risk.evaluate(&req, &context);
            assert!(!verdict.approved, "{trigger:?}");
            assert_eq!(verdict.reason, "invalid_trigger");
        }
    }

    /// A conditional order that would fire the moment it is placed is a market
    /// order wearing a trigger, and the agent should be told so.
    #[test]
    fn a_conditional_trigger_must_still_be_ahead_of_the_market() {
        let (risk, _) = engine(cfg());
        let mut context = ctx(10_000.0); // market price 100
        context.current_position_qty = 10.0;
        for (order_type, side, trigger) in [
            (OrderType::Stop, Side::Sell, 120.0),
            (OrderType::Stop, Side::Buy, 80.0),
            (OrderType::TakeProfit, Side::Sell, 80.0),
            (OrderType::TakeProfit, Side::Buy, 120.0),
        ] {
            let mut req = conditional(side, trigger);
            req.order_type = order_type;
            let verdict = risk.evaluate(&req, &context);
            assert!(!verdict.approved, "{order_type:?} {side} {trigger}");
            assert_eq!(verdict.reason, "trigger_already_crossed");
        }
    }

    #[test]
    fn a_conditional_order_may_not_open_a_position() {
        let (risk, _) = engine(cfg());
        let mut context = ctx(10_000.0);
        context.current_position_qty = 10.0;
        let mut req = conditional(Side::Sell, 90.0);
        req.reduce_only = false;
        let verdict = risk.evaluate(&req, &context);
        assert!(!verdict.approved);
        assert_eq!(verdict.reason, "conditional_not_reduce_only");
    }

    #[test]
    fn a_reduce_only_stop_ahead_of_the_market_is_approved() {
        let (risk, _) = engine(cfg());
        let mut context = ctx(10_000.0);
        context.current_position_qty = 10.0;
        let verdict = risk.evaluate(&conditional(Side::Sell, 90.0), &context);
        assert!(verdict.approved, "{}", verdict.detail);
    }

    #[test]
    fn breaker_requires_confirmation_to_reset() {
        let (risk, _) = engine(cfg());
        risk.update_equity(10_000.0, now_ms());
        risk.update_equity(8_000.0, now_ms());
        assert_eq!(risk.breaker_state(), BreakerState::Open);
        assert!(risk.reset_breaker(false).is_err());
        assert_eq!(risk.breaker_state(), BreakerState::Open);
        assert!(risk.reset_breaker(true).is_ok());
        assert_eq!(risk.breaker_state(), BreakerState::Closed);
    }

    #[tokio::test]
    async fn half_open_allows_only_a_probe_order() {
        let mut c = cfg();
        c.breaker_cooldown_ms = 50;
        let (risk, _) = engine(c);
        risk.update_equity(10_000.0, now_ms());
        risk.update_equity(8_000.0, now_ms());
        assert_eq!(risk.breaker_state(), BreakerState::Open);

        tokio::time::sleep(std::time::Duration::from_millis(80)).await;

        // The half-open probe is capped at a quarter of the position limit:
        // 20% of 10k equity is $2000, so a probe may be at most $500.
        let req = OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Buy, 2.0);
        let verdict = risk.evaluate(&req, &ctx(10_000.0));
        assert!(
            verdict.approved,
            "probe should be allowed: {}",
            verdict.detail
        );
        assert_eq!(verdict.breaker_state, BreakerState::HalfOpen);

        // A probe that never fills re-opens the breaker.
        risk.note_probe_failure("venue rejected the order");
        assert_eq!(risk.breaker_state(), BreakerState::Open);
    }

    #[test]
    fn fill_tracks_exposure_and_closes_the_breaker_after_a_probe() {
        let (risk, metrics) = engine(cfg());
        let fill = OrderFill {
            venue: Venue::Paper,
            order_id: "1".into(),
            symbol: "SOL/USDT".into(),
            side: Side::Buy,
            qty: 1.0,
            price: 100.0,
            fee: 0.1,
            ts_ms: now_ms(),
        };
        risk.on_fill(&fill, 10_000.0);
        assert_eq!(risk.open_positions(), 1);
        assert!((risk.gross_exposure() - 100.0).abs() < 1e-6);
        assert_eq!(
            metrics.counter_value("openether_risk_checks_total", &[("result", "approved")]),
            0.0
        );
    }

    #[test]
    fn decisions_are_audited() {
        let (risk, _) = engine(cfg());
        let req = OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Buy, 1.0);
        risk.evaluate(&req, &ctx(10_000.0));
        let req = OrderRequest::market(Venue::Paper, "DOGE/USDT", Side::Buy, 1.0);
        risk.evaluate(&req, &ctx(10_000.0));
        let decisions = risk.decisions(10);
        assert_eq!(decisions.len(), 2);
        assert!(decisions[0].approved);
        assert!(!decisions[1].approved);
    }

    /// Feeds `start` and then one price per percentage move.
    fn observe_series(risk: &RiskEngine, symbol: &str, start: f64, moves: &[f64]) {
        let mut price = start;
        risk.observe_price(symbol, price, 0);
        for mv in moves {
            price *= 1.0 + mv;
            risk.observe_price(symbol, price, 0);
        }
    }

    fn moves(count: usize) -> Vec<f64> {
        (0..count).map(|i| (i % 5) as f64 * 0.002 - 0.004).collect()
    }

    fn two_symbols() -> RiskConfig {
        let mut config = cfg();
        config.symbol_allowlist.insert("BTC/USDT".into());
        config.max_correlated_exposure_pct = 10.0;
        config
    }

    fn hold(risk: &RiskEngine, symbol: &str, qty: f64, price: f64) {
        let fill = OrderFill {
            venue: Venue::Paper,
            order_id: "seed".into(),
            symbol: symbol.into(),
            side: Side::Buy,
            qty,
            price,
            fee: 0.0,
            ts_ms: now_ms(),
        };
        risk.on_fill(&fill, 10_000.0);
    }

    fn ctx_with(equity: f64, gross: f64, position: f64, price: f64) -> RiskContext {
        RiskContext {
            equity_usd: equity,
            gross_exposure_usd: gross,
            open_positions: usize::from(position.abs() > 1e-9),
            current_position_qty: position,
            price,
            contracts: vec![],
        }
    }

    #[test]
    fn a_correlated_cluster_is_capped_as_one_bet() {
        let (risk, _) = engine(two_symbols());
        let history = moves(30);
        observe_series(&risk, "SOL/USDT", 100.0, &history);
        observe_series(&risk, "BTC/USDT", 100.0, &history);
        assert!(risk.correlation("SOL/USDT", "BTC/USDT").unwrap() > 0.99);
        hold(&risk, "BTC/USDT", 8.0, 100.0);

        let req = OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Buy, 9.0);
        let verdict = risk.evaluate(&req, &ctx_with(10_000.0, 800.0, 0.0, 100.0));

        assert!(!verdict.approved, "the cluster must share one budget");
        assert_eq!(verdict.reason, "correlated_exposure_exceeded");
    }

    #[test]
    fn a_reduce_only_exit_is_exempt_from_the_correlation_cap() {
        let (risk, _) = engine(two_symbols());
        let history = moves(30);
        observe_series(&risk, "SOL/USDT", 100.0, &history);
        observe_series(&risk, "BTC/USDT", 100.0, &history);
        hold(&risk, "BTC/USDT", 8.0, 100.0);

        let mut req = OrderRequest::market(Venue::Paper, "BTC/USDT", Side::Sell, 2.0);
        req.reduce_only = true;
        let verdict = risk.evaluate(&req, &ctx_with(10_000.0, 800.0, 8.0, 100.0));

        assert!(
            verdict.approved,
            "shrinking a position is never the risk: {:?}",
            verdict.detail
        );
    }

    #[test]
    fn without_enough_history_the_correlation_cap_does_not_speak() {
        let (risk, _) = engine(two_symbols());
        let short = moves(5);
        observe_series(&risk, "SOL/USDT", 100.0, &short);
        observe_series(&risk, "BTC/USDT", 100.0, &short);
        hold(&risk, "BTC/USDT", 8.0, 100.0);
        assert!(risk.correlation("SOL/USDT", "BTC/USDT").is_none());

        let req = OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Buy, 9.0);
        let verdict = risk.evaluate(&req, &ctx_with(10_000.0, 800.0, 0.0, 100.0));

        assert!(verdict.approved);
    }

    #[test]
    fn a_hot_market_is_sized_down_toward_the_volatility_target() {
        let (risk, _) = engine(cfg());
        let hot: Vec<f64> = (0..30)
            .map(|i| if i % 2 == 0 { 0.05 } else { -0.05 })
            .collect();
        observe_series(&risk, "SOL/USDT", 100.0, &hot);

        let scale = risk.volatility_scale("SOL/USDT");
        assert!(scale < 1.0, "scale was {scale}");
        assert!(scale >= 0.25, "scale was cut below the floor: {scale}");

        let (qty, applied) = risk.scale_for_volatility(Side::Buy, 10.0, 0.0, "SOL/USDT");
        assert!(qty < 10.0, "qty was {qty}");
        assert_eq!(applied, scale);
    }

    #[test]
    fn a_calm_market_keeps_full_size_and_exits_are_never_scaled() {
        let (risk, _) = engine(cfg());
        let calm: Vec<f64> = (0..30)
            .map(|i| if i % 2 == 0 { 0.001 } else { -0.001 })
            .collect();
        observe_series(&risk, "SOL/USDT", 100.0, &calm);
        assert_eq!(risk.volatility_scale("SOL/USDT"), 1.0);

        let (hot, _) = engine(cfg());
        let hot_moves: Vec<f64> = (0..30)
            .map(|i| if i % 2 == 0 { 0.05 } else { -0.05 })
            .collect();
        observe_series(&hot, "SOL/USDT", 100.0, &hot_moves);
        let (qty, applied) = hot.scale_for_volatility(Side::Sell, 4.0, 10.0, "SOL/USDT");
        assert_eq!(qty, 4.0);
        assert_eq!(applied, 1.0);
    }
}

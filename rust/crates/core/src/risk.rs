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
    probe_in_flight: bool,
}

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

    /// The single gate every order must pass.
    pub fn evaluate(&self, req: &OrderRequest, ctx: &RiskContext) -> RiskVerdict {
        let ts_ms = now_ms();
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
}

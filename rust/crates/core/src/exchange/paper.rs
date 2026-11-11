//! Paper trading venue — a deterministic market simulator used as the default
//! execution target for openEther.
//!
//! # Accounting model
//!
//! The venue uses a *USD-margined futures style* account so that the equity
//! formula used by the rest of the core (`equity = cash + unrealized_pnl`) is
//! literally true:
//!
//! * `cash` is the wallet balance. It moves when fees are charged and when a
//!   position is closed (realised PnL is settled into cash).
//! * Open positions lock margin: `margin = |qty| * avg_entry`. The margin is
//!   *locked*, not spent, so `free_cash = cash - locked_margin`.
//! * `equity = cash + Σ unrealized_pnl`, where `unrealized_pnl` is
//!   `(mark - avg_entry) * signed_qty`.
//!
//! # Price process
//!
//! Prices follow a mean-reverting random walk around a per-symbol base price.
//! The walk is driven by a single [`rand::rngs::StdRng`] seeded with
//! `OPENETHER_PAPER_SEED` (default `20260101`), so a given run is reproducible.
//! Symbols are always visited in sorted order which keeps the sequence
//! deterministic regardless of hash iteration order.
//!
//! # Fees and slippage
//!
//! * Taker fee: `OPENETHER_PAPER_TAKER_FEE_BPS` (default `5` bps) of the fill
//!   notional.
//! * Market slippage: `OPENETHER_PAPER_SLIPPAGE_BPS` (default `2` bps) applied
//!   against the trader (buys fill higher, sells fill lower).
//! * Resting limit orders fill at their limit price (conservative) when the
//!   simulated market price crosses them; the mark price then follows the fill.

use crate::exchange::{ExchangeConnector, ExchangeError, ExchangeResult, OpenOrder, TickSink};
use crate::types::*;
use async_trait::async_trait;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const DEFAULT_SEED: u64 = 20260101;
pub const DEFAULT_SLIPPAGE_BPS: f64 = 2.0;
pub const DEFAULT_TAKER_FEE_BPS: f64 = 5.0;
pub const DEFAULT_STARTING_CASH: f64 = 10_000.0;
pub const DEFAULT_TICK_INTERVAL_MS: u64 = 500;
pub const DEFAULT_VOL_BPS: f64 = 10.0;
/// Pull back towards the base price by this fraction on every tick.
pub const DEFAULT_MEAN_REVERSION: f64 = 0.02;
/// Upper bound on the retained trade history.
const MAX_TRADES: usize = 20_000;

/// Base prices of the symbols the simulator knows about.
pub fn default_base_prices() -> BTreeMap<String, f64> {
    let mut m = BTreeMap::new();
    m.insert("SOL/USDT".to_string(), 142.0);
    m.insert("BTC/USDT".to_string(), 62_000.0);
    m.insert("ETH/USDT".to_string(), 3_300.0);
    m
}

/// Base price for a symbol; unknown symbols are quoted at `1.0`.
pub fn base_price_for(symbol: &str, base_prices: &BTreeMap<String, f64>) -> f64 {
    base_prices.get(symbol).copied().unwrap_or_else(|| {
        base_prices
            .get(&symbol.to_ascii_uppercase())
            .copied()
            .unwrap_or(1.0)
    })
}

/// Tunables of the paper venue.
#[derive(Debug, Clone)]
pub struct PaperConfig {
    /// Seed of the deterministic price walk.
    pub seed: u64,
    /// Starting wallet balance.
    pub starting_cash: f64,
    /// Market order slippage in basis points.
    pub slippage_bps: f64,
    /// Taker fee in basis points of the fill notional.
    pub taker_fee_bps: f64,
    /// How often the simulated market ticks.
    pub tick_interval: Duration,
    /// Per-tick volatility in basis points.
    pub vol_bps: f64,
    /// Mean reversion strength towards the base price.
    pub mean_reversion: f64,
    /// Base prices per symbol.
    pub base_prices: BTreeMap<String, f64>,
}

impl Default for PaperConfig {
    fn default() -> Self {
        Self {
            seed: DEFAULT_SEED,
            starting_cash: DEFAULT_STARTING_CASH,
            slippage_bps: DEFAULT_SLIPPAGE_BPS,
            taker_fee_bps: DEFAULT_TAKER_FEE_BPS,
            tick_interval: Duration::from_millis(DEFAULT_TICK_INTERVAL_MS),
            vol_bps: DEFAULT_VOL_BPS,
            mean_reversion: DEFAULT_MEAN_REVERSION,
            base_prices: default_base_prices(),
        }
    }
}

impl PaperConfig {
    /// Build the configuration from the environment.
    ///
    /// | variable | default | meaning |
    /// | --- | --- | --- |
    /// | `OPENETHER_PAPER_SEED` | `20260101` | RNG seed of the price walk |
    /// | `OPENETHER_PAPER_CASH` | `10000` | starting wallet balance |
    /// | `OPENETHER_PAPER_SLIPPAGE_BPS` | `2` | market order slippage |
    /// | `OPENETHER_PAPER_TAKER_FEE_BPS` | `5` | taker fee |
    /// | `OPENETHER_PAPER_TICK_MS` | `500` | tick interval |
    /// | `OPENETHER_PAPER_VOL_BPS` | `10` | per-tick volatility |
    pub fn from_env() -> Self {
        Self {
            seed: env_u64("OPENETHER_PAPER_SEED", DEFAULT_SEED),
            starting_cash: env_f64("OPENETHER_PAPER_CASH", DEFAULT_STARTING_CASH),
            slippage_bps: env_f64("OPENETHER_PAPER_SLIPPAGE_BPS", DEFAULT_SLIPPAGE_BPS),
            taker_fee_bps: env_f64("OPENETHER_PAPER_TAKER_FEE_BPS", DEFAULT_TAKER_FEE_BPS),
            tick_interval: Duration::from_millis(env_u64(
                "OPENETHER_PAPER_TICK_MS",
                DEFAULT_TICK_INTERVAL_MS,
            )),
            vol_bps: env_f64("OPENETHER_PAPER_VOL_BPS", DEFAULT_VOL_BPS),
            ..Self::default()
        }
    }
}

fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .unwrap_or(default)
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

#[derive(Debug, Clone)]
struct PaperPosition {
    /// Signed size: positive = long, negative = short.
    qty: f64,
    avg_entry: f64,
}

#[derive(Debug, Clone)]
struct PaperOrder {
    order_id: String,
    client_id: String,
    symbol: String,
    side: Side,
    qty: f64,
    price: f64,
    reduce_only: bool,
    rationale: String,
    created_ts_ms: i64,
}

#[derive(Debug)]
struct PaperState {
    rng: StdRng,
    cash: f64,
    realized_pnl: f64,
    fees_paid: f64,
    positions: BTreeMap<String, PaperPosition>,
    open_orders: Vec<PaperOrder>,
    trades: Vec<TradeRecord>,
    /// Fills in the venue's own words, so the engine can book the real price,
    /// fee and slippage instead of synthesising them from the pre-order mark.
    fills: Vec<OrderFill>,
    prices: BTreeMap<String, f64>,
    seq: u64,
}

impl PaperState {
    fn position_qty(&self, symbol: &str) -> f64 {
        self.positions.get(symbol).map(|p| p.qty).unwrap_or(0.0)
    }

    /// Margin currently locked by open positions.
    fn locked_margin(&self) -> f64 {
        self.positions
            .values()
            .map(|p| p.qty.abs() * p.avg_entry)
            .sum::<f64>()
    }

    fn mark_price(&self, symbol: &str) -> f64 {
        self.prices
            .get(symbol)
            .copied()
            .or_else(|| self.positions.get(symbol).map(|p| p.avg_entry))
            .unwrap_or(0.0)
    }

    fn unrealized_pnl(&self) -> f64 {
        self.positions
            .iter()
            .map(|(sym, p)| (self.mark_price(sym) - p.avg_entry) * p.qty)
            .sum()
    }

    fn next_id(&mut self, prefix: &str) -> String {
        self.seq += 1;
        format!(
            "{}-{:08}-{}",
            prefix,
            self.seq,
            uuid::Uuid::new_v4().simple()
        )
    }
}

#[derive(Debug)]
struct PaperInner {
    cfg: PaperConfig,
    state: Mutex<PaperState>,
    /// Only one background task is allowed to advance the price walk, otherwise
    /// the deterministic sequence would depend on the number of subscribers.
    ticker_owner: AtomicBool,
}

/// Simulated exchange venue.
///
/// All state lives behind a [`std::sync::Mutex`], so every accessor works on
/// `&self` and satisfies the [`ExchangeConnector`] trait.
#[derive(Debug, Clone)]
pub struct PaperConnector {
    inner: Arc<PaperInner>,
}

impl PaperConnector {
    /// Build a connector from the environment (see [`PaperConfig::from_env`]).
    pub fn new() -> Self {
        Self::with_config(PaperConfig::from_env())
    }

    /// Build a connector with an explicit configuration.
    pub fn with_config(cfg: PaperConfig) -> Self {
        let mut prices = BTreeMap::new();
        for (sym, px) in &cfg.base_prices {
            prices.insert(sym.clone(), *px);
        }
        let state = PaperState {
            rng: StdRng::seed_from_u64(cfg.seed),
            cash: cfg.starting_cash,
            realized_pnl: 0.0,
            fees_paid: 0.0,
            positions: BTreeMap::new(),
            open_orders: Vec::new(),
            trades: Vec::new(),
            fills: Vec::new(),
            prices,
            seq: 0,
        };
        Self {
            inner: Arc::new(PaperInner {
                cfg,
                state: Mutex::new(state),
                ticker_owner: AtomicBool::new(false),
            }),
        }
    }

    /// Convenience constructor with a custom starting balance.
    pub fn with_cash(cash: f64) -> Self {
        let mut cfg = PaperConfig::from_env();
        cfg.starting_cash = cash;
        Self::with_config(cfg)
    }

    pub fn config(&self) -> &PaperConfig {
        &self.inner.cfg
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PaperState> {
        // A poisoned mutex must not take the whole process down: the state is
        // still structurally valid, so recover instead of panicking.
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Register a symbol, seeding its price from the base price table.
    fn register(state: &mut PaperState, cfg: &PaperConfig, symbol: &str) {
        if !state.prices.contains_key(symbol) {
            let px = base_price_for(symbol, &cfg.base_prices);
            state.prices.insert(symbol.to_string(), px);
        }
    }

    /// Set the price of a symbol and fill any crossed limit orders.
    fn apply_price(
        state: &mut PaperState,
        cfg: &PaperConfig,
        symbol: &str,
        new_price: f64,
    ) -> MarketTick {
        let new_price = if new_price.is_finite() && new_price > 0.0 {
            new_price
        } else {
            state.mark_price(symbol)
        };

        // Fill resting limit orders that the new price crosses.
        let crossed: Vec<usize> = state
            .open_orders
            .iter()
            .enumerate()
            .filter(|(_, o)| o.symbol == symbol)
            .filter(|(_, o)| match o.side {
                Side::Buy => new_price <= o.price,
                Side::Sell => new_price >= o.price,
            })
            .map(|(i, _)| i)
            .collect();

        // The mark follows the last trade: a filled limit order becomes the
        // new mark price (the fill happened at the limit price).
        let mut mark = new_price;
        for idx in crossed {
            let Some(order) = state.open_orders.get(idx).cloned() else {
                continue;
            };
            state.open_orders.remove(idx);
            match execute_fill(
                state,
                cfg,
                Fill {
                    symbol: &order.symbol,
                    side: order.side,
                    qty: order.qty,
                    price: order.price,
                    reduce_only: order.reduce_only,
                    rationale: &order.rationale,
                    order_id: &order.order_id,
                },
            ) {
                Ok(trade) => {
                    mark = order.price;
                    tracing::info!(
                        venue = "paper",
                        symbol = %trade.symbol,
                        side = %trade.side,
                        qty = trade.qty,
                        price = trade.price,
                        client_id = %order.client_id,
                        "limit order filled"
                    );
                    push_trade(state, trade, &order.order_id);
                }
                Err(err) => {
                    tracing::warn!(
                        venue = "paper",
                        symbol = %order.symbol,
                        order_id = %order.order_id,
                        error = %err,
                        "resting limit order could not be filled and was dropped"
                    );
                }
            }
        }
        state.prices.insert(symbol.to_string(), mark);

        MarketTick {
            venue: Venue::Paper,
            symbol: symbol.to_string(),
            price: mark,
            size: 0.0,
            ts_ms: now_ms(),
        }
    }

    /// Advance every known symbol by one random walk step.
    pub fn tick_once(&self) -> Vec<MarketTick> {
        let mut state = self.lock();
        let cfg = &self.inner.cfg;
        advance_prices(&mut state, cfg)
    }

    /// Force the price of a symbol (manual control / tests). Crossed limit
    /// orders are filled exactly like on a regular tick.
    pub fn set_price(&self, symbol: &str, price: f64) -> MarketTick {
        let mut state = self.lock();
        let cfg = &self.inner.cfg;
        Self::register(&mut state, cfg, symbol);
        Self::apply_price(&mut state, cfg, symbol, price)
    }

    /// Current simulated price of a symbol.
    pub fn price_of(&self, symbol: &str) -> f64 {
        let state = self.lock();
        let cfg = &self.inner.cfg;
        match state.prices.get(symbol) {
            Some(p) => *p,
            None => base_price_for(symbol, &cfg.base_prices),
        }
    }

    /// Wallet balance (fees and settled PnL only).
    pub fn cash(&self) -> f64 {
        self.lock().cash
    }

    /// Cash available for new positions (wallet balance minus locked margin).
    pub fn free_cash(&self) -> f64 {
        let state = self.lock();
        state.cash - state.locked_margin()
    }

    /// Total fees charged so far.
    pub fn fees_paid(&self) -> f64 {
        self.lock().fees_paid
    }

    /// Realised PnL of closed position portions.
    pub fn realized_pnl(&self) -> f64 {
        self.lock().realized_pnl
    }

    /// Mark-to-market PnL of the open positions.
    pub fn unrealized_pnl(&self) -> f64 {
        self.lock().unrealized_pnl()
    }

    /// Equity = wallet balance + unrealised PnL.
    pub fn equity(&self) -> f64 {
        let state = self.lock();
        state.cash + state.unrealized_pnl()
    }

    /// Open positions with mark prices.
    pub fn positions(&self) -> Vec<Position> {
        let state = self.lock();
        let ts = now_ms();
        state
            .positions
            .iter()
            .map(|(sym, p)| {
                let mark = state.mark_price(sym);
                Position {
                    venue: Venue::Paper,
                    symbol: sym.clone(),
                    qty: p.qty,
                    avg_entry: p.avg_entry,
                    mark_price: mark,
                    unrealized_pnl: (mark - p.avg_entry) * p.qty,
                    notional_usd: p.qty.abs() * mark,
                    ts_ms: ts,
                }
            })
            .collect()
    }

    /// Executed trades, oldest first.
    pub fn trades(&self) -> Vec<TradeRecord> {
        self.lock().trades.clone()
    }

    /// Resting limit orders.
    pub fn resting_orders(&self) -> Vec<OpenOrder> {
        let state = self.lock();
        state
            .open_orders
            .iter()
            .map(|o| OpenOrder {
                venue: Venue::Paper,
                order_id: o.order_id.clone(),
                symbol: o.symbol.clone(),
                side: o.side,
                qty: o.qty,
                price: o.price,
                ts_ms: o.created_ts_ms,
            })
            .collect()
    }

    /// Snapshot in the shape used by the HTTP API.
    pub fn portfolio_snapshot(&self) -> PortfolioSnapshot {
        let state = self.lock();
        let ts = now_ms();
        let unrealized = state.unrealized_pnl();
        let equity = state.cash + unrealized;
        let positions: Vec<Position> = state
            .positions
            .iter()
            .map(|(sym, p)| {
                let mark = state.mark_price(sym);
                Position {
                    venue: Venue::Paper,
                    symbol: sym.clone(),
                    qty: p.qty,
                    avg_entry: p.avg_entry,
                    mark_price: mark,
                    unrealized_pnl: (mark - p.avg_entry) * p.qty,
                    notional_usd: p.qty.abs() * mark,
                    ts_ms: ts,
                }
            })
            .collect();
        let starting = self.inner.cfg.starting_cash;
        PortfolioSnapshot {
            venue: Venue::Paper,
            equity_usd: equity,
            cash_usd: state.cash,
            unrealized_pnl_usd: unrealized,
            realized_pnl_usd: state.realized_pnl,
            total_pnl_pct: if starting > 0.0 {
                (equity - starting) / starting
            } else {
                0.0
            },
            positions,
            ts_ms: ts,
        }
    }
}

impl Default for PaperConnector {
    fn default() -> Self {
        Self::new()
    }
}

/// One step of the mean-reverting random walk for every known symbol.
fn advance_prices(state: &mut PaperState, cfg: &PaperConfig) -> Vec<MarketTick> {
    let symbols: Vec<String> = state.prices.keys().cloned().collect();
    let mut ticks = Vec::with_capacity(symbols.len());
    for symbol in symbols {
        let base = base_price_for(&symbol, &cfg.base_prices);
        let current = state.mark_price(&symbol);
        let shock: f64 = state.rng.random_range(-cfg.vol_bps..=cfg.vol_bps) / 10_000.0;
        // The gap to the base price is in dollars, so mean reversion adds a
        // fraction of that gap directly. Only the shock is a return on price.
        let next = current + (base - current) * cfg.mean_reversion + current * shock;
        let mut tick = PaperConnector::apply_price(state, cfg, &symbol, next);
        tick.size = state.rng.random_range(0.01..=2.0);
        ticks.push(tick);
    }
    ticks
}

fn push_trade(state: &mut PaperState, trade: TradeRecord, order_id: &str) {
    state.fills.push(OrderFill {
        venue: trade.venue,
        order_id: order_id.to_string(),
        symbol: trade.symbol.clone(),
        side: trade.side,
        qty: trade.qty,
        price: trade.price,
        fee: trade.fee,
        ts_ms: trade.ts_ms,
    });
    state.trades.push(trade);
    // Both histories grow together, so one excess count trims them in lockstep.
    if state.trades.len() > MAX_TRADES {
        let excess = state.trades.len() - MAX_TRADES;
        state.trades.drain(0..excess);
        state.fills.drain(0..excess);
    }
}

/// A single fill that has to be booked against the account.
#[derive(Debug, Clone, Copy)]
struct Fill<'a> {
    symbol: &'a str,
    side: Side,
    qty: f64,
    price: f64,
    reduce_only: bool,
    rationale: &'a str,
    order_id: &'a str,
}

/// Execute a fill against the account.
///
/// Handles position bookkeeping (signed quantity, average entry, realised
/// PnL), the taker fee and the margin check that rejects orders the account
/// cannot afford.
fn execute_fill(
    state: &mut PaperState,
    cfg: &PaperConfig,
    fill: Fill<'_>,
) -> ExchangeResult<TradeRecord> {
    let Fill {
        symbol,
        side,
        qty,
        price,
        reduce_only,
        rationale,
        order_id,
    } = fill;

    if !qty.is_finite() || qty <= 0.0 {
        return Err(ExchangeError::InvalidRequest(format!(
            "order quantity must be a positive finite number (got {qty})"
        )));
    }
    if !price.is_finite() || price <= 0.0 {
        return Err(ExchangeError::InvalidRequest(format!(
            "fill price must be a positive finite number (got {price})"
        )));
    }

    let pos_qty = state.position_qty(symbol);

    // Reduce-only orders may never increase the position.
    let mut qty = qty;
    if reduce_only {
        match side {
            Side::Sell => {
                if pos_qty <= 0.0 {
                    return Err(ExchangeError::InvalidRequest(
                        "reduce-only sell requires an open long position".into(),
                    ));
                }
                if qty > pos_qty {
                    tracing::warn!(
                        venue = "paper",
                        symbol = %symbol,
                        order_id = %order_id,
                        requested = qty,
                        allowed = pos_qty,
                        "reduce-only order clamped to the open position size"
                    );
                    qty = pos_qty;
                }
            }
            Side::Buy => {
                if pos_qty >= 0.0 {
                    return Err(ExchangeError::InvalidRequest(
                        "reduce-only buy requires an open short position".into(),
                    ));
                }
                if qty > -pos_qty {
                    tracing::warn!(
                        venue = "paper",
                        symbol = %symbol,
                        order_id = %order_id,
                        requested = qty,
                        allowed = -pos_qty,
                        "reduce-only order clamped to the open position size"
                    );
                    qty = -pos_qty;
                }
            }
        }
    }

    let signed = side.sign() * qty;
    let old_qty = pos_qty;
    let old_avg = state
        .positions
        .get(symbol)
        .map(|p| p.avg_entry)
        .unwrap_or(0.0);

    // Realised PnL of the portion that closes an existing position.
    let mut realized = 0.0;
    if old_qty != 0.0 && (old_qty > 0.0) != (signed > 0.0) {
        let closing = old_qty.abs().min(qty);
        realized = closing * (price - old_avg) * old_qty.signum();
    }

    // New average entry price.
    let new_avg = if old_qty == 0.0 {
        price
    } else if (old_qty > 0.0) == (signed > 0.0) {
        // Increasing the position: weighted average.
        (old_qty.abs() * old_avg + qty * price) / (old_qty.abs() + qty)
    } else if qty > old_qty.abs() {
        // Flipped through zero.
        price
    } else {
        // Partial close, the remaining position keeps its entry.
        old_avg
    };

    let new_qty = old_qty + signed;
    let old_margin = old_qty.abs() * old_avg;
    let new_margin = new_qty.abs() * new_avg;
    let margin_delta = new_margin - old_margin;

    if margin_delta > 0.0 {
        let available = state.cash - state.locked_margin();
        if margin_delta > available {
            return Err(ExchangeError::InsufficientBalance {
                asset: "USDT".to_string(),
                needed: margin_delta,
                available,
            });
        }
    }

    let notional = qty * price;
    let fee = notional * cfg.taker_fee_bps / 10_000.0;

    state.cash -= fee;
    state.fees_paid += fee;
    state.cash += realized;
    state.realized_pnl += realized;

    if new_qty.abs() < f64::EPSILON {
        state.positions.remove(symbol);
    } else {
        state.positions.insert(
            symbol.to_string(),
            PaperPosition {
                qty: new_qty,
                avg_entry: new_avg,
            },
        );
    }

    Ok(TradeRecord {
        id: format!("paper-trade-{}", uuid::Uuid::new_v4().simple()),
        venue: Venue::Paper,
        symbol: symbol.to_string(),
        side,
        qty,
        price,
        fee,
        realized_pnl: realized,
        ts_ms: now_ms(),
        rationale: rationale.to_string(),
        source: "strategy".to_string(),
    })
}

#[async_trait]
impl ExchangeConnector for PaperConnector {
    fn venue(&self) -> Venue {
        Venue::Paper
    }

    async fn health(&self) -> ExchangeResult<()> {
        Ok(())
    }

    async fn account(&self) -> ExchangeResult<AccountSnapshot> {
        let state = self.lock();
        let unrealized = state.unrealized_pnl();
        let equity = state.cash + unrealized;
        let locked = state.locked_margin();
        let free = state.cash - locked;
        Ok(AccountSnapshot {
            venue: Venue::Paper,
            equity_usd: equity,
            free_usd: free,
            balances: vec![Balance {
                asset: "USDT".to_string(),
                free,
                locked,
            }],
            ts_ms: now_ms(),
        })
    }

    async fn fills_since(&self, since_ms: i64) -> ExchangeResult<Vec<OrderFill>> {
        Ok(self
            .lock()
            .fills
            .iter()
            .filter(|fill| fill.ts_ms >= since_ms)
            .cloned()
            .collect())
    }

    async fn place_order(&self, req: &OrderRequest) -> ExchangeResult<OrderAck> {
        if req.venue != Venue::Paper {
            tracing::warn!(
                venue = "paper",
                requested_venue = %req.venue,
                "order routed to the paper venue carries a different venue tag"
            );
        }
        if !req.qty.is_finite() || req.qty <= 0.0 {
            return Err(ExchangeError::InvalidRequest(format!(
                "order quantity must be a positive finite number (got {})",
                req.qty
            )));
        }

        let mut state = self.lock();
        let cfg = &self.inner.cfg;
        Self::register(&mut state, cfg, &req.symbol);

        match req.order_type {
            OrderType::Market => {
                let reference = state.mark_price(&req.symbol);
                let fill_price = reference * (1.0 + req.side.sign() * cfg.slippage_bps / 10_000.0);
                let order_id = state.next_id("paper");
                let trade = execute_fill(
                    &mut state,
                    cfg,
                    Fill {
                        symbol: &req.symbol,
                        side: req.side,
                        qty: req.qty,
                        price: fill_price,
                        reduce_only: req.reduce_only,
                        rationale: &req.rationale,
                        order_id: &order_id,
                    },
                )?;
                // The trade happened at the fill price: the mark follows the
                // last trade so that the slippage shows up as a cost instead of
                // an instant mark-to-market loss.
                state.prices.insert(req.symbol.clone(), fill_price);
                push_trade(&mut state, trade.clone(), &order_id);
                Ok(OrderAck {
                    venue: Venue::Paper,
                    order_id,
                    client_id: req.client_id.clone(),
                    status: OrderStatus::Filled,
                    accepted_ts_ms: now_ms(),
                    message: format!(
                        "filled {} {} @ {} (fee {:.6})",
                        trade.qty, trade.symbol, trade.price, trade.fee
                    ),
                })
            }
            OrderType::Limit => {
                let Some(limit_price) = req.limit_price else {
                    return Err(ExchangeError::InvalidRequest(
                        "limit orders require a limit price".into(),
                    ));
                };
                if !limit_price.is_finite() || limit_price <= 0.0 {
                    return Err(ExchangeError::InvalidRequest(format!(
                        "limit price must be a positive finite number (got {limit_price})"
                    )));
                }

                // Validate the reduce-only semantics and the margin
                // requirement up front; both are re-checked at fill time.
                let pos_qty = state.position_qty(&req.symbol);
                let mut qty = req.qty;
                if req.reduce_only {
                    match req.side {
                        Side::Sell => {
                            if pos_qty <= 0.0 {
                                return Err(ExchangeError::InvalidRequest(
                                    "reduce-only sell requires an open long position".into(),
                                ));
                            }
                            qty = qty.min(pos_qty);
                        }
                        Side::Buy => {
                            if pos_qty >= 0.0 {
                                return Err(ExchangeError::InvalidRequest(
                                    "reduce-only buy requires an open short position".into(),
                                ));
                            }
                            qty = qty.min(-pos_qty);
                        }
                    }
                }

                let available = state.cash - state.locked_margin();
                let required = qty * limit_price;
                if required > available {
                    return Err(ExchangeError::InsufficientBalance {
                        asset: "USDT".to_string(),
                        needed: required,
                        available,
                    });
                }

                let order_id = state.next_id("paper");
                state.open_orders.push(PaperOrder {
                    order_id: order_id.clone(),
                    client_id: req.client_id.clone(),
                    symbol: req.symbol.clone(),
                    side: req.side,
                    qty,
                    price: limit_price,
                    reduce_only: req.reduce_only,
                    rationale: req.rationale.clone(),
                    created_ts_ms: now_ms(),
                });
                Ok(OrderAck {
                    venue: Venue::Paper,
                    order_id,
                    client_id: req.client_id.clone(),
                    status: OrderStatus::Accepted,
                    accepted_ts_ms: now_ms(),
                    message: format!(
                        "resting limit order for {} {} @ {}",
                        qty, req.symbol, limit_price
                    ),
                })
            }
        }
    }

    async fn cancel_order(&self, order_id: &str) -> ExchangeResult<()> {
        let mut state = self.lock();
        match state
            .open_orders
            .iter()
            .position(|o| o.order_id == order_id)
        {
            Some(idx) => {
                state.open_orders.remove(idx);
                Ok(())
            }
            None => Err(ExchangeError::InvalidRequest(format!(
                "unknown paper order id: {order_id}"
            ))),
        }
    }

    async fn open_orders(&self) -> ExchangeResult<Vec<OpenOrder>> {
        Ok(self.resting_orders())
    }

    async fn positions(&self) -> ExchangeResult<Vec<Position>> {
        Ok(PaperConnector::positions(self))
    }

    async fn subscribe_trades(&self, symbols: &[String], sink: TickSink) -> ExchangeResult<()> {
        {
            let mut state = self.lock();
            let cfg = &self.inner.cfg;
            for symbol in symbols {
                Self::register(&mut state, cfg, symbol);
            }
        }

        let wanted: Vec<String> = symbols.to_vec();
        let inner = self.inner.clone();
        let owner = inner
            .ticker_owner
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(inner.cfg.tick_interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let ticks = if owner {
                    let mut state = inner
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let cfg = &inner.cfg;
                    advance_prices(&mut state, cfg)
                } else {
                    // Another task already owns the price walk: mirror the
                    // current prices instead of advancing the RNG twice.
                    let state = inner
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    state
                        .prices
                        .iter()
                        .map(|(sym, px)| MarketTick {
                            venue: Venue::Paper,
                            symbol: sym.clone(),
                            price: *px,
                            size: 0.0,
                            ts_ms: now_ms(),
                        })
                        .collect()
                };

                for tick in ticks {
                    if !wanted.is_empty() && !wanted.iter().any(|w| w == &tick.symbol) {
                        continue;
                    }
                    if sink.send(tick).is_err() {
                        tracing::debug!(
                            venue = "paper",
                            "tick sink closed, stopping the simulator"
                        );
                        return;
                    }
                }
            }
        });

        Ok(())
    }

    async fn last_price(&self, symbol: &str) -> ExchangeResult<f64> {
        Ok(self.price_of(symbol))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connector() -> PaperConnector {
        let cfg = PaperConfig {
            seed: 42,
            starting_cash: 10_000.0,
            slippage_bps: 2.0,
            taker_fee_bps: 5.0,
            vol_bps: 0.0, // deterministic prices in the unit tests
            ..PaperConfig::default()
        };
        PaperConnector::with_config(cfg)
    }

    fn market(symbol: &str, side: Side, qty: f64) -> OrderRequest {
        OrderRequest::market(Venue::Paper, symbol, side, qty)
    }

    #[tokio::test]
    async fn market_order_fills_with_slippage_and_fee() {
        let c = connector();
        let ack = c
            .place_order(&market("SOL/USDT", Side::Buy, 2.0))
            .await
            .expect("order should fill");
        assert_eq!(ack.status, OrderStatus::Filled);
        assert_eq!(ack.venue, Venue::Paper);

        let trades = c.trades();
        assert_eq!(trades.len(), 1);
        let trade = &trades[0];
        // Buy: 2 bps of slippage against the trader.
        let expected_price = 142.0 * 1.0002;
        assert!(
            (trade.price - expected_price).abs() < 1e-9,
            "{}",
            trade.price
        );
        let expected_fee = expected_price * 2.0 * 5.0 / 10_000.0;
        assert!((trade.fee - expected_fee).abs() < 1e-9, "{}", trade.fee);

        // Cash: only the fee and the settled PnL move the wallet.
        assert!(
            (c.cash() - (10_000.0 - expected_fee)).abs() < 1e-9,
            "{}",
            c.cash()
        );
        // Position is long 2 @ the fill price.
        let positions = c.positions();
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].symbol, "SOL/USDT");
        assert!((positions[0].qty - 2.0).abs() < 1e-9);
        assert!((positions[0].avg_entry - expected_price).abs() < 1e-9);
        // Equity is unchanged by the fill apart from the fee.
        assert!((c.equity() - (10_000.0 - expected_fee)).abs() < 1e-9);
    }

    /// The engine books orders from `fills_since`. When it returned nothing the
    /// engine fell back to a synthetic fill at the pre-order mark with a zero
    /// fee, so the trade history disagreed with the venue's own account.
    #[tokio::test]
    async fn fills_since_reports_the_real_fill() {
        let c = connector();
        assert!(c.fills_since(0).await.expect("fills").is_empty());

        let ack = c
            .place_order(&market("SOL/USDT", Side::Buy, 2.0))
            .await
            .expect("order should fill");

        let fills = c.fills_since(0).await.expect("fills");
        assert_eq!(fills.len(), 1);
        let fill = &fills[0];
        assert_eq!(fill.order_id, ack.order_id);
        assert_eq!(fill.symbol, "SOL/USDT");
        assert_eq!(fill.side, Side::Buy);
        assert!((fill.qty - 2.0).abs() < 1e-9);

        let expected_price = 142.0 * 1.0002;
        let expected_fee = expected_price * 2.0 * 5.0 / 10_000.0;
        assert!((fill.price - expected_price).abs() < 1e-9, "{}", fill.price);
        assert!(fill.fee > 0.0, "the venue fee must reach the engine");
        assert!((fill.fee - expected_fee).abs() < 1e-9, "{}", fill.fee);

        // The window is inclusive of the fill timestamp and excludes later ones.
        assert_eq!(c.fills_since(fill.ts_ms).await.expect("fills").len(), 1);
        assert!(c
            .fills_since(fill.ts_ms + 1)
            .await
            .expect("fills")
            .is_empty());
    }

    #[tokio::test]
    async fn sell_market_order_applies_adverse_slippage() {
        let c = connector();
        c.place_order(&market("SOL/USDT", Side::Buy, 1.0))
            .await
            .unwrap();
        c.place_order(&market("SOL/USDT", Side::Sell, 1.0))
            .await
            .unwrap();
        let trades = c.trades();
        let sell = &trades[1];
        let expected_price = 142.0 * (1.0 + 2.0 / 10_000.0) * (1.0 - 2.0 / 10_000.0);
        assert!((sell.price - expected_price).abs() < 1e-9, "{}", sell.price);
    }

    #[tokio::test]
    async fn fees_are_charged_on_both_sides() {
        let c = connector();
        c.place_order(&market("SOL/USDT", Side::Buy, 1.0))
            .await
            .unwrap();
        c.place_order(&market("SOL/USDT", Side::Sell, 1.0))
            .await
            .unwrap();
        let trades = c.trades();
        let total: f64 = trades.iter().map(|t| t.fee).sum();
        assert!((c.fees_paid() - total).abs() < 1e-12);
        assert!(total > 0.0);
        // A round trip at the same price loses the fees plus the exit
        // slippage (the market moved to the fill price of the entry).
        let entry = trades[0].price;
        let exit = trades[1].price;
        assert!((c.realized_pnl() - (exit - entry)).abs() < 1e-9);
        assert!(
            c.realized_pnl() < 0.0,
            "the round trip must not be profitable"
        );
        assert!((c.cash() - (10_000.0 - total + c.realized_pnl())).abs() < 1e-9);
        assert!((c.unrealized_pnl() - 0.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn order_larger_than_available_cash_is_rejected() {
        let c = connector();
        let err = c
            .place_order(&market("SOL/USDT", Side::Buy, 1_000.0))
            .await
            .expect_err("order must be rejected");
        match err {
            ExchangeError::InsufficientBalance {
                asset,
                needed,
                available,
            } => {
                assert_eq!(asset, "USDT");
                assert!(needed > available);
                assert!((available - 10_000.0).abs() < 1e-9);
            }
            other => panic!("unexpected error: {other}"),
        }
        // Nothing was executed.
        assert!(c.trades().is_empty());
        assert!((c.cash() - 10_000.0).abs() < 1e-12);
    }

    #[tokio::test]
    async fn reduce_only_never_increases_the_position() {
        let c = connector();
        c.place_order(&market("SOL/USDT", Side::Buy, 1.0))
            .await
            .unwrap();

        // A reduce-only buy would increase the long: rejected.
        let mut buy = market("SOL/USDT", Side::Buy, 1.0);
        buy.reduce_only = true;
        let err = c.place_order(&buy).await.expect_err("must reject");
        assert!(matches!(err, ExchangeError::InvalidRequest(_)), "{err}");

        // A reduce-only sell larger than the position is clamped.
        let mut sell = market("SOL/USDT", Side::Sell, 5.0);
        sell.reduce_only = true;
        c.place_order(&sell).await.expect("clamped sell fills");
        let positions = c.positions();
        assert!(
            positions.is_empty(),
            "position should be flat: {positions:?}"
        );
        let trades = c.trades();
        assert_eq!(trades.len(), 2);
        assert!((trades[1].qty - 1.0).abs() < 1e-9, "clamped quantity");
    }

    #[tokio::test]
    async fn reduce_only_sell_without_position_is_rejected() {
        let c = connector();
        let mut sell = market("SOL/USDT", Side::Sell, 1.0);
        sell.reduce_only = true;
        let err = c.place_order(&sell).await.expect_err("must reject");
        assert!(matches!(err, ExchangeError::InvalidRequest(_)), "{err}");
        assert!(c.trades().is_empty());
    }

    #[tokio::test]
    async fn realized_pnl_on_a_round_trip() {
        let c = connector();
        c.place_order(&market("SOL/USDT", Side::Buy, 1.0))
            .await
            .unwrap();
        let entry = c.positions()[0].avg_entry;

        // Market moves up by 10.
        c.set_price("SOL/USDT", 152.0);
        let unrealized = c.unrealized_pnl();
        assert!((unrealized - (152.0 - entry)).abs() < 1e-9, "{unrealized}");
        assert!((c.equity() - (c.cash() + unrealized)).abs() < 1e-9);

        c.place_order(&market("SOL/USDT", Side::Sell, 1.0))
            .await
            .unwrap();
        let realized = c.realized_pnl();
        // Exit at 152 minus 2 bps of slippage.
        let exit_price = 152.0 * (1.0 - 2.0 / 10_000.0);
        assert!((realized - (exit_price - entry)).abs() < 1e-9, "{realized}");
        assert!((c.unrealized_pnl() - 0.0).abs() < 1e-9);
        assert!(c.positions().is_empty());
    }

    #[tokio::test]
    async fn short_round_trip_realizes_pnl() {
        let c = connector();
        c.place_order(&market("SOL/USDT", Side::Sell, 2.0))
            .await
            .unwrap();
        let positions = c.positions();
        assert!((positions[0].qty + 2.0).abs() < 1e-9, "should be short");

        c.set_price("SOL/USDT", 130.0);
        assert!(
            c.unrealized_pnl() > 0.0,
            "short profits when the price drops"
        );

        c.place_order(&market("SOL/USDT", Side::Buy, 2.0))
            .await
            .unwrap();
        let realized = c.realized_pnl();
        assert!(
            realized > 0.0,
            "short round trip should profit, got {realized}"
        );
        assert!(c.positions().is_empty());
    }

    #[tokio::test]
    async fn limit_order_rests_and_fills_when_the_price_crosses() {
        let c = connector();
        let mut req = market("SOL/USDT", Side::Buy, 1.0);
        req.order_type = OrderType::Limit;
        req.limit_price = Some(130.0);
        let ack = c.place_order(&req).await.unwrap();
        assert_eq!(ack.status, OrderStatus::Accepted);
        assert_eq!(c.resting_orders().len(), 1);
        assert!(c.trades().is_empty());

        // Price above the limit: no fill.
        c.set_price("SOL/USDT", 140.0);
        assert_eq!(c.resting_orders().len(), 1);

        // Price crosses the limit: filled at the limit price.
        c.set_price("SOL/USDT", 129.0);
        assert!(c.resting_orders().is_empty());
        let trades = c.trades();
        assert_eq!(trades.len(), 1);
        assert!(
            (trades[0].price - 130.0).abs() < 1e-9,
            "{}",
            trades[0].price
        );
        // The mark follows the fill, so the position is not instantly underwater.
        let positions = c.positions();
        assert_eq!(positions.len(), 1);
        assert!((positions[0].mark_price - 130.0).abs() < 1e-9);
        assert!((positions[0].unrealized_pnl - 0.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn limit_order_can_be_cancelled() {
        let c = connector();
        let mut req = market("SOL/USDT", Side::Buy, 1.0);
        req.order_type = OrderType::Limit;
        req.limit_price = Some(130.0);
        let ack = c.place_order(&req).await.unwrap();
        c.cancel_order(&ack.order_id).await.unwrap();
        assert!(c.resting_orders().is_empty());
        let err = c.cancel_order(&ack.order_id).await.expect_err("gone");
        assert!(matches!(err, ExchangeError::InvalidRequest(_)), "{err}");
    }

    #[tokio::test]
    async fn account_snapshot_reports_equity_and_free_cash() {
        let c = connector();
        let snap = c.account().await.unwrap();
        assert_eq!(snap.venue, Venue::Paper);
        assert!((snap.equity_usd - 10_000.0).abs() < 1e-9);
        assert!((snap.free_usd - 10_000.0).abs() < 1e-9);
        assert_eq!(snap.balances.len(), 1);
        assert_eq!(snap.balances[0].asset, "USDT");

        c.place_order(&market("SOL/USDT", Side::Buy, 1.0))
            .await
            .unwrap();
        let snap = c.account().await.unwrap();
        // Part of the wallet is locked as margin after the fill.
        assert!(snap.free_usd < snap.equity_usd);
        assert!(snap.balances[0].locked > 0.0);
    }

    #[tokio::test]
    async fn last_price_returns_the_simulated_price() {
        let c = connector();
        let px = c.last_price("SOL/USDT").await.unwrap();
        assert!((px - 142.0).abs() < 1e-9);
        // Unknown symbols are quoted at the default base price.
        let px = c.last_price("DOGE/USDT").await.unwrap();
        assert!((px - 1.0).abs() < 1e-9);
    }

    /// Mean reversion must pull back a fraction of the dollar gap. Treating that
    /// gap as a fractional return instead multiplied the price by (1 + gap),
    /// which produced ~94% jumps in a single tick.
    #[test]
    fn mean_reversion_pulls_back_a_fraction_of_the_gap() {
        let cfg = PaperConfig {
            seed: 7,
            vol_bps: 0.0,
            mean_reversion: 0.02,
            ..PaperConfig::default()
        };
        let c = PaperConnector::with_config(cfg.clone());
        let mut state = c.inner.state.lock().expect("state");
        state.prices.insert("SOL/USDT".to_string(), 50.0);

        let ticks = advance_prices(&mut state, &cfg);
        let sol = ticks
            .iter()
            .find(|t| t.symbol == "SOL/USDT")
            .expect("a SOL tick");
        // 50 + 2% of the 92-dollar gap to the 142 base price.
        assert!((sol.price - 51.84).abs() < 1e-9, "{}", sol.price);
    }

    #[tokio::test]
    async fn price_walk_is_deterministic_for_a_fixed_seed() {
        let build = || {
            let cfg = PaperConfig {
                seed: 7,
                vol_bps: 25.0,
                ..PaperConfig::default()
            };
            PaperConnector::with_config(cfg)
        };
        let a = build();
        let b = build();
        let ta = a.tick_once();
        let tb = b.tick_once();
        assert_eq!(ta.len(), tb.len());
        for (x, y) in ta.iter().zip(tb.iter()) {
            assert_eq!(x.symbol, y.symbol);
            assert!((x.price - y.price).abs() < 1e-12);
        }
    }

    #[tokio::test]
    async fn subscribe_trades_pushes_ticks() {
        let c = connector();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        c.subscribe_trades(&["SOL/USDT".to_string()], tx)
            .await
            .unwrap();
        let tick = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("a tick should arrive")
            .expect("channel open");
        assert_eq!(tick.venue, Venue::Paper);
        assert_eq!(tick.symbol, "SOL/USDT");
        assert!(tick.price > 0.0);
    }
}

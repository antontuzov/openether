use crate::bus::{Event, EventBus, StatusEvent};
use crate::config::Config;
use crate::exchange::{ExchangeConnector, ExchangeError, OpenOrder, TickSink};
use crate::metrics::Metrics;
use crate::onchain::OnchainClient;
use crate::risk::{RiskContext, RiskEngine, RiskState, RiskVerdict};
use crate::strategy::{StrategyHost, StrategySignal};
use crate::types::*;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

const MAX_TICKS: usize = 2_000;
const MAX_TRADES: usize = 1_000;
const MAX_LOGS: usize = 2_000;

#[derive(Debug, Clone, Serialize)]
pub struct SubmitOutcome {
    pub approved: bool,
    pub reason: String,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<OrderStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fill_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee: Option<f64>,
    pub risk: RiskVerdict,
}

#[derive(Debug, Default)]
struct DataStore {
    ticks: VecDeque<MarketTick>,
    trades: VecDeque<TradeRecord>,
    logs: VecDeque<LogEntry>,
    last_prices: HashMap<String, f64>,
    position_qty: HashMap<String, f64>,
    avg_entry: HashMap<String, f64>,
    realized_pnl: f64,
    equity_usd: f64,
    cash_usd: f64,
    open_orders: Vec<OpenOrder>,
}

/// Shared engine state. The API layer, the MCP server and the background loops
/// all operate on this single instance.
pub struct AppState {
    pub config: Config,
    pub metrics: Arc<Metrics>,
    pub bus: Arc<EventBus>,
    pub risk: Arc<RiskEngine>,
    pub strategy: Arc<std::sync::Mutex<Option<Arc<StrategyHost>>>>,
    pub connector: Arc<dyn ExchangeConnector>,
    pub onchain: Arc<OnchainClient>,
    pub ws_clients: std::sync::atomic::AtomicUsize,
    data: std::sync::Mutex<DataStore>,
}

impl AppState {
    pub fn new(config: Config) -> anyhow::Result<Arc<Self>> {
        let metrics = Arc::new(Metrics::new());
        let bus = Arc::new(EventBus::new(4_096, Arc::clone(&metrics)));
        let risk = Arc::new(RiskEngine::new(config.risk.clone(), Arc::clone(&metrics)));

        let connector = build_connector(&config)?;
        let onchain = Arc::new(
            OnchainClient::new(config.solana_rpc_url.clone(), config.evm_rpc_urls.clone())
                .map_err(|e| anyhow::anyhow!("on-chain client: {e}"))?,
        );

        let strategy = Arc::new(std::sync::Mutex::new(None));
        if let Some(path) = config.strategy_wasm.clone() {
            match StrategyHost::new(
                &path,
                config.wasm_fuel,
                config.wasm_memory_pages,
                Arc::clone(&metrics),
            ) {
                Ok(host) => {
                    tracing::info!(path = %path.display(), "sandboxed strategy loaded");
                    *strategy.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(host));
                }
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "no strategy loaded; the engine will run without an automated strategy");
                }
            }
        }

        let equity = config.paper_start_equity;
        {
            let mut data = data_store();
            data.equity_usd = equity;
            data.cash_usd = equity;
        }

        let state = Arc::new(Self {
            config,
            metrics,
            bus,
            risk,
            strategy,
            connector,
            onchain,
            ws_clients: std::sync::atomic::AtomicUsize::new(0),
            data: std::sync::Mutex::new(DataStore::default()),
        });
        {
            let mut data = state.lock_data();
            data.equity_usd = equity;
            data.cash_usd = equity;
        }
        Ok(state)
    }

    pub fn venue(&self) -> Venue {
        self.connector.venue()
    }

    pub fn log(&self, source: &str, level: &str, message: &str, context: serde_json::Value) {
        let entry = LogEntry {
            id: uuid::Uuid::new_v4().to_string(),
            ts_ms: now_ms(),
            source: source.to_string(),
            level: level.to_string(),
            message: message.to_string(),
            context,
        };
        match level {
            "error" => tracing::error!(source, "{message}"),
            "warn" => tracing::warn!(source, "{message}"),
            _ => tracing::info!(source, "{message}"),
        }
        let mut data = self.lock_data();
        if data.logs.len() >= MAX_LOGS {
            data.logs.pop_front();
        }
        data.logs.push_back(entry.clone());
        drop(data);
        self.bus.publish(Event::Log(entry));
    }

    pub fn ticks(&self, limit: usize) -> Vec<MarketTick> {
        let data = self.lock_data();
        let skip = data.ticks.len().saturating_sub(limit);
        data.ticks.iter().skip(skip).cloned().collect()
    }

    pub fn trades(&self, limit: usize) -> Vec<TradeRecord> {
        let data = self.lock_data();
        let skip = data.trades.len().saturating_sub(limit);
        data.trades.iter().skip(skip).rev().cloned().collect()
    }

    pub fn logs(&self, limit: usize) -> Vec<LogEntry> {
        let data = self.lock_data();
        let skip = data.logs.len().saturating_sub(limit);
        data.logs.iter().skip(skip).rev().cloned().collect()
    }

    pub fn last_price(&self, symbol: &str) -> Option<f64> {
        self.lock_data().last_prices.get(symbol).copied()
    }

    pub fn equity(&self) -> f64 {
        self.lock_data().equity_usd
    }

    pub fn portfolio(&self) -> PortfolioSnapshot {
        let data = self.lock_data();
        let positions: Vec<Position> = data
            .position_qty
            .iter()
            .filter(|(_, qty)| qty.abs() > 0.000_001)
            .map(|(symbol, qty)| {
                let mark = data.last_prices.get(symbol).copied().unwrap_or(0.0);
                let avg = data.avg_entry.get(symbol).copied().unwrap_or(0.0);
                Position {
                    venue: self.venue(),
                    symbol: symbol.clone(),
                    qty: *qty,
                    avg_entry: avg,
                    mark_price: mark,
                    unrealized_pnl: (mark - avg) * qty,
                    notional_usd: mark * qty.abs(),
                    ts_ms: now_ms(),
                }
            })
            .collect();
        let unrealized: f64 = positions.iter().map(|p| p.unrealized_pnl).sum();
        let realized = data.realized_pnl;
        let equity = data.equity_usd;
        let start_equity = self.config.paper_start_equity;
        PortfolioSnapshot {
            venue: self.venue(),
            equity_usd: equity,
            cash_usd: data.cash_usd,
            unrealized_pnl_usd: unrealized,
            realized_pnl_usd: realized,
            total_pnl_pct: if start_equity > 0.0 {
                (equity - start_equity) / start_equity * 100.0
            } else {
                0.0
            },
            positions,
            ts_ms: now_ms(),
        }
    }

    pub fn risk_state(&self) -> RiskState {
        self.risk.snapshot(self.equity(), now_ms())
    }

    /// The single entry point for placing an order.
    ///
    /// Every path — the agent through MCP, the WASM strategy, the HTTP API —
    /// funnels through here, and the risk layer is evaluated before the venue is
    /// ever contacted. There is no bypass.
    pub async fn submit_order(&self, mut req: OrderRequest, source: &str) -> SubmitOutcome {
        if req.venue != self.connector.venue() {
            req.venue = self.connector.venue();
        }
        let symbol = req.symbol.clone();
        let price = match self.resolve_price(&symbol).await {
            Some(p) => p,
            None => {
                let verdict = RiskVerdict {
                    approved: false,
                    reason: "price_unavailable".into(),
                    detail: format!("no market price available for {symbol}"),
                    breaker_state: self.risk.breaker_state(),
                    checked_at_ms: now_ms(),
                };
                self.log(
                    source,
                    "warn",
                    &format!("order rejected: no price for {symbol}"),
                    serde_json::json!({"symbol": symbol, "reason": verdict.reason}),
                );
                return SubmitOutcome {
                    approved: false,
                    reason: verdict.reason.clone(),
                    detail: verdict.detail.clone(),
                    order_id: None,
                    status: None,
                    fill_price: None,
                    fee: None,
                    risk: verdict,
                };
            }
        };

        let context = RiskContext {
            equity_usd: self.equity(),
            gross_exposure_usd: self.risk.gross_exposure(),
            open_positions: self.risk.open_positions(),
            current_position_qty: self.risk.position_qty(&symbol),
            price,
            contracts: Vec::new(),
        };

        let verdict = self.risk.evaluate(&req, &context);
        if !verdict.approved {
            self.log(
                source,
                "warn",
                &format!("order rejected by the risk layer: {}", verdict.detail),
                serde_json::json!({
                    "symbol": symbol,
                    "side": req.side,
                    "qty": req.qty,
                    "reason": verdict.reason,
                    "breaker_state": verdict.breaker_state,
                }),
            );
            return SubmitOutcome {
                approved: false,
                reason: verdict.reason.clone(),
                detail: verdict.detail.clone(),
                order_id: None,
                status: None,
                fill_price: None,
                fee: None,
                risk: verdict,
            };
        }

        let before = now_ms();
        let ack = match self.connector.place_order(&req).await {
            Ok(ack) => ack,
            Err(e) => {
                let detail = e.to_string();
                if self.risk.breaker_state() == crate::risk::BreakerState::HalfOpen {
                    self.risk.note_probe_failure(&detail);
                }
                self.log(
                    source,
                    "error",
                    &format!("venue rejected the order: {detail}"),
                    serde_json::json!({"symbol": symbol, "error": detail}),
                );
                let verdict = RiskVerdict {
                    approved: false,
                    reason: "venue_error".into(),
                    detail: detail.clone(),
                    breaker_state: self.risk.breaker_state(),
                    checked_at_ms: now_ms(),
                };
                return SubmitOutcome {
                    approved: false,
                    reason: verdict.reason.clone(),
                    detail,
                    order_id: None,
                    status: None,
                    fill_price: None,
                    fee: None,
                    risk: verdict,
                };
            }
        };

        self.metrics.counter_add(
            "openether_orders_submitted_total",
            &[("venue", req.venue.as_str()), ("side", req.side.as_str())],
            1.0,
        );

        let fills = self
            .connector
            .fills_since(before - 1)
            .await
            .unwrap_or_default();
        let mut fill_price = None;
        let mut fee = None;
        for fill in fills {
            if fill.symbol != symbol {
                continue;
            }
            self.apply_fill(&fill, &req);
            fill_price = Some(fill.price);
            fee = Some(fill.fee);
            self.metrics.counter_add(
                "openether_orders_filled_total",
                &[("venue", fill.venue.as_str())],
                1.0,
            );
        }
        if fill_price.is_none() {
            // The venue did not report a fill (limit order resting, or a venue
            // without fill reporting). Record the intent so the UI still shows it.
            let synthetic = OrderFill {
                venue: req.venue,
                order_id: ack.order_id.clone(),
                symbol: symbol.clone(),
                side: req.side,
                qty: req.qty,
                price,
                fee: 0.0,
                ts_ms: now_ms(),
            };
            self.apply_fill(&synthetic, &req);
            fill_price = Some(price);
        }

        self.log(
            source,
            "info",
            &format!(
                "order filled: {} {} {} @ {}",
                req.side,
                req.qty,
                symbol,
                fill_price.unwrap_or(price)
            ),
            serde_json::json!({
                "symbol": symbol,
                "side": req.side,
                "qty": req.qty,
                "price": fill_price,
                "rationale": req.rationale,
                "order_id": ack.order_id,
            }),
        );

        SubmitOutcome {
            approved: true,
            reason: verdict.reason.clone(),
            detail: verdict.detail.clone(),
            order_id: Some(ack.order_id),
            status: Some(ack.status),
            fill_price,
            fee,
            risk: verdict,
        }
    }

    fn apply_fill(&self, fill: &OrderFill, req: &OrderRequest) {
        let signed_qty = fill.side.sign() * fill.qty;
        let mut data = self.lock_data();
        let symbol = fill.symbol.clone();
        let prev_qty = data.position_qty.get(&symbol).copied().unwrap_or(0.0);
        let prev_avg = data.avg_entry.get(&symbol).copied().unwrap_or(0.0);
        let new_qty = prev_qty + signed_qty;

        // Realized PnL when reducing an existing position.
        let mut realized = 0.0;
        if prev_qty != 0.0 && (prev_qty > 0.0) != (signed_qty > 0.0) {
            let closed = prev_qty.abs().min(fill.qty.abs());
            let direction = if prev_qty > 0.0 { 1.0 } else { -1.0 };
            realized = (fill.price - prev_avg) * closed * direction;
            data.realized_pnl += realized;
        }
        let new_avg = if prev_qty == 0.0 || (prev_qty > 0.0) == (signed_qty > 0.0) {
            let total = prev_qty.abs() * prev_avg + signed_qty.abs() * fill.price;
            let qty_sum = prev_qty.abs() + signed_qty.abs();
            if qty_sum > 0.0 {
                total / qty_sum
            } else {
                0.0
            }
        } else {
            prev_avg
        };
        data.position_qty.insert(symbol.clone(), new_qty);
        data.avg_entry.insert(symbol, new_avg);
        data.last_prices.insert(fill.symbol.clone(), fill.price);
        data.equity_usd -= fill.fee;

        let record = TradeRecord {
            id: uuid::Uuid::new_v4().to_string(),
            venue: fill.venue,
            symbol: fill.symbol.clone(),
            side: fill.side,
            qty: fill.qty,
            price: fill.price,
            fee: fill.fee,
            realized_pnl: realized,
            ts_ms: fill.ts_ms,
            rationale: req.rationale.clone(),
            source: if req.rationale.is_empty() {
                "engine".into()
            } else {
                "agent".into()
            },
        };
        if data.trades.len() >= MAX_TRADES {
            data.trades.pop_front();
        }
        data.trades.push_back(record.clone());
        drop(data);

        self.risk.on_fill(fill, self.equity());
        self.bus.publish(Event::Trade(record));
    }

    pub async fn resolve_price(&self, symbol: &str) -> Option<f64> {
        if let Some(p) = self.last_price(symbol) {
            if p > 0.0 {
                return Some(p);
            }
        }
        self.connector
            .last_price(symbol)
            .await
            .ok()
            .filter(|p| *p > 0.0)
    }

    pub fn record_tick(&self, tick: MarketTick) {
        let mut data = self.lock_data();
        data.last_prices.insert(tick.symbol.clone(), tick.price);
        if data.ticks.len() >= MAX_TICKS {
            data.ticks.pop_front();
        }
        data.ticks.push_back(tick.clone());
        drop(data);
        self.bus.publish(Event::Tick(tick));
    }

    pub async fn refresh_account(&self) {
        match self.connector.account().await {
            Ok(account) => {
                {
                    let mut data = self.lock_data();
                    data.equity_usd = account.equity_usd;
                    data.cash_usd = account.free_usd;
                }
                self.risk.update_equity(account.equity_usd, account.ts_ms);
                self.metrics
                    .gauge_set("openether_pnl_usd", &[], self.risk.daily_realized_pnl());
                let portfolio = self.portfolio();
                self.bus.publish(Event::Portfolio(portfolio));
                let risk_state = self.risk_state();
                self.bus.publish(Event::Risk(risk_state));
            }
            Err(e) => {
                tracing::warn!(error = %e, "account refresh failed");
            }
        }
    }

    pub async fn refresh_open_orders(&self) {
        if let Ok(orders) = self.connector.open_orders().await {
            self.lock_data().open_orders = orders;
        }
    }

    /// Runs one strategy tick and forwards any proposals to the risk layer.
    pub async fn run_strategy_tick(&self) {
        let strategy = {
            let guard = self.strategy.lock().unwrap_or_else(|e| e.into_inner());
            guard.clone()
        };
        let Some(strategy) = strategy else { return };

        let portfolio = self.portfolio();
        let symbol = self
            .config
            .symbols
            .first()
            .cloned()
            .unwrap_or_else(|| "SOL/USDT".to_string());
        let price = self.resolve_price(&symbol).await.unwrap_or(0.0);
        let ctx = serde_json::json!({
            "ts_ms": now_ms(),
            "symbol": symbol,
            "price": price,
            "equity_usd": portfolio.equity_usd,
            "positions": portfolio.positions.iter().map(|p| serde_json::json!({
                "symbol": p.symbol,
                "qty": p.qty,
                "avg_entry": p.avg_entry,
                "mark": p.mark_price,
            })).collect::<Vec<_>>(),
            "params": self.strategy_params(),
        });

        match strategy.on_tick(&ctx) {
            Ok(signals) => {
                for signal in signals {
                    self.apply_signal(signal).await;
                }
            }
            Err(e) => {
                self.metrics
                    .counter_add("openether_strategy_errors_total", &[], 1.0);
                self.log(
                    "strategy",
                    "error",
                    &format!("strategy error: {e}"),
                    serde_json::json!({"error": e.to_string()}),
                );
            }
        }
    }

    fn strategy_params(&self) -> serde_json::Value {
        serde_json::json!({
            "max_position_pct": self.config.risk.max_position_pct_of_equity,
            "min_notional_usd": self.config.risk.min_order_notional_usd,
            "stop_loss_pct": 2.0,
            "take_profit_pct": 4.0,
        })
    }

    async fn apply_signal(&self, signal: StrategySignal) {
        let req = OrderRequest {
            venue: self.connector.venue(),
            symbol: signal.symbol.clone(),
            side: signal.side,
            qty: signal.qty,
            order_type: OrderType::Market,
            limit_price: None,
            reduce_only: false,
            client_id: format!("oe-strategy-{}", uuid::Uuid::new_v4().simple()),
            rationale: format!("wasm strategy: {}", signal.reason),
            reduce_only_guard: false,
        };
        let outcome = self.submit_order(req, "strategy").await;
        if !outcome.approved {
            self.log(
                "strategy",
                "warn",
                &format!("strategy proposal rejected: {}", outcome.detail),
                serde_json::json!({"reason": outcome.reason, "detail": outcome.detail}),
            );
        }
    }

    pub fn publish_status(&self, state: &str, detail: &str) {
        self.bus.publish(Event::Status(StatusEvent {
            state: state.to_string(),
            detail: detail.to_string(),
            ts_ms: now_ms(),
        }));
    }

    fn lock_data(&self) -> std::sync::MutexGuard<'_, DataStore> {
        self.data.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn data_store() -> DataStore {
    DataStore::default()
}

fn build_connector(config: &Config) -> anyhow::Result<Arc<dyn ExchangeConnector>> {
    Ok(match config.venue {
        Venue::Paper => Arc::new(crate::exchange::paper::PaperConnector::new()),
        Venue::Binance => Arc::new(crate::exchange::binance::BinanceConnector::from_env()?),
        Venue::Kraken => Arc::new(crate::exchange::kraken::KrakenConnector::from_env()?),
        Venue::Hyperliquid => {
            Arc::new(crate::exchange::hyperliquid::HyperliquidConnector::from_env()?)
        }
    })
}

/// Background loops: market data, strategy evaluation and account refresh.
pub async fn run_background_tasks(state: Arc<AppState>) {
    let symbols = state.config.symbols.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<MarketTick>();

    let sink: TickSink = tx;
    if let Err(e) = state.connector.subscribe_trades(&symbols, sink).await {
        tracing::warn!(error = %e, "market data subscription failed");
        state.log(
            "engine",
            "warn",
            &format!("market data subscription failed: {e}"),
            serde_json::json!({"error": e.to_string()}),
        );
    }

    let tick_state = Arc::clone(&state);
    tokio::spawn(async move {
        while let Some(tick) = rx.recv().await {
            tick_state.record_tick(tick);
        }
    });

    let strategy_state = Arc::clone(&state);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(5));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            strategy_state.run_strategy_tick().await;
        }
    });

    let account_state = Arc::clone(&state);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(2));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            account_state.refresh_account().await;
            account_state.refresh_open_orders().await;
        }
    });
}

/// Maps a connector error into a stable machine-readable reason.
pub fn error_reason(error: &ExchangeError) -> &'static str {
    match error {
        ExchangeError::Network(_) => "network_error",
        ExchangeError::Rejected(_) => "venue_rejected",
        ExchangeError::InsufficientBalance { .. } => "insufficient_balance",
        ExchangeError::Unsupported(_) => "unsupported",
        ExchangeError::InvalidRequest(_) => "invalid_request",
        ExchangeError::Auth(_) => "auth_error",
        ExchangeError::RateLimited(_) => "rate_limited",
        ExchangeError::Config(_) => "config_error",
        ExchangeError::Internal(_) => "internal_error",
    }
}

use crate::bus::{Event, EventBus, StatusEvent};
use crate::config::Config;
use crate::exchange::{AccountRestore, ExchangeConnector, ExchangeError, OpenOrder, TickSink};
use crate::metrics::Metrics;
use crate::onchain::OnchainClient;
use crate::risk::{RiskContext, RiskEngine, RiskState, RiskVerdict};
use crate::store::{PortfolioRow, PositionRow, Store};
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
    /// Durable state. `None` when the run is configured to keep everything in
    /// memory.
    pub store: Option<Arc<Store>>,
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
        let store = match config.state_db_path.as_deref() {
            Some(path) => match Store::open(path) {
                Ok(store) => Some(Arc::new(store)),
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "could not open the state database; running without persistence"
                    );
                    None
                }
            },
            None => None,
        };

        let state = Arc::new(Self {
            config,
            metrics,
            bus,
            risk,
            strategy,
            connector,
            onchain,
            ws_clients: std::sync::atomic::AtomicUsize::new(0),
            store,
            data: std::sync::Mutex::new(DataStore::default()),
        });
        {
            let mut data = state.lock_data();
            data.equity_usd = equity;
            data.cash_usd = equity;
        }
        state.restore_persisted();
        Ok(state)
    }

    /// Load the persisted account back into the engine.
    ///
    /// A restart must not look like a fresh account. The positions, the equity
    /// baseline and the risk layer's peak and daily anchors all come back,
    /// otherwise every limit would be measured against a fresh start — and a
    /// breaker that was open would quietly close itself.
    fn restore_persisted(&self) {
        let Some(store) = self.store.as_ref() else {
            return;
        };

        let portfolio = match store.load_portfolio() {
            Ok(row) => row,
            Err(e) => {
                tracing::warn!(error = %e, "could not read the persisted portfolio");
                None
            }
        };
        let positions = match store.load_positions() {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(error = %e, "could not read the persisted positions");
                Vec::new()
            }
        };
        let risk_row = match store.load_risk_state() {
            Ok(row) => row,
            Err(e) => {
                tracing::warn!(error = %e, "could not read the persisted risk state");
                None
            }
        };

        let restored_positions = !positions.is_empty();
        {
            let mut data = self.lock_data();
            if let Some(p) = portfolio {
                data.equity_usd = p.equity_usd;
                data.cash_usd = p.cash_usd;
                data.realized_pnl = p.realized_pnl;
            }
            for p in &positions {
                data.position_qty.insert(p.symbol.clone(), p.qty);
                data.avg_entry.insert(p.symbol.clone(), p.avg_entry);
            }
        }

        // Hand the account back to the venue. The simulated venue has no
        // server to ask, so without this it would come back from a restart
        // holding its starting cash and no positions, while the engine
        // believed it held a position — two accounts that disagree.
        //
        // Only when something was persisted: a fresh run must keep the
        // venue's starting balance.
        if let Some(p) = portfolio.as_ref() {
            let restored_account = AccountRestore {
                cash_usd: p.cash_usd,
                realized_pnl: p.realized_pnl,
                positions: positions
                    .iter()
                    .map(|p| (p.symbol.clone(), p.qty, p.avg_entry))
                    .collect(),
            };
            if let Err(e) = self.connector.restore_account(&restored_account) {
                tracing::warn!(error = %e, "could not restore the venue account");
            }
        }

        if let Some(row) = risk_row {
            let triples: Vec<(String, f64, f64)> = positions
                .iter()
                .map(|p| (p.symbol.clone(), p.qty, p.avg_entry))
                .collect();
            self.risk.restore(&row, &triples);
        }

        let trades = store.trades(MAX_TRADES).unwrap_or_default();
        let trade_count = trades.len();
        {
            let mut data = self.lock_data();
            for trade in trades.into_iter().rev() {
                data.trades.push_back(trade);
            }
        }

        if restored_positions || portfolio.is_some() {
            tracing::info!(
                positions = positions.len(),
                trades = trade_count,
                "restored the persisted account"
            );
        }
    }

    /// Write the engine's risk-relevant state to disk.
    ///
    /// Called after every fill. The portfolio, the positions and the risk
    /// state are written together, because the risk layer measures exposure
    /// from the position map: if the two were written separately a crash
    /// could leave them disagreeing.
    fn persist(&self) {
        let Some(store) = self.store.as_ref() else {
            return;
        };

        let account = self.connector.account_state();
        let (portfolio, positions, orders) = {
            let data = self.lock_data();
            let portfolio = PortfolioRow {
                equity_usd: data.equity_usd,
                cash_usd: account
                    .as_ref()
                    .map(|a| a.cash_usd)
                    .unwrap_or(data.cash_usd),
                realized_pnl: data.realized_pnl,
            };
            let positions: Vec<PositionRow> = match &account {
                // The venue owns the account, so its positions are the truth.
                Some(a) if !a.positions.is_empty() => a
                    .positions
                    .iter()
                    .map(|(symbol, qty, avg_entry)| PositionRow {
                        symbol: symbol.clone(),
                        qty: *qty,
                        avg_entry: *avg_entry,
                    })
                    .collect(),
                _ => data
                    .position_qty
                    .iter()
                    .filter(|(_, qty)| qty.abs() > 0.000_001)
                    .map(|(symbol, qty)| PositionRow {
                        symbol: symbol.clone(),
                        qty: *qty,
                        avg_entry: data.avg_entry.get(symbol).copied().unwrap_or(0.0),
                    })
                    .collect(),
            };
            let orders = data.open_orders.clone();
            (portfolio, positions, orders)
        };

        if let Err(e) = store.save_portfolio(portfolio) {
            tracing::warn!(error = %e, "could not persist the portfolio");
        }
        if let Err(e) = store.save_positions(&positions) {
            tracing::warn!(error = %e, "could not persist the positions");
        }
        if let Err(e) = store.save_open_orders(&orders) {
            tracing::warn!(error = %e, "could not persist the open orders");
        }
        if let Err(e) = store.save_risk_state(&self.risk.export()) {
            tracing::warn!(error = %e, "could not persist the risk state");
        }
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

        // Keep the client id as the stable handle for this order, so it can
        // still be found and cancelled after the process restarts. The
        // connectors keep this mapping in memory only.
        if let Some(store) = self.store.as_ref() {
            if let Err(e) =
                store.remember_order_client(&req.client_id, req.venue, &ack.order_id, &req.symbol)
            {
                tracing::warn!(error = %e, "could not persist the order client mapping");
            }
        }

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

        if let Some(store) = self.store.as_ref() {
            if let Err(e) = store.record_trade(&record) {
                tracing::warn!(error = %e, "could not journal the trade");
            }
        }

        self.risk.on_fill(fill, self.equity());
        self.persist();
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

fn build_connector(config: &Config) -> anyhow::Result<Arc<dyn ExchangeConnector>> {
    // The arming ceremony decides whether this connector may place real
    // orders. The connector never decides that for itself.
    let live = config.live_trading;
    Ok(match config.venue {
        Venue::Paper => Arc::new(crate::exchange::paper::PaperConnector::new()),
        Venue::Binance => Arc::new(crate::exchange::binance::BinanceConnector::from_env(live)?),
        Venue::Kraken => Arc::new(crate::exchange::kraken::KrakenConnector::from_env(live)?),
        Venue::Hyperliquid => {
            Arc::new(crate::exchange::hyperliquid::HyperliquidConnector::from_env(live)?)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::risk::BreakerState;
    use std::path::{Path, PathBuf};

    fn temp_db() -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("oe-engine-{}", uuid::Uuid::new_v4()));
        let db = dir.join("state.db");
        (dir, db)
    }

    fn config_with_db(db: &Path) -> Config {
        let mut config = Config::from_env();
        config.venue = Venue::Paper;
        config.live_trading = false;
        config.paper_start_equity = 10_000.0;
        config.state_db_path = Some(db.to_path_buf());
        config.strategy_wasm = None;
        config
    }

    #[tokio::test]
    async fn a_restart_restores_the_account() {
        let (dir, db) = temp_db();

        let equity_before;
        {
            let state = AppState::new(config_with_db(&db)).unwrap();
            let outcome = state
                .submit_order(
                    OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Buy, 1.0),
                    "test",
                )
                .await;
            assert!(outcome.approved, "{outcome:?}");
            equity_before = state.equity();
            // The fee is charged, the margin is locked rather than spent.
            assert!(equity_before < 10_000.0, "the fee should have been charged");
        }

        // A second handle on the same database is what a restarted process gets.
        let state = AppState::new(config_with_db(&db)).unwrap();
        let portfolio = state.portfolio();
        assert!(
            (portfolio.equity_usd - equity_before).abs() < 1e-9,
            "equity should survive a restart: {} != {}",
            portfolio.equity_usd,
            equity_before
        );
        assert_eq!(portfolio.positions.len(), 1, "the position should survive");
        assert_eq!(portfolio.positions[0].symbol, "SOL/USDT");
        assert!((portfolio.positions[0].qty - 1.0).abs() < 1e-9);
        assert_eq!(
            state.trades(10).len(),
            1,
            "the trade journal should survive"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_restart_restores_the_venue_account_too() {
        let (dir, db) = temp_db();
        {
            let state = AppState::new(config_with_db(&db)).unwrap();
            let outcome = state
                .submit_order(
                    OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Buy, 5.0),
                    "test",
                )
                .await;
            assert!(outcome.approved, "{outcome:?}");
        }

        let state = AppState::new(config_with_db(&db)).unwrap();
        // The simulator keeps the account, not just the engine's mirror of it.
        // If the venue came back empty it would hold its starting cash while
        // the engine believed it held a position.
        let account = state.connector.account().await.unwrap();
        assert!(
            account.equity_usd < 10_000.0,
            "the fee must survive the restart: {}",
            account.equity_usd
        );
        assert!(
            account.free_usd < account.equity_usd,
            "the margin must still be locked after the restart: free {} vs equity {}",
            account.free_usd,
            account.equity_usd
        );
        let venue_positions = state.connector.positions().await.unwrap();
        assert_eq!(venue_positions.len(), 1, "the venue must hold the position");
        assert_eq!(venue_positions[0].symbol, "SOL/USDT");
        assert!((venue_positions[0].qty - 5.0).abs() < 1e-9);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_restart_keeps_the_risk_layer_aware_of_the_position() {
        let (dir, db) = temp_db();
        {
            let state = AppState::new(config_with_db(&db)).unwrap();
            let outcome = state
                .submit_order(
                    OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Buy, 1.0),
                    "test",
                )
                .await;
            assert!(outcome.approved, "{outcome:?}");
            assert!(
                state.risk.gross_exposure() > 0.0,
                "the risk layer should know about the fill"
            );
        }

        let state = AppState::new(config_with_db(&db)).unwrap();
        assert!(
            state.risk.gross_exposure() > 0.0,
            "the restored position must be visible to the risk layer, otherwise \
             the gross exposure cap would be measured against nothing"
        );
        assert_eq!(state.risk.open_positions(), 1);
        assert!((state.risk.position_qty("SOL/USDT") - 1.0).abs() < 1e-9);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn an_open_breaker_survives_a_restart() {
        let (dir, db) = temp_db();
        {
            let state = AppState::new(config_with_db(&db)).unwrap();
            // Establish a peak, then collapse the equity to trip the drawdown
            // limit the same way a real loss would.
            state.risk.update_equity(10_000.0, now_ms());
            state.risk.update_equity(1_000.0, now_ms());
            assert_eq!(state.risk.breaker_state(), BreakerState::Open);
            state.persist();
        }

        let state = AppState::new(config_with_db(&db)).unwrap();
        assert_eq!(
            state.risk.breaker_state(),
            BreakerState::Open,
            "restarting must not close the breaker — that would make a restart \
             a way around the risk layer"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_restored_position_is_still_capped_by_the_risk_layer() {
        let (dir, db) = temp_db();
        {
            let state = AppState::new(config_with_db(&db)).unwrap();
            let outcome = state
                .submit_order(
                    OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Buy, 10.0),
                    "test",
                )
                .await;
            assert!(outcome.approved, "{outcome:?}");
        }

        let state = AppState::new(config_with_db(&db)).unwrap();
        // The restored position is already at the 20% cap, so another order of
        // the same size must be refused.
        let outcome = state
            .submit_order(
                OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Buy, 10.0),
                "test",
            )
            .await;
        assert!(
            !outcome.approved,
            "the position cap must still apply to a restored position"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}

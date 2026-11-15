use crate::alert::{Alerter, BreakerWatch};
use crate::backtest::{BacktestError, BacktestParams, BacktestReport, Bar};
use crate::broadcast::{
    Broadcast, BroadcastError, BroadcastState, Broadcaster, HttpRpc, TransferRequest, TxSigner,
};
use crate::bus::{Event, EventBus, StatusEvent};
use crate::config::Config;
use crate::exchange::{
    AccountRestore, ExchangeConnector, ExchangeError, OpenOrder, RestoredPosition, TickSink,
};
use crate::holdings::{self, HoldingsService, WalletHoldings};
use crate::metrics::Metrics;
use crate::onchain::OnchainClient;
use crate::risk::{RiskContext, RiskEngine, RiskState, RiskVerdict};
use crate::store::{PortfolioRow, PositionLot, PositionRow, Store};
use crate::strategy::{StrategyHost, StrategyInfo, StrategySignal};
use crate::swap::{encode_approve, HttpQuoter, SwapError, SwapQuote, SwapRequest, SwapService};
use crate::types::*;
use crate::wallet::{AccountSelector, WalletError, WalletService};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

const MAX_TICKS: usize = 2_000;
pub const MAX_TRADES: usize = 1_000;
const MAX_LOGS: usize = 2_000;
const MAX_ORDERS: usize = 500;
const MAX_BROADCASTS: usize = 500;
/// How often the on-chain wallet is revalued.
const WALLET_REFRESH_SECS: u64 = 30;
/// How often submitted transactions are re-read from the chain.
const BROADCAST_RECONCILE_SECS: u64 = 5;
/// How often submitted orders are reconciled against the venue.
const ORDER_RECONCILE_SECS: u64 = 2;
/// Reconciliation re-reads a window that overlaps the last pass, so a fill
/// reported late is still seen. Dedupe makes the overlap harmless.
const FILL_WINDOW_OVERLAP_MS: i64 = 60_000;
/// How long an order may be absent from the venue's open list before it is
/// retired. Trade history lags the open list, so retiring on the first absence
/// would label a real fill as a cancellation.
const ORDER_RETIRE_GRACE_MS: i64 = 30_000;

/// Why the router refused to name a venue for an order.
#[derive(Debug, Clone)]
struct RouteRefusal {
    reason: &'static str,
    detail: String,
}

/// The venue an order goes to, and the price the risk layer sizes it at.
#[derive(Clone)]
struct Route {
    connector: Arc<dyn ExchangeConnector>,
    venue: Venue,
    price: f64,
    /// Every venue that quoted, fee-adjusted, for the log line. Empty when
    /// there was nothing to compare.
    compared: Vec<(Venue, f64)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubmitOutcome {
    pub approved: bool,
    pub reason: String,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
    /// The venue the router sent the order to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub venue: Option<Venue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<OrderStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fill_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee: Option<f64>,
    pub risk: RiskVerdict,
}

/// A transfer that was signed and sent, with the dollar value the armed cap
/// measured it at.
#[derive(Debug, Clone, Serialize)]
pub struct TransferReceipt {
    pub broadcast: Broadcast,
    pub notional_usd: f64,
}

/// Why a transfer was refused before anything was signed or sent.
#[derive(Debug, thiserror::Error)]
pub enum TransferError {
    #[error("on-chain transfers are not armed: {0}")]
    NotArmed(String),
    #[error("the wallet is in dry-run mode; live signing is disabled")]
    DryRun,
    #[error("on-chain transfers are not configured: no RPC client")]
    NotConfigured,
    #[error("the network '{0}' has no RPC url configured")]
    NetworkNotConfigured(String),
    #[error("'{0}' is not a network this core can send on")]
    UnsupportedNetwork(String),
    #[error(
        "the {0} gas coin could not be priced, so the transfer cannot be measured against the cap"
    )]
    Unpriced(String),
    #[error("the transfer is worth ${0:.2} of {1}, above the armed cap of ${2:.2}")]
    AboveCap(f64, String, f64),
    #[error(transparent)]
    Wallet(#[from] WalletError),
    #[error(transparent)]
    Broadcast(#[from] BroadcastError),
}

/// A checkable swap: the quote, the account that would sign it and the dollar
/// value at risk.
#[derive(Debug, Clone, Serialize)]
pub struct SwapPlan {
    pub quote: SwapQuote,
    pub notional_usd: f64,
    pub from: String,
    /// `true` when the token sale needs an approval before it can execute.
    pub approval_required: bool,
}

/// Why a registry selection was refused.
#[derive(Debug, thiserror::Error)]
pub enum StrategySelectError {
    #[error("unknown strategy: {0}")]
    Unknown(String),
    #[error("no strategy is loaded, so there is no sandbox to swap")]
    NoHost,
    #[error("the strategy artifact is missing: {0} (run ./scripts/build-strategy.sh)")]
    MissingArtifact(String),
    #[error("strategy parameters must be a JSON object")]
    InvalidParams,
    #[error("could not load the strategy: {0}")]
    Load(String),
}

/// A swap that was signed and sent.
#[derive(Debug, Clone, Serialize)]
pub struct SwapReceipt {
    pub broadcast: Broadcast,
    pub quote: SwapQuote,
    pub notional_usd: f64,
}

/// An exact ERC-20 approval to send.
#[derive(Debug, Clone)]
pub struct TokenApproval {
    pub network: String,
    /// The ERC-20 whose allowance is being granted.
    pub token: String,
    /// The spender, which must be on the contract allowlist.
    pub spender: String,
    /// The exact amount the spender may move.
    pub amount_raw: u128,
    pub max_fee_per_gas_wei: Option<u128>,
}

/// Why a swap was refused before anything was signed or sent.
#[derive(Debug, thiserror::Error)]
pub enum SwapExecutionError {
    #[error("on-chain trading is not armed: {0}")]
    NotArmed(String),
    #[error("the wallet is in dry-run mode; live signing is disabled")]
    DryRun,
    #[error("on-chain transfers are not configured: no RPC client")]
    NotConfigured,
    #[error("swaps are not configured: set OPENETHER_SWAP_QUOTE_URL to a quote source")]
    NoQuoter,
    #[error("the {0} price could not be read, so the swap cannot be measured against the cap")]
    Unpriced(String),
    #[error("the swap is worth ${0:.2} of {1}, above the armed cap of ${2:.2}")]
    AboveCap(f64, String, f64),
    #[error(
        "the spender {spender} may move {current_raw} raw units of {token}, but the swap sells {required_raw}; approve it first"
    )]
    ApprovalRequired {
        spender: String,
        current_raw: u128,
        token: String,
        required_raw: u128,
    },
    #[error(transparent)]
    Swap(#[from] SwapError),
    #[error(transparent)]
    Wallet(#[from] WalletError),
    #[error(transparent)]
    Broadcast(#[from] BroadcastError),
}

/// A FIFO lot: an open purchase (or short sale) at a price.
///
/// Closing a position consumes the oldest lots first, so realized PnL and the
/// remaining cost basis match a first-in-first-out tax report instead of a
/// running average that hides which units were sold.
#[derive(Debug, Clone, Copy)]
struct Lot {
    qty: f64,
    price: f64,
}

const LOT_EPSILON: f64 = 1e-12;

/// Consume `qty` from the front of `lots`, returning the realized PnL.
///
/// `direction` is the side of the position being closed: `1.0` for a long,
/// `-1.0` for a short. The caller separates the closing part of a fill from
/// any remainder that flips the position.
fn consume_lots(lots: &mut Vec<Lot>, qty: f64, price: f64, direction: f64) -> f64 {
    let mut remaining = qty;
    let mut realized = 0.0;
    while remaining > LOT_EPSILON && !lots.is_empty() {
        let lot = &mut lots[0];
        let take = lot.qty.min(remaining);
        realized += (price - lot.price) * take * direction;
        lot.qty -= take;
        remaining -= take;
        if lot.qty <= LOT_EPSILON {
            lots.remove(0);
        }
    }
    realized
}

fn lots_average(lots: &[Lot]) -> f64 {
    let total: f64 = lots.iter().map(|lot| lot.qty).sum();
    if total > LOT_EPSILON {
        lots.iter().map(|lot| lot.qty * lot.price).sum::<f64>() / total
    } else {
        0.0
    }
}

#[derive(Debug, Default)]
struct DataStore {
    ticks: VecDeque<MarketTick>,
    trades: VecDeque<TradeRecord>,
    logs: VecDeque<LogEntry>,
    last_prices: HashMap<String, f64>,
    position_qty: HashMap<String, f64>,
    avg_entry: HashMap<String, f64>,
    /// The venue that holds each symbol's inventory.
    ///
    /// The engine keeps one book per symbol. A symbol therefore lives on at
    /// most one venue at a time: routing may choose any venue to open it, but
    /// every later order for that symbol — above all the closing order — stays
    /// on the venue that actually holds it. That is what keeps a long on one
    /// exchange from being netted against a short on another, which would look
    /// flat to the risk layer while leaving a naked position on each venue.
    position_venue: HashMap<String, Venue>,
    /// Open lots per symbol, oldest first.
    lots: HashMap<String, Vec<Lot>>,
    realized_pnl: f64,
    equity_usd: f64,
    /// Whether `equity_usd` came from the venue or a persisted run rather than
    /// the configured starting balance. Until it does, marks must not feed the
    /// risk layer: anchoring its peak to a guess would invent a drawdown.
    equity_anchored: bool,
    cash_usd: f64,
    open_orders: Vec<OpenOrder>,
    /// Orders the engine submitted, with the fills the venue has confirmed.
    orders: VecDeque<TrackedOrder>,
    /// How far the fill history has been read. Persisted so a restart does not
    /// re-book fills the previous run already journaled.
    fill_cursor_ms: i64,
    /// The last reading of the on-chain wallet.
    wallet_holdings: Option<WalletHoldings>,
    /// Every transfer this core signed, in the order it was created.
    broadcasts: VecDeque<Broadcast>,
}

/// Shared engine state. The API layer, the MCP server and the background loops
/// all operate on this single instance.
pub struct AppState {
    pub config: Config,
    pub metrics: Arc<Metrics>,
    pub bus: Arc<EventBus>,
    pub risk: Arc<RiskEngine>,
    pub strategy: Arc<std::sync::Mutex<Option<Arc<StrategyHost>>>>,
    /// The loaded strategy's own knobs. The risk-derived keys are filled in
    /// from the risk limits on every tick, so selecting a strategy cannot
    /// widen a limit.
    pub strategy_params: std::sync::Mutex<serde_json::Value>,
    /// Every venue this process may route to, in preference order.
    pub connectors: Vec<Arc<dyn ExchangeConnector>>,
    /// The primary venue: the book is reported under it and its account is the
    /// one restored on restart.
    pub connector: Arc<dyn ExchangeConnector>,
    pub onchain: Arc<OnchainClient>,
    /// The encrypted keystore and its unlock lease. Public metadata only until
    /// an operator unlocks it.
    pub wallet: Arc<WalletService>,
    /// Sends signed transfers to the chains. `None` when the RPC client could
    /// not be built; a transfer is also refused when the network has no url.
    pub broadcaster: Option<Arc<Broadcaster>>,
    /// Values the wallet's holdings. `None` only when the price client could
    /// not be built.
    pub holdings: Option<Arc<HoldingsService>>,
    /// Quotes and checks DEX swaps. `None` when no quote source is configured,
    /// in which case every swap is refused.
    pub swaps: Option<Arc<SwapService>>,
    pub ws_clients: std::sync::atomic::AtomicUsize,
    /// Durable state. `None` when the run is configured to keep everything in
    /// memory.
    pub store: Option<Arc<Store>>,
    /// Pushes breaker events to the operator's webhook.
    pub alerts: Arc<Alerter>,
    /// The last breaker state this process reported, so one opening is one
    /// alert even when several orders are refused in a row.
    breaker_watch: std::sync::Mutex<BreakerWatch>,
    data: std::sync::Mutex<DataStore>,
}

impl AppState {
    pub fn new(config: Config) -> anyhow::Result<Arc<Self>> {
        let connectors = build_connectors(&config)?;
        Self::build(config, connectors)
    }

    /// Build an engine around a venue the caller supplies. Tests use this to
    /// drive reconciliation through the answers a real exchange gives.
    #[cfg(test)]
    pub fn with_connector(
        config: Config,
        connector: Arc<dyn ExchangeConnector>,
    ) -> anyhow::Result<Arc<Self>> {
        Self::build(config, vec![connector])
    }

    /// Build an engine around several venues, in preference order.
    #[cfg(test)]
    pub fn with_connectors(
        config: Config,
        connectors: Vec<Arc<dyn ExchangeConnector>>,
    ) -> anyhow::Result<Arc<Self>> {
        Self::build(config, connectors)
    }

    fn build(
        config: Config,
        connectors: Vec<Arc<dyn ExchangeConnector>>,
    ) -> anyhow::Result<Arc<Self>> {
        let connector = connectors
            .first()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no exchange connector was configured"))?;
        let metrics = Arc::new(Metrics::new());
        let bus = Arc::new(EventBus::new(4_096, Arc::clone(&metrics)));
        let risk = Arc::new(RiskEngine::new(config.risk.clone(), Arc::clone(&metrics)));
        let webhook = config.alert_webhook.clone();

        let onchain = Arc::new(
            OnchainClient::new(config.solana_rpc_url.clone(), config.evm_rpc_urls.clone())
                .map_err(|e| anyhow::anyhow!("on-chain client: {e}"))?,
        );
        let holdings = match HoldingsService::new(
            Arc::clone(&onchain),
            config.wallet.price_feed_url.clone(),
        ) {
            Ok(service) => Some(Arc::new(service)),
            Err(e) => {
                tracing::warn!(error = %e, "wallet valuation is disabled: no price client");
                None
            }
        };
        let wallet = Arc::new(WalletService::new(config.wallet.clone()));
        wallet.spawn_key_reaper();

        let broadcaster = match HttpRpc::new() {
            Ok(transport) => {
                let broadcaster = Broadcaster::new(
                    Arc::new(transport),
                    config.evm_rpc_urls.clone(),
                    config.solana_rpc_url.clone(),
                    config.broadcast_max_fee_per_gas_wei,
                );
                if broadcaster.is_configured() {
                    tracing::info!(
                        networks = ?broadcaster.networks(),
                        "on-chain broadcast is configured"
                    );
                }
                Some(Arc::new(broadcaster))
            }
            Err(e) => {
                tracing::warn!(error = %e, "on-chain broadcast is disabled: no RPC client");
                None
            }
        };

        // Swaps need both a quote source and a way to send the transaction.
        let swaps = match (config.swap_quote_url.clone(), broadcaster.clone()) {
            (Some(template), Some(broadcaster)) => match HttpQuoter::new(template) {
                Ok(quoter) => Some(Arc::new(SwapService::new(Arc::new(quoter), broadcaster))),
                Err(e) => {
                    tracing::warn!(error = %e, "swaps are disabled: the quote source is unusable");
                    None
                }
            },
            _ => None,
        };

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
        let strategy_params = {
            let guard = strategy.lock().unwrap_or_else(|e| e.into_inner());
            match guard
                .as_ref()
                .and_then(|host| crate::strategies::find(&host.info().name))
            {
                Some(def) => crate::strategies::defaults(def.name),
                None => serde_json::json!({}),
            }
        };
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
            strategy_params: std::sync::Mutex::new(strategy_params),
            connectors,
            connector,
            onchain,
            wallet,
            broadcaster,
            holdings,
            swaps,
            ws_clients: std::sync::atomic::AtomicUsize::new(0),
            store,
            alerts: Arc::new(Alerter::new(webhook)),
            breaker_watch: std::sync::Mutex::new(BreakerWatch::default()),
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
                data.equity_anchored = true;
            }
            for p in &positions {
                data.position_qty.insert(p.symbol.clone(), p.qty);
                data.avg_entry.insert(p.symbol.clone(), p.avg_entry);
                data.position_venue.insert(p.symbol.clone(), p.venue);
                let lots: Vec<Lot> = if p.lots.is_empty() {
                    if p.qty.abs() > LOT_EPSILON {
                        vec![Lot {
                            qty: p.qty.abs(),
                            price: p.avg_entry,
                        }]
                    } else {
                        Vec::new()
                    }
                } else {
                    p.lots
                        .iter()
                        .map(|lot| Lot {
                            qty: lot.qty,
                            price: lot.price,
                        })
                        .collect()
                };
                data.lots.insert(p.symbol.clone(), lots);
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
            let primary = self.connector.venue();
            let restored_account = AccountRestore {
                cash_usd: p.cash_usd,
                realized_pnl: p.realized_pnl,
                // A venue only restores its own inventory. With one book per
                // symbol this is the whole book in every run that has a
                // restorable venue, but filtering keeps the two accounts from
                // ever disagreeing.
                positions: positions
                    .iter()
                    .filter(|p| p.venue == primary)
                    .map(|p| RestoredPosition {
                        symbol: p.symbol.clone(),
                        qty: p.qty,
                        avg_entry: p.avg_entry,
                        lots: p.lots.iter().map(|lot| (lot.qty, lot.price)).collect(),
                    })
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

        // The fill ladder comes back with the order, so a restart reconciles
        // against the venue without re-booking what the last run already
        // journaled.
        let tracked = store.load_tracked_orders().unwrap_or_default();
        let order_count = tracked.len();
        let cursor = store.load_fill_cursor().unwrap_or(0);
        // A transfer that was in flight when the process stopped is still in
        // flight on the chain: the reconciler picks it up from here instead of
        // the operator losing sight of it.
        let broadcasts = store.load_broadcasts().unwrap_or_default();
        let broadcast_count = broadcasts.len();
        {
            let mut data = self.lock_data();
            for order in tracked {
                data.orders.push_back(order);
            }
            data.fill_cursor_ms = cursor;
            for broadcast in broadcasts {
                data.broadcasts.push_back(broadcast);
            }
        }

        if restored_positions || portfolio.is_some() {
            tracing::info!(
                positions = positions.len(),
                trades = trade_count,
                orders = order_count,
                broadcasts = broadcast_count,
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
        let (portfolio, positions, orders, tracked, cursor) = {
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
                    .map(|p| PositionRow {
                        symbol: p.symbol.clone(),
                        // The account that is captured here is the primary
                        // venue's, so its positions belong to it.
                        venue: self.connector.venue(),
                        qty: p.qty,
                        avg_entry: p.avg_entry,
                        // A venue that reports lots keeps their FIFO order. One
                        // that only reports an average comes back as a single
                        // lot: accurate cost basis, no history to invent.
                        lots: if !p.lots.is_empty() {
                            p.lots
                                .iter()
                                .map(|(qty, price)| PositionLot {
                                    qty: *qty,
                                    price: *price,
                                })
                                .collect()
                        } else if p.qty.abs() > LOT_EPSILON {
                            vec![PositionLot {
                                qty: p.qty.abs(),
                                price: p.avg_entry,
                            }]
                        } else {
                            Vec::new()
                        },
                    })
                    .collect(),
                _ => data
                    .position_qty
                    .iter()
                    .filter(|(_, qty)| qty.abs() > 0.000_001)
                    .map(|(symbol, qty)| PositionRow {
                        symbol: symbol.clone(),
                        venue: data
                            .position_venue
                            .get(symbol)
                            .copied()
                            .unwrap_or_else(|| self.connector.venue()),
                        qty: *qty,
                        avg_entry: data.avg_entry.get(symbol).copied().unwrap_or(0.0),
                        lots: data
                            .lots
                            .get(symbol)
                            .map(|lots| {
                                lots.iter()
                                    .map(|lot| PositionLot {
                                        qty: lot.qty,
                                        price: lot.price,
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                    })
                    .collect(),
            };
            let orders = data.open_orders.clone();
            let tracked: Vec<TrackedOrder> = data.orders.iter().cloned().collect();
            let cursor = data.fill_cursor_ms;
            (portfolio, positions, orders, tracked, cursor)
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
        if let Err(e) = store.save_tracked_orders(&tracked) {
            tracing::warn!(error = %e, "could not persist the tracked orders");
        }
        if let Err(e) = store.save_fill_cursor(cursor) {
            tracing::warn!(error = %e, "could not persist the fill cursor");
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
                    venue: data
                        .position_venue
                        .get(symbol)
                        .copied()
                        .unwrap_or_else(|| self.venue()),
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
        let onchain_value_usd = data
            .wallet_holdings
            .as_ref()
            .map(|wallet| wallet.value_usd)
            .unwrap_or(0.0);
        let wallet_gas_usd = gas_spent_raw(&data)
            .iter()
            .map(|(chain, raw)| {
                let price = data
                    .wallet_holdings
                    .as_ref()
                    .and_then(|wallet| wallet.chains.iter().find(|c| &c.chain == chain))
                    .map(|chain| chain.native_price_usd)
                    .unwrap_or(0.0);
                gas_value(*raw, chain, price)
            })
            .sum();
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
            onchain_value_usd,
            wallet_gas_usd,
            positions,
            ts_ms: now_ms(),
        }
    }

    /// The last reading of the on-chain wallet.
    ///
    /// Before the first refresh completes there is no reading yet, but the
    /// wallet is still configured — reporting otherwise would tell the
    /// operator their keystore vanished.
    pub fn wallet_holdings(&self) -> WalletHoldings {
        self.lock_data()
            .wallet_holdings
            .clone()
            .unwrap_or_else(|| holdings::wallet_of(Vec::new(), self.wallet.is_configured()))
    }

    /// The transfers this core signed, newest first.
    pub fn broadcasts(&self, limit: usize) -> Vec<Broadcast> {
        let data = self.lock_data();
        let skip = data.broadcasts.len().saturating_sub(limit);
        data.broadcasts.iter().skip(skip).rev().cloned().collect()
    }

    /// Moves coins out of the wallet on a real chain.
    ///
    /// Every gate is checked before the wallet is asked for a signature:
    /// the on-chain arming record must be live, the wallet must not be in
    /// dry-run mode, the network must have an RPC url, and the move must be
    /// worth less than the armed per-transfer cap — priced at the current
    /// gas-token price, so a balance cannot be moved with an old exchange
    /// rate. There is no way to skip any of them from this path.
    pub async fn broadcast_transfer(
        &self,
        network: &str,
        to: &str,
        amount_raw: u128,
        max_fee_per_gas_wei: Option<u128>,
        passphrase: &str,
        account: AccountSelector,
    ) -> Result<TransferReceipt, TransferError> {
        if !self.config.onchain_live {
            return Err(TransferError::NotArmed(
                self.config
                    .onchain_arming
                    .reason
                    .unwrap_or("no valid on-chain arming record")
                    .to_string(),
            ));
        }
        if self.wallet.dry_run() {
            return Err(TransferError::DryRun);
        }
        let broadcaster = self
            .broadcaster
            .as_ref()
            .ok_or(TransferError::NotConfigured)?;
        let network = network.trim().to_ascii_lowercase();
        let decimals = native_decimals(&network)
            .ok_or_else(|| TransferError::UnsupportedNetwork(network.clone()))?;
        if !broadcaster.networks().iter().any(|n| n == &network) {
            return Err(TransferError::NetworkNotConfigured(network));
        }

        let service = self
            .holdings
            .as_ref()
            .ok_or_else(|| TransferError::Unpriced(network.clone()))?;
        let price = service
            .native_price_usd(&network)
            .await
            .ok_or_else(|| TransferError::Unpriced(network.clone()))?;
        let notional_usd = amount_raw as f64 / 10f64.powi(decimals as i32) * price;
        self.metrics
            .counter_add("openether_wallet_transfer_notional_usd", &[], notional_usd);
        let cap = self.config.onchain_arming.effective_max_order_notional_usd;
        if !notional_usd.is_finite() || notional_usd > cap {
            return Err(TransferError::AboveCap(notional_usd, network, cap));
        }

        let signer = WalletSigner {
            wallet: &self.wallet,
            passphrase,
            selector: account,
        };
        let request = TransferRequest {
            network,
            to: to.to_string(),
            amount_raw,
            data: Vec::new(),
            max_fee_per_gas_wei,
        };
        let mut broadcast = broadcaster.transfer(&request, &signer).await?;
        broadcast.updated_at_ms = now_ms();
        self.log(
            "wallet",
            "info",
            &format!(
                "sent a {} transfer of {} {} for ${:.2}",
                broadcast.chain, broadcast.amount_raw, broadcast.chain, notional_usd
            ),
            serde_json::json!({
                "tx_hash": broadcast.tx_hash,
                "from": broadcast.from,
                "to": broadcast.to,
                "amount_raw": broadcast.amount_raw,
                "notional_usd": notional_usd,
            }),
        );
        self.metrics.counter_add(
            "openether_wallet_broadcasts_total",
            &[
                ("chain", broadcast.chain.as_str()),
                ("state", broadcast.state.as_str()),
            ],
            1.0,
        );
        self.store_broadcast(broadcast.clone());
        Ok(TransferReceipt {
            broadcast,
            notional_usd,
        })
    }

    /// Quotes a swap and checks everything the core can check without a
    /// signature: the on-chain gates, the contract allowlist, the wallet's
    /// balance, the spender's allowance and the armed per-trade cap.
    ///
    /// Nothing here signs. The quote is fetched fresh on every call, and
    /// [`Self::execute_swap`] quotes again rather than replaying a stored one.
    pub async fn quote_swap(
        &self,
        request: &SwapRequest,
        account: &AccountSelector,
    ) -> Result<SwapPlan, SwapExecutionError> {
        let swaps = self.require_swaps()?;
        let from = self.evm_account(account)?;
        let quote = swaps
            .quote(request, &from, &self.config.contract_allowlist)
            .await?;
        let notional_usd = self.swap_notional_usd(&quote).await?;
        let cap = self.config.onchain_arming.effective_max_order_notional_usd;
        if !notional_usd.is_finite() || notional_usd > cap {
            return Err(SwapExecutionError::AboveCap(
                notional_usd,
                quote.sell_token.clone(),
                cap,
            ));
        }
        let approval_required = quote
            .allowance
            .as_ref()
            .map(|allowance| !allowance.sufficient)
            .unwrap_or(false);
        Ok(SwapPlan {
            quote,
            notional_usd,
            from,
            approval_required,
        })
    }

    /// Quotes, checks and sends a swap.
    ///
    /// The transaction signs the calldata of the quote taken in this call, for
    /// the router on the contract allowlist, behind the same gates as a
    /// transfer. A token sale whose allowance is short is refused with the
    /// spender to approve; the core never sends an unlimited approval on its
    /// own.
    pub async fn execute_swap(
        &self,
        request: &SwapRequest,
        max_fee_per_gas_wei: Option<u128>,
        passphrase: &str,
        account: AccountSelector,
    ) -> Result<SwapReceipt, SwapExecutionError> {
        let plan = self.quote_swap(request, &account).await?;
        if let Some(allowance) = plan.quote.allowance.as_ref().filter(|a| !a.sufficient) {
            return Err(SwapExecutionError::ApprovalRequired {
                spender: allowance.spender.clone(),
                current_raw: allowance.current_raw,
                token: allowance.token.clone(),
                required_raw: allowance.required_raw,
            });
        }
        let broadcaster = self
            .broadcaster
            .as_ref()
            .ok_or(SwapExecutionError::NotConfigured)?;
        let data = plan.quote.call_data()?;
        let signer = WalletSigner {
            wallet: &self.wallet,
            passphrase,
            selector: account,
        };
        let transfer = TransferRequest {
            network: plan.quote.network.clone(),
            to: plan.quote.to.clone(),
            amount_raw: plan.quote.value_raw,
            data,
            max_fee_per_gas_wei,
        };
        let mut broadcast = broadcaster.transfer(&transfer, &signer).await?;
        broadcast.updated_at_ms = now_ms();
        self.metrics.counter_add(
            "openether_swap_notional_usd",
            &[("network", broadcast.chain.as_str())],
            plan.notional_usd,
        );
        self.metrics.counter_add(
            "openether_wallet_broadcasts_total",
            &[
                ("chain", broadcast.chain.as_str()),
                ("state", broadcast.state.as_str()),
            ],
            1.0,
        );
        self.log(
            "swap",
            "info",
            &format!(
                "sent a {} swap of {} for {} worth ${:.2} through {}",
                broadcast.chain,
                plan.quote.sell_token,
                plan.quote.buy_token,
                plan.notional_usd,
                plan.quote.to
            ),
            serde_json::json!({
                "tx_hash": broadcast.tx_hash,
                "router": plan.quote.to,
                "sell_token": plan.quote.sell_token,
                "sell_amount_raw": plan.quote.sell_amount_raw,
                "buy_token": plan.quote.buy_token,
                "buy_amount_raw": plan.quote.buy_amount_raw,
                "notional_usd": plan.notional_usd,
                "source": plan.quote.source,
            }),
        );
        self.store_broadcast(broadcast.clone());
        Ok(SwapReceipt {
            broadcast,
            quote: plan.quote,
            notional_usd: plan.notional_usd,
        })
    }

    /// Sends an exact ERC-20 approval for the spender a quote named.
    ///
    /// The spender must be on the contract allowlist; the amount is exactly
    /// what the caller asks for, so the core never signs an unlimited
    /// approval. An approval moves no tokens, so it is logged and gated like a
    /// transfer but carries no notional.
    pub async fn approve_token(
        &self,
        approval: TokenApproval,
        passphrase: &str,
        account: AccountSelector,
    ) -> Result<TransferReceipt, SwapExecutionError> {
        self.require_swaps()?;
        let broadcaster = self
            .broadcaster
            .as_ref()
            .ok_or(SwapExecutionError::NotConfigured)?;
        let network = approval.network.trim().to_ascii_lowercase();
        if broadcaster.evm_chain_id(&network).is_none() {
            return Err(SwapExecutionError::Swap(SwapError::NetworkUnsupported(
                network,
                "it has no EVM RPC url configured".into(),
            )));
        }
        if approval.amount_raw == 0 {
            return Err(SwapExecutionError::Swap(SwapError::InvalidRequest(
                "the allowance must be greater than zero".into(),
            )));
        }
        let token = crate::swap::normalize_token(&approval.token)?;
        if token == crate::swap::NATIVE_TOKEN {
            return Err(SwapExecutionError::Swap(SwapError::InvalidRequest(
                "the native coin needs no allowance".into(),
            )));
        }
        let spender = crate::swap::normalize_address(&approval.spender)?;
        crate::swap::enforce_allowlist(&spender, None, &self.config.contract_allowlist)?;
        let amount_raw = approval.amount_raw;
        let data = encode_approve(&spender, amount_raw)?;
        let signer = WalletSigner {
            wallet: &self.wallet,
            passphrase,
            selector: account,
        };
        let request = TransferRequest {
            network: network.clone(),
            to: token.clone(),
            amount_raw: 0,
            data,
            max_fee_per_gas_wei: approval.max_fee_per_gas_wei,
        };
        let mut broadcast = broadcaster.transfer(&request, &signer).await?;
        broadcast.updated_at_ms = now_ms();
        self.metrics.counter_add(
            "openether_wallet_broadcasts_total",
            &[
                ("chain", broadcast.chain.as_str()),
                ("state", broadcast.state.as_str()),
            ],
            1.0,
        );
        self.log(
            "swap",
            "info",
            &format!(
                "approved {spender} to move up to {amount_raw} raw units of {token} on {network}"
            ),
            serde_json::json!({
                "tx_hash": broadcast.tx_hash,
                "token": token,
                "spender": spender,
                "amount_raw": amount_raw,
            }),
        );
        self.store_broadcast(broadcast.clone());
        Ok(TransferReceipt {
            broadcast,
            notional_usd: 0.0,
        })
    }

    /// The state of the swap path, for the dashboard and the agent.
    pub fn swap_status(&self) -> serde_json::Value {
        let mut allowlist: Vec<String> = self.config.contract_allowlist.iter().cloned().collect();
        allowlist.sort();
        serde_json::json!({
            "enabled": self.swaps.is_some(),
            "source": self.swaps.as_ref().map(|swaps| swaps.source()),
            "networks": self
                .broadcaster
                .as_ref()
                .map(|broadcaster| broadcaster.networks())
                .unwrap_or_default(),
            "contract_allowlist": allowlist,
            "default_slippage_bps": crate::swap::DEFAULT_SLIPPAGE_BPS,
            "max_slippage_bps": crate::swap::MAX_SLIPPAGE_BPS,
            "onchain_live": self.config.onchain_live,
            "dry_run": self.config.wallet.dry_run,
            "effective_max_swap_notional_usd": self
                .config
                .onchain_arming
                .effective_max_order_notional_usd,
        })
    }

    fn require_swaps(&self) -> Result<&Arc<SwapService>, SwapExecutionError> {
        if !self.config.onchain_live {
            return Err(SwapExecutionError::NotArmed(
                self.config
                    .onchain_arming
                    .reason
                    .unwrap_or("no valid on-chain arming record")
                    .to_string(),
            ));
        }
        if self.wallet.dry_run() {
            return Err(SwapExecutionError::DryRun);
        }
        self.swaps.as_ref().ok_or(SwapExecutionError::NoQuoter)
    }

    fn evm_account(&self, selector: &AccountSelector) -> Result<String, SwapExecutionError> {
        if let Some(chain) = selector.chain.as_deref() {
            if crate::onchain::parse_chain(chain) != Some(Chain::Evm) {
                return Err(SwapExecutionError::Swap(SwapError::InvalidRequest(
                    "a swap is signed by an EVM account".into(),
                )));
            }
        }
        let account = self
            .wallet
            .address_for(Chain::Evm, selector.index.unwrap_or(0))?;
        Ok(account.address)
    }

    async fn swap_notional_usd(&self, quote: &SwapQuote) -> Result<f64, SwapExecutionError> {
        let service = self
            .holdings
            .as_ref()
            .ok_or_else(|| SwapExecutionError::Unpriced(quote.sell_token.clone()))?;
        let price = if quote.sell_token == crate::swap::NATIVE_TOKEN {
            service.native_price_usd(&quote.network).await
        } else {
            service
                .token_price_usd(&quote.network, &quote.sell_token)
                .await
        }
        .ok_or_else(|| SwapExecutionError::Unpriced(quote.sell_token.clone()))?;
        Ok(quote.sell_amount_raw as f64 / 10f64.powi(quote.sell_decimals as i32) * price)
    }

    /// Re-reads every transfer that has not reached a final state.
    ///
    /// A terminal transfer is never re-read, so a confirmed fee cannot be
    /// booked twice. When one reaches a terminal state the wallet is revalued
    /// once, because a confirmed transfer has moved coins.
    pub async fn reconcile_broadcasts(&self) {
        let Some(broadcaster) = self.broadcaster.as_ref() else {
            return;
        };
        let pending: Vec<Broadcast> = {
            let data = self.lock_data();
            data.broadcasts
                .iter()
                .filter(|b| !b.state.is_terminal())
                .cloned()
                .collect()
        };
        let mut reached_finality = false;
        for mut broadcast in pending {
            let before = broadcast.state;
            match broadcaster.refresh(&mut broadcast).await {
                Ok(()) if broadcast.state != before => {
                    broadcast.updated_at_ms = now_ms();
                    let level = if broadcast.state == BroadcastState::Failed {
                        "warn"
                    } else {
                        "info"
                    };
                    self.log(
                        "wallet",
                        level,
                        &format!(
                            "transfer {} is now {}: {}",
                            broadcast.tx_hash,
                            broadcast.state.as_str(),
                            broadcast.note
                        ),
                        serde_json::json!({
                            "tx_hash": broadcast.tx_hash,
                            "state": broadcast.state.as_str(),
                            "confirmations": broadcast.confirmations,
                            "fee_paid_raw": broadcast.fee_paid_raw,
                        }),
                    );
                    self.metrics.counter_add(
                        "openether_wallet_broadcasts_total",
                        &[
                            ("chain", broadcast.chain.as_str()),
                            ("state", broadcast.state.as_str()),
                        ],
                        1.0,
                    );
                    reached_finality |= broadcast.state.is_terminal();
                    self.store_broadcast(broadcast);
                }
                Ok(()) => {}
                Err(e) => {
                    tracing::warn!(
                        tx = %broadcast.tx_hash,
                        error = %e,
                        "could not refresh a broadcast"
                    );
                }
            }
        }
        if reached_finality {
            self.refresh_wallet_holdings().await;
        }
    }

    fn store_broadcast(&self, broadcast: Broadcast) {
        if let Some(store) = self.store.as_ref() {
            if let Err(e) = store.save_broadcast(&broadcast) {
                tracing::warn!(error = %e, "could not persist a broadcast");
            }
        }
        let mut data = self.lock_data();
        if let Some(existing) = data.broadcasts.iter_mut().find(|b| b.id == broadcast.id) {
            *existing = broadcast;
            return;
        }
        data.broadcasts.push_back(broadcast);
        while data.broadcasts.len() > MAX_BROADCASTS {
            data.broadcasts.pop_front();
        }
    }

    /// Reads and values the wallet.
    ///
    /// Balances are public data and the addresses come from the keystore's
    /// public header, so no key is ever unlocked here.
    pub async fn refresh_wallet_holdings(&self) {
        let Some(service) = self.holdings.as_ref() else {
            return;
        };
        let configured = self.wallet.is_configured();
        let accounts = holdings::watched_accounts(&self.wallet.metadata());
        if accounts.is_empty() {
            self.lock_data().wallet_holdings = Some(holdings::wallet_of(Vec::new(), configured));
            return;
        }

        let watchlist = &self.config.wallet.evm_token_watchlist;
        let evm_chains: Vec<String> = self
            .config
            .evm_rpc_urls
            .iter()
            .map(|(name, _)| name.clone())
            .collect();
        let mut chains = Vec::new();
        for (chain, address) in accounts {
            let targets: Vec<String> = if chain == "evm" {
                // An EVM address is valid on every EVM chain the operator
                // configured, so the wallet is valued on each of them.
                evm_chains.clone()
            } else {
                vec![chain]
            };
            for target in targets {
                let reading = if target == "solana" {
                    service.solana(&address).await
                } else {
                    service.evm(&target, &address, watchlist).await
                };
                match reading {
                    Ok(holding) => chains.push(holding),
                    Err(e) => {
                        self.log(
                            "wallet",
                            "warn",
                            &format!("could not value {target}: {e}"),
                            serde_json::json!({"chain": target, "error": e.to_string()}),
                        );
                    }
                }
            }
        }

        // Gas is booked from the broadcasts, which survive a restart, and
        // priced with the reading just taken. A chain with no reading does not
        // invent a price.
        let gas = gas_spent_raw(&self.lock_data());
        for holding in chains.iter_mut() {
            let raw = gas.get(&holding.chain).copied().unwrap_or(0);
            holding.gas_spent_usd = gas_value(raw, &holding.chain, holding.native_price_usd);
        }

        let wallet = holdings::wallet_of(chains, configured);
        self.metrics
            .gauge_set("openether_wallet_value_usd", &[], wallet.value_usd);
        self.lock_data().wallet_holdings = Some(wallet);
    }

    pub fn risk_state(&self) -> RiskState {
        self.risk.snapshot(self.equity(), now_ms())
    }

    /// Push a breaker opening to the webhook, once.
    ///
    /// Called wherever the breaker can be observed to trip. Orders refused
    /// while it is already open pass through here too, but only the transition
    /// is news.
    fn note_breaker(&self, state: crate::risk::BreakerState) {
        let opened = {
            let mut watch = self
                .breaker_watch
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            watch.note(state)
        };
        if !opened {
            return;
        }
        let risk = self.risk_state();
        self.alerts
            .send(self.alerts.breaker_opened(&risk, now_ms()));
    }

    /// Tell the operator that a human reset the halt.
    pub fn note_breaker_reset(&self) {
        self.alerts.send(self.alerts.breaker_reset(now_ms()));
    }

    /// The connector for a venue, when the venue is configured.
    pub fn connector_for(&self, venue: Venue) -> Option<Arc<dyn ExchangeConnector>> {
        self.connectors
            .iter()
            .find(|connector| connector.venue() == venue)
            .cloned()
    }

    /// Pick the venue an order goes to.
    ///
    /// Four rules, in order:
    ///
    /// * **An explicit venue is a constraint.** When the request names a venue
    ///   — and it is not the neutral `paper` placeholder the simulated book
    ///   uses — only that venue is considered. Asking for a venue that cannot
    ///   trade the symbol is refused, never quietly rerouted.
    /// * **Inventory pins the venue.** A symbol that already has a position
    ///   stays on the venue that holds it. That venue is where the position can
    ///   be closed and where its balances live; opening the other side on a
    ///   different exchange would make the engine's one book look flat while
    ///   leaving a naked position on each venue.
    /// * **Only a venue that can place the order is a candidate.** A live venue
    ///   without a valid arming record is skipped, and when nothing is left the
    ///   order is refused rather than quietly simulated. The paper venue is a
    ///   simulation and needs no arming; a venue with no order flow in this
    ///   build (Hyperliquid) is never chosen at all.
    /// * **The fee-adjusted price decides.** A buy wants the lowest cost after
    ///   the venue's taker fee, a sell the highest proceeds. A tie keeps the
    ///   configured preference order.
    ///
    /// One candidate is not quoted twice: it is simply used, at the engine's
    /// usual price for the symbol.
    async fn route(
        &self,
        symbol: &str,
        side: Side,
        requested: Option<Venue>,
    ) -> Result<Route, RouteRefusal> {
        let holder = {
            let data = self.lock_data();
            let held = data.position_qty.get(symbol).copied().unwrap_or(0.0).abs() > LOT_EPSILON;
            if held {
                data.position_venue.get(symbol).copied()
            } else {
                None
            }
        };

        let candidates: Vec<Arc<dyn ExchangeConnector>> = self
            .connectors
            .iter()
            .filter(|connector| holder.is_none_or(|venue| connector.venue() == venue))
            .filter(|connector| requested.is_none_or(|venue| connector.venue() == venue))
            // A venue that cannot place an order in this build (Hyperliquid)
            // is not a candidate: routing to it would only turn an approved
            // order into a `venue_error` after the risk layer said yes.
            .filter(|connector| {
                let venue = connector.venue();
                venue == Venue::Paper || venue.trades_live()
            })
            .cloned()
            .collect();
        if candidates.is_empty() {
            let detail = match (holder, requested) {
                (Some(held), Some(asked)) => format!(
                    "the position in {symbol} is held on {held}; its orders must go there, not to {asked}"
                ),
                (None, Some(asked)) => format!(
                    "{asked} cannot receive an order for {symbol}: it is not a venue that places orders in this build"
                ),
                (Some(held), None) => format!(
                    "the position in {symbol} is held on {held}, which is not configured"
                ),
                (None, None) => format!("no venue can trade {symbol}"),
            };
            return Err(RouteRefusal {
                reason: "venue_unavailable",
                detail,
            });
        }

        let armed: Vec<Arc<dyn ExchangeConnector>> = candidates
            .into_iter()
            .filter(|connector| self.config.is_live(connector.venue()))
            .collect();
        if armed.is_empty() {
            return Err(RouteRefusal {
                reason: "venue_not_armed",
                detail: match requested {
                    Some(venue) => format!(
                        "{venue} may not trade {symbol}: its arming record is not valid. Run `openether-live arm --venue {venue}`."
                    ),
                    None => format!(
                        "no venue that may trade {symbol} is armed: {}. Run `openether-live arm`.",
                        holder
                            .map(|venue| format!(
                                "{venue} holds it and its arming record is not valid"
                            ))
                            .unwrap_or_else(|| "every configured live venue is unarmed".to_string())
                    ),
                },
            });
        }

        if armed.len() == 1 {
            let connector = armed[0].clone();
            let venue = connector.venue();
            let price = match connector.last_price(symbol).await {
                Ok(price) if price > 0.0 => price,
                _ => self
                    .resolve_price(symbol)
                    .await
                    .ok_or_else(|| RouteRefusal {
                        reason: "price_unavailable",
                        detail: format!("no market price available for {symbol}"),
                    })?,
            };
            return Ok(Route {
                connector,
                venue,
                price,
                compared: Vec::new(),
            });
        }

        let mut quotes: Vec<(Venue, f64, f64, Arc<dyn ExchangeConnector>)> = Vec::new();
        for connector in &armed {
            let venue = connector.venue();
            match connector.last_price(symbol).await {
                Ok(price) if price > 0.0 => {
                    let fee = connector.taker_fee_bps() / 10_000.0;
                    let net = match side {
                        Side::Buy => price * (1.0 + fee),
                        Side::Sell => price * (1.0 - fee),
                    };
                    quotes.push((venue, price, net, connector.clone()));
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(
                    venue = venue.as_str(),
                    symbol,
                    error = %error,
                    "venue could not quote; excluded from routing"
                ),
            }
        }
        if quotes.is_empty() {
            return Err(RouteRefusal {
                reason: "price_unavailable",
                detail: format!("no configured venue quoted {symbol}"),
            });
        }
        quotes.sort_by(|a, b| match side {
            Side::Buy => a.2.total_cmp(&b.2),
            Side::Sell => b.2.total_cmp(&a.2),
        });
        let compared: Vec<(Venue, f64)> = quotes.iter().map(|quote| (quote.0, quote.2)).collect();
        let (venue, price, _, connector) = quotes.remove(0);
        Ok(Route {
            connector,
            venue,
            price,
            compared,
        })
    }

    /// The single entry point for placing an order.
    ///
    /// Every path — the agent through MCP, the WASM strategy, the HTTP API —
    /// funnels through here, and the risk layer is evaluated before the venue is
    /// ever contacted. There is no bypass.
    pub async fn submit_order(&self, mut req: OrderRequest, source: &str) -> SubmitOutcome {
        let symbol = req.symbol.clone();
        // `paper` is the neutral placeholder of the simulated book, so it does
        // not constrain the router; naming a live venue does.
        let requested = (req.venue != Venue::Paper).then_some(req.venue);
        let route = match self.route(&symbol, req.side, requested).await {
            Ok(route) => route,
            Err(refusal) => {
                let verdict = RiskVerdict {
                    approved: false,
                    reason: refusal.reason.into(),
                    detail: refusal.detail.clone(),
                    breaker_state: self.risk.breaker_state(),
                    checked_at_ms: now_ms(),
                };
                self.log(
                    source,
                    "warn",
                    &format!("order rejected: {}", verdict.detail),
                    serde_json::json!({"symbol": symbol, "reason": verdict.reason}),
                );
                return SubmitOutcome {
                    approved: false,
                    reason: verdict.reason.clone(),
                    detail: verdict.detail.clone(),
                    order_id: None,
                    venue: None,
                    status: None,
                    fill_price: None,
                    fee: None,
                    risk: verdict,
                };
            }
        };
        // The connector never decides which venue it is: the router does. The
        // risk layer then judges the order against the venue it would reach.
        req.venue = route.venue;
        let price = route.price;

        let context = RiskContext {
            equity_usd: self.equity(),
            gross_exposure_usd: self.risk.gross_exposure(),
            open_positions: self.risk.open_positions(),
            current_position_qty: self.risk.position_qty(&symbol),
            price,
            contracts: Vec::new(),
            // The armed cap belongs to the venue the order was routed to, not
            // to the process: a tight Kraken arming must not size a Binance
            // order, and a loose one must not widen Binance's.
            max_order_notional_usd: Some(self.config.armed_cap(route.venue)),
        };

        let verdict = self.risk.evaluate(&req, &context);
        self.note_breaker(verdict.breaker_state);
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
                venue: Some(route.venue),
                status: None,
                fill_price: None,
                fee: None,
                risk: verdict,
            };
        }

        let before = now_ms();
        let ack = match route.connector.place_order(&req).await {
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
                    venue: Some(route.venue),
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

        let mut order = TrackedOrder {
            venue: req.venue,
            order_id: ack.order_id.clone(),
            client_id: req.client_id.clone(),
            symbol: symbol.clone(),
            side: req.side,
            order_type: req.order_type,
            limit_price: req.limit_price,
            trigger_price: req.trigger_price,
            oco_group: req.oco_group.clone(),
            qty: req.qty,
            status: ack.status,
            rationale: req.rationale.clone(),
            submitted_ts_ms: ack.accepted_ts_ms,
            updated_ts_ms: now_ms(),
            fills: Vec::new(),
            missing_since_ms: None,
        };

        // Only the venue's own fills are booked, and only this order's. A
        // resting limit order has filled nothing, so it must not become a
        // position: the risk layer would then measure exposure that does not
        // exist, and the journal would record a trade that never happened.
        let fills = route
            .connector
            .fills_since(before - 1)
            .await
            .unwrap_or_default();
        for fill in fills {
            if fill.order_id != ack.order_id {
                continue;
            }
            self.book_fill(&mut order, &fill);
        }

        let has_fill = !order.fills.is_empty();
        let fill_price = has_fill.then(|| order.avg_fill_price());
        let fee = has_fill.then(|| order.fee());
        let status = order.status;
        self.remember_order(order);

        self.log(
            source,
            "info",
            &format!(
                "order {}: {} {} {} @ {}",
                status.as_str(),
                req.side,
                req.qty,
                symbol,
                fill_price.unwrap_or(price)
            ),
            serde_json::json!({
                "symbol": symbol,
                "side": req.side,
                "qty": req.qty,
                "status": status,
                "price": fill_price,
                "rationale": req.rationale,
                "order_id": ack.order_id,
                "venue": route.venue.as_str(),
                "compared": route
                    .compared
                    .iter()
                    .map(|(venue, net)| serde_json::json!({
                        "venue": venue.as_str(),
                        "net_price": net,
                    }))
                    .collect::<Vec<_>>(),
            }),
        );

        SubmitOutcome {
            approved: true,
            reason: verdict.reason.clone(),
            detail: verdict.detail.clone(),
            order_id: Some(ack.order_id),
            venue: Some(route.venue),
            status: Some(status),
            fill_price,
            fee,
            risk: verdict,
        }
    }

    /// Book one venue-confirmed fill against an order and the account.
    ///
    /// Idempotent: reconciliation re-reads an overlapping window, so the same
    /// fill arrives more than once and must only ever be booked once.
    fn book_fill(&self, order: &mut TrackedOrder, fill: &OrderFill) {
        if order.knows_fill(fill) {
            return;
        }
        order.fills.push(fill.clone());
        order.updated_ts_ms = now_ms();
        order.status = if order.remaining_qty() <= 1e-9 {
            OrderStatus::Filled
        } else {
            OrderStatus::PartiallyFilled
        };
        self.apply_fill(fill, &order.rationale);
        self.metrics.counter_add(
            "openether_orders_filled_total",
            &[("venue", fill.venue.as_str())],
            1.0,
        );
    }

    /// Keep a submitted order, bounded, and write it to disk.
    fn remember_order(&self, order: TrackedOrder) {
        {
            let mut data = self.lock_data();
            if let Some(existing) = data
                .orders
                .iter_mut()
                .find(|known| known.order_id == order.order_id)
            {
                *existing = order;
            } else {
                if data.orders.len() >= MAX_ORDERS {
                    data.orders.pop_front();
                }
                data.orders.push_back(order);
            }
        }
        self.persist();
    }

    /// The orders the engine has submitted, newest first.
    pub fn orders(&self) -> Vec<TrackedOrder> {
        self.lock_data().orders.iter().rev().cloned().collect()
    }

    /// Cancel a resting order at the venue.
    ///
    /// Cancelling only ever reduces exposure, so it does not go through the
    /// risk layer. The venue has to confirm: an order it will not cancel stays
    /// active and keeps being reconciled. A fill that landed just before the
    /// cancel is still booked, because reconciliation books whatever the venue
    /// reports regardless of the label the engine last gave the order.
    pub async fn cancel_order(&self, order_id: &str) -> Result<(), String> {
        let known = self
            .lock_data()
            .orders
            .iter()
            .find(|order| order.order_id == order_id)
            .cloned();
        let Some(order) = known else {
            return Err(format!("unknown order {order_id}"));
        };
        if !order.is_active() {
            return Err(format!(
                "order {order_id} is {}: only a resting order can be cancelled",
                order.status.as_str()
            ));
        }
        let connector = self.connector_for(order.venue).ok_or_else(|| {
            format!(
                "order {order_id} lives on {}, which is not a configured venue",
                order.venue
            )
        })?;
        connector
            .cancel_order(order_id)
            .await
            .map_err(|e| e.to_string())?;

        {
            let mut data = self.lock_data();
            if let Some(slot) = data
                .orders
                .iter_mut()
                .find(|known| known.order_id == order_id)
            {
                slot.status = OrderStatus::Cancelled;
                slot.updated_ts_ms = now_ms();
            }
            data.open_orders.retain(|open| open.order_id != order_id);
        }
        self.log(
            "engine",
            "info",
            &format!("order {order_id} cancelled at the venue"),
            serde_json::json!({
                "order_id": order_id,
                "filled_qty": order.filled_qty(),
            }),
        );
        self.persist();
        Ok(())
    }

    fn apply_fill(&self, fill: &OrderFill, rationale: &str) {
        let signed_qty = fill.side.sign() * fill.qty;
        let mut data = self.lock_data();
        let symbol = fill.symbol.clone();
        let prev_qty = data.position_qty.get(&symbol).copied().unwrap_or(0.0);
        let prev_avg = data.avg_entry.get(&symbol).copied().unwrap_or(0.0);
        // The mark stays the market's, not the fill's: the venue values the
        // position at the market price, and slippage is a cost, not a mark.
        let prev_mark = data.last_prices.get(&symbol).copied().unwrap_or(fill.price);
        let new_qty = prev_qty + signed_qty;

        // FIFO: the part of the fill that closes the old side consumes the
        // oldest lots; anything left over opens at this fill's price. A fill
        // that crosses zero does both.
        let mut lots = data.lots.remove(&symbol).unwrap_or_default();
        let mut realized = 0.0;
        if prev_qty != 0.0 && (prev_qty > 0.0) != (signed_qty > 0.0) {
            let closing = prev_qty.abs().min(fill.qty.abs());
            let direction = if prev_qty > 0.0 { 1.0 } else { -1.0 };
            realized = consume_lots(&mut lots, closing, fill.price, direction);
            let opening = fill.qty.abs() - closing;
            if opening > LOT_EPSILON {
                lots.clear();
                lots.push(Lot {
                    qty: opening,
                    price: fill.price,
                });
            }
        } else {
            lots.push(Lot {
                qty: fill.qty.abs(),
                price: fill.price,
            });
        }
        if new_qty.abs() <= LOT_EPSILON {
            lots.clear();
        }
        let new_avg = lots_average(&lots);
        data.realized_pnl += realized;
        data.position_qty.insert(symbol.clone(), new_qty);
        data.avg_entry.insert(symbol.clone(), new_avg);
        data.lots.insert(symbol.clone(), lots);
        // The venue that filled the opening order holds the position until it
        // is flat again. Routing reads this to keep every later order — above
        // all the close — on that venue.
        if new_qty.abs() > LOT_EPSILON {
            data.position_venue.insert(symbol.clone(), fill.venue);
        } else {
            data.position_venue.remove(&symbol);
        }
        // A first fill has no mark to keep; it seeds one.
        data.last_prices
            .entry(fill.symbol.clone())
            .or_insert(fill.price);
        // Equity is cash plus the book at its marks. A fill realizes PnL that
        // was already marked in, charges a fee, and re-bases the position, so
        // the snapshot only moves by the fee and by any gap between the mark
        // and the fill.
        let old_unrealized = prev_qty * (prev_mark - prev_avg);
        let new_unrealized = new_qty * (prev_mark - new_avg);
        data.equity_usd += realized - fill.fee + new_unrealized - old_unrealized;

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
            rationale: rationale.to_string(),
            source: if rationale.is_empty() {
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
        // The book is marked by the venue that holds the position it belongs
        // to. With several venues streaming, the same symbol arrives at two
        // prices; only the holder's is the price that position could be
        // closed at, and mixing both into the return history would corrupt
        // the volatility and correlation estimates.
        let (equity, anchored, governs) = {
            let mut data = self.lock_data();
            let qty = data
                .position_qty
                .get(&tick.symbol)
                .copied()
                .unwrap_or(0.0)
                .abs();
            let holder = data.position_venue.get(&tick.symbol).copied();
            let governs = qty <= LOT_EPSILON || holder.is_none() || holder == Some(tick.venue);
            if governs {
                let previous = data.last_prices.insert(tick.symbol.clone(), tick.price);
                // The snapshot renders the position rows at this mark, so the
                // headline equity has to move with them. Without this, one
                // response shows a position up $20 next to an unchanged equity.
                if let Some(previous) = previous {
                    if previous > 0.0 && previous != tick.price {
                        let held = data.position_qty.get(&tick.symbol).copied().unwrap_or(0.0);
                        data.equity_usd += (tick.price - previous) * held;
                    }
                }
            }
            if data.ticks.len() >= MAX_TICKS {
                data.ticks.pop_front();
            }
            data.ticks.push_back(tick.clone());
            (data.equity_usd, data.equity_anchored, governs)
        };
        if governs {
            // The risk layer keeps the return history itself: volatility sizing
            // and the correlation cap are risk inputs, not dashboard data.
            self.risk
                .observe_price(&tick.symbol, tick.price, tick.ts_ms);
        }
        // The breaker watches the marked book, not only the periodic account
        // refresh: a mark that halves between refreshes is exactly the
        // drawdown the halt exists for. Before the venue has been heard from,
        // the starting balance is a guess and must not anchor a peak.
        if anchored && governs {
            self.risk.update_equity(equity, tick.ts_ms);
            self.note_breaker(self.risk.breaker_state());
        }
        self.bus.publish(Event::Tick(tick));
    }

    /// Re-anchor the account from every connected venue.
    ///
    /// The sum is only taken when every venue answered. A partial sum would
    /// understate equity — and an equity that is wrong by a whole exchange is
    /// worse than one that is a few seconds stale.
    pub async fn refresh_account(&self) {
        let mut equity = 0.0;
        let mut cash = 0.0;
        let mut ts_ms = 0;
        let mut complete = true;
        for connector in &self.connectors {
            match connector.account().await {
                Ok(account) => {
                    equity += account.equity_usd;
                    cash += account.free_usd;
                    ts_ms = ts_ms.max(account.ts_ms);
                }
                Err(error) => {
                    complete = false;
                    tracing::warn!(
                        venue = connector.venue().as_str(),
                        error = %error,
                        "account refresh failed for one venue; keeping the marked book"
                    );
                }
            }
        }
        if !complete {
            return;
        }
        {
            let mut data = self.lock_data();
            data.equity_usd = equity;
            data.cash_usd = cash;
            data.equity_anchored = true;
        }
        self.risk.update_equity(equity, ts_ms);
        self.note_breaker(self.risk.breaker_state());
        self.metrics
            .gauge_set("openether_pnl_usd", &[], self.risk.daily_realized_pnl());
        let portfolio = self.portfolio();
        self.bus.publish(Event::Portfolio(portfolio));
        let risk_state = self.risk_state();
        self.bus.publish(Event::Risk(risk_state));
    }

    /// Join the venue's two truths: the fills it reports and the orders it
    /// still holds open.
    ///
    /// A fill books part of an order. An order that has left the open list
    /// without filling the rest is gone — cancelled, expired or rejected — and
    /// leaving it "accepted" forever would have the engine wait on a trade that
    /// can no longer happen.
    pub async fn reconcile_orders(&self) {
        // Keep reading fills for as long as one could still land: an order that
        // was just cancelled or retired can still have a fill the venue has not
        // reported yet, and missing it would under-report a real position.
        let watching = {
            let data = self.lock_data();
            let now = now_ms();
            data.orders.iter().any(|order| {
                order.is_active() || now - order.updated_ts_ms < FILL_WINDOW_OVERLAP_MS
            })
        };
        let since = {
            let data = self.lock_data();
            (data.fill_cursor_ms - FILL_WINDOW_OVERLAP_MS).max(0)
        };

        // A venue that cannot list its open orders must not retire anything:
        // "no answer" is not "no orders". With several venues that means one
        // silent venue suspends retirement for all of them — retiring an order
        // that is still resting is how a fill later appears out of nowhere.
        let mut open: Option<Vec<OpenOrder>> = Some(Vec::new());
        let mut fills: Vec<OrderFill> = Vec::new();
        for connector in &self.connectors {
            match connector.open_orders().await {
                Ok(orders) => {
                    if let Some(all) = open.as_mut() {
                        all.extend(orders);
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        venue = connector.venue().as_str(),
                        error = %error,
                        "a venue did not list its open orders; retiring nothing this pass"
                    );
                    open = None;
                }
            }
            if watching {
                match connector.fills_since(since).await {
                    Ok(mut reported) => fills.append(&mut reported),
                    Err(error) => tracing::warn!(
                        venue = connector.venue().as_str(),
                        error = %error,
                        "a venue did not report its fills"
                    ),
                }
            }
        }

        // A fill the venue confirms is booked even when the order was already
        // labelled retired. The label is the engine's inference; the fill is
        // the venue's fact, and dropping a real fill would under-report the
        // exposure the risk layer measures.
        let pending: Vec<OrderFill> = {
            let data = self.lock_data();
            fills
                .iter()
                .filter(|fill| {
                    data.orders
                        .iter()
                        .find(|order| order.order_id == fill.order_id && order.venue == fill.venue)
                        .is_some_and(|order| !order.knows_fill(fill))
                })
                .cloned()
                .collect()
        };

        let mut booked = false;
        for fill in pending {
            let Some(mut order) = self
                .lock_data()
                .orders
                .iter()
                .find(|order| order.order_id == fill.order_id && order.venue == fill.venue)
                .cloned()
            else {
                continue;
            };
            self.book_fill(&mut order, &fill);
            let mut data = self.lock_data();
            if let Some(slot) = data
                .orders
                .iter_mut()
                .find(|known| known.order_id == order.order_id && known.venue == order.venue)
            {
                *slot = order;
            }
            booked = true;
        }

        let retired: Vec<(String, OrderStatus, f64)> = match open.as_ref() {
            None => Vec::new(),
            Some(open) => {
                let mut data = self.lock_data();
                let mut out = Vec::new();
                let now = now_ms();
                for order in data.orders.iter_mut() {
                    if !order.is_active() {
                        continue;
                    }
                    if open
                        .iter()
                        .any(|o| o.order_id == order.order_id && o.venue == order.venue)
                    {
                        order.missing_since_ms = None;
                        continue;
                    }
                    let missing_since = *order.missing_since_ms.get_or_insert(now);
                    if now - missing_since < ORDER_RETIRE_GRACE_MS {
                        continue;
                    }
                    // An active order is by definition not fully filled, so an
                    // order the venue stopped resting was cancelled.
                    order.status = OrderStatus::Cancelled;
                    order.updated_ts_ms = now;
                    out.push((order.order_id.clone(), order.status, order.filled_qty()));
                }
                out
            }
        };

        {
            let mut data = self.lock_data();
            for fill in &fills {
                data.fill_cursor_ms = data.fill_cursor_ms.max(fill.ts_ms);
            }
            if let Some(open) = open {
                data.open_orders = open;
            }
        }

        for (order_id, status, filled_qty) in &retired {
            self.log(
                "engine",
                "info",
                &format!(
                    "order {order_id} {}: {filled_qty} filled, the rest is no longer resting",
                    status.as_str()
                ),
                serde_json::json!({
                    "order_id": order_id,
                    "status": status,
                    "filled_qty": filled_qty,
                }),
            );
        }

        if booked || !retired.is_empty() {
            self.persist();
            let portfolio = self.portfolio();
            self.bus.publish(Event::Portfolio(portfolio));
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
        if price > 0.0 {
            self.risk.observe_price(&symbol, price, now_ms());
        }
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

    pub fn strategy_params(&self) -> serde_json::Value {
        let mut params = self
            .strategy_params
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if !params.is_object() {
            params = serde_json::json!({});
        }
        params["max_position_pct"] = serde_json::json!(self.config.risk.max_position_pct_of_equity);
        params["min_notional_usd"] = serde_json::json!(self.config.risk.min_order_notional_usd);
        params
    }

    /// The registry, resolved against the configured artifact directory.
    pub fn strategy_list(&self) -> serde_json::Value {
        let loaded = {
            let guard = self.strategy.lock().unwrap_or_else(|e| e.into_inner());
            guard.as_ref().map(|host| host.info().name)
        };
        let strategies = crate::strategies::STRATEGIES
            .iter()
            .map(|def| {
                let path = crate::strategies::path_for(&self.config.strategy_dir, def);
                serde_json::json!({
                    "name": def.name,
                    "description": def.description,
                    "wasm": def.wasm,
                    "path": path.display().to_string(),
                    "available": path.is_file(),
                    "loaded": loaded.as_deref() == Some(def.name),
                    "default_params": crate::strategies::defaults(def.name),
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({
            "strategy_dir": self.config.strategy_dir.display().to_string(),
            "loaded": loaded,
            "strategies": strategies,
        })
    }

    /// Load a built-in strategy by name.
    ///
    /// The path comes from the registry, never from the caller, so a selection
    /// cannot point the sandbox at an arbitrary file. Parameters are validated
    /// as a JSON object and layered over that strategy's defaults; the
    /// risk-derived keys are added fresh on every tick.
    pub fn select_strategy(
        &self,
        name: &str,
        overrides: Option<&serde_json::Value>,
    ) -> Result<StrategyInfo, StrategySelectError> {
        let def = crate::strategies::find(name)
            .ok_or_else(|| StrategySelectError::Unknown(name.trim().to_string()))?;
        if let Some(overrides) = overrides {
            if !overrides.is_object() {
                return Err(StrategySelectError::InvalidParams);
            }
        }
        let path = crate::strategies::path_for(&self.config.strategy_dir, def);
        if !path.is_file() {
            return Err(StrategySelectError::MissingArtifact(
                path.display().to_string(),
            ));
        }

        let host = {
            let guard = self.strategy.lock().unwrap_or_else(|e| e.into_inner());
            guard
                .as_ref()
                .map(Arc::clone)
                .ok_or(StrategySelectError::NoHost)?
        };
        let info = host
            .reload(path.display().to_string())
            .map_err(|e| StrategySelectError::Load(e.to_string()))?;
        let merged = crate::strategies::merge(
            crate::strategies::defaults(def.name),
            overrides.unwrap_or(&serde_json::Value::Null),
        );
        *self
            .strategy_params
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = merged;
        self.log(
            "strategy",
            "info",
            &format!("selected strategy {}", def.name),
            serde_json::json!({"name": def.name, "path": path.display().to_string()}),
        );
        Ok(info)
    }

    /// Replays bars through the loaded strategy and the real risk layer.
    ///
    /// The live strategy is forked first, so the replay starts with empty
    /// history and cannot disturb the instance that trades. Strategy
    /// parameters default to the live ones unless the caller overrides them.
    pub fn run_backtest(
        &self,
        params: &BacktestParams,
        bars: &[Bar],
    ) -> Result<BacktestReport, BacktestError> {
        let strategy = {
            let guard = self.strategy.lock().unwrap_or_else(|e| e.into_inner());
            guard.clone().ok_or(BacktestError::NoStrategy)?
        };
        let replay = strategy.fork().map_err(|error| BacktestError::Strategy {
            bar: 0,
            ts_ms: 0,
            message: error.to_string(),
        })?;
        let mut params = params.clone();
        if params.params.is_null() {
            params.params = self.strategy_params();
        }
        let mut report = crate::backtest::run(&params, bars, self.config.risk.clone(), |ctx| {
            replay.on_tick(ctx).map_err(|error| error.to_string())
        })?;
        report.strategy = strategy.info().name;
        Ok(report)
    }

    async fn apply_signal(&self, signal: StrategySignal) {
        let current_qty = self
            .portfolio()
            .positions
            .iter()
            .find(|position| position.symbol == signal.symbol)
            .map(|position| position.qty)
            .unwrap_or(0.0);
        let (qty, scale) =
            self.risk
                .scale_for_volatility(signal.side, signal.qty, current_qty, &signal.symbol);
        if qty <= 0.0 {
            self.log(
                "strategy",
                "warn",
                &format!(
                    "{} signal skipped: volatility targeting shrank it below the fill size",
                    signal.symbol
                ),
                serde_json::json!({"symbol": signal.symbol, "scale": scale}),
            );
            return;
        }
        let rationale = if scale < 1.0 {
            self.log(
                "strategy",
                "info",
                &format!(
                    "sized {} {} to {:.0}% for volatility",
                    signal.symbol,
                    signal.side.as_str(),
                    scale * 100.0
                ),
                serde_json::json!({"symbol": signal.symbol, "requested_qty": signal.qty, "qty": qty, "scale": scale}),
            );
            format!(
                "wasm strategy: {} (volatility scale {scale:.2})",
                signal.reason
            )
        } else {
            format!("wasm strategy: {}", signal.reason)
        };
        let req = OrderRequest {
            venue: self.connector.venue(),
            symbol: signal.symbol.clone(),
            side: signal.side,
            qty,
            order_type: OrderType::Market,
            limit_price: None,
            trigger_price: None,
            oco_group: None,
            reduce_only: false,
            client_id: format!("oe-strategy-{}", uuid::Uuid::new_v4().simple()),
            rationale,
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

/// Decimals of a network's gas token.
fn native_decimals(network: &str) -> Option<u32> {
    match network {
        "solana" => Some(9),
        "ethereum" | "base" => Some(18),
        _ => None,
    }
}

/// What a network's fees have cost so far, in the gas token's smallest unit.
///
/// Derived from the broadcast records rather than accumulated: reading the
/// same receipt again can then never book the same fee twice. Only final
/// states count, because a fee on a transfer that was replaced was not paid.
fn gas_spent_raw(data: &DataStore) -> HashMap<String, u128> {
    let mut gas: HashMap<String, u128> = HashMap::new();
    for broadcast in data.broadcasts.iter().filter(|b| b.state.is_terminal()) {
        if broadcast.fee_paid_raw > 0 {
            *gas.entry(broadcast.chain.clone()).or_insert(0) += u128::from(broadcast.fee_paid_raw);
        }
    }
    gas
}

fn gas_value(raw: u128, network: &str, price_usd: f64) -> f64 {
    let Some(decimals) = native_decimals(network) else {
        return 0.0;
    };
    raw as f64 / 10f64.powi(decimals as i32) * price_usd
}

/// The wallet as the broadcaster sees it: addresses from the keystore header,
/// and signatures over the exact bytes the chain will hash.
///
/// The plaintext key exists only for the duration of one signature, and the
/// passphrase is never retained.
struct WalletSigner<'a> {
    wallet: &'a WalletService,
    passphrase: &'a str,
    selector: AccountSelector,
}

impl WalletSigner<'_> {
    fn address(&self, chain: Chain) -> Result<String, BroadcastError> {
        self.wallet
            .address_for(chain, self.selector.index.unwrap_or(0))
            .map(|account| account.address)
            .map_err(|e| BroadcastError::Signer(e.to_string()))
    }

    fn sign(&self, chain: Chain, preimage: &[u8]) -> Result<Vec<u8>, BroadcastError> {
        self.wallet
            .sign_live_with_passphrase(self.passphrase, chain, &self.selector, preimage)
            .map(|signature| signature.signature)
            .map_err(|e| BroadcastError::Signer(e.to_string()))
    }
}

impl TxSigner for WalletSigner<'_> {
    fn evm_address(&self) -> Result<String, BroadcastError> {
        self.address(Chain::Evm)
    }

    fn sign_evm_digest(&self, digest: &[u8; 32]) -> Result<Vec<u8>, BroadcastError> {
        self.sign(Chain::Evm, digest)
    }

    fn solana_address(&self) -> Result<String, BroadcastError> {
        self.address(Chain::Solana)
    }

    fn sign_solana_message(&self, message: &[u8]) -> Result<[u8; 64], BroadcastError> {
        self.sign(Chain::Solana, message)?.try_into().map_err(|_| {
            BroadcastError::Signer("the wallet returned a signature of the wrong length".into())
        })
    }
}

fn build_connectors(config: &Config) -> anyhow::Result<Vec<Arc<dyn ExchangeConnector>>> {
    // The arming ceremony decides whether a connector may place real orders.
    // The connector never decides that for itself, and each venue is decided
    // from its own record.
    let mut connectors: Vec<Arc<dyn ExchangeConnector>> = Vec::new();
    for venue in &config.venues {
        let live = config.is_live(*venue);
        let connector: Arc<dyn ExchangeConnector> = match venue {
            Venue::Paper => Arc::new(crate::exchange::paper::PaperConnector::new()),
            Venue::Binance => Arc::new(crate::exchange::binance::BinanceConnector::from_env(live)?),
            Venue::Kraken => Arc::new(crate::exchange::kraken::KrakenConnector::from_env(live)?),
            Venue::Hyperliquid => {
                Arc::new(crate::exchange::hyperliquid::HyperliquidConnector::from_env(live)?)
            }
        };
        connectors.push(connector);
    }
    Ok(connectors)
}

/// Background loops: market data, strategy evaluation and account refresh.
pub async fn run_background_tasks(state: Arc<AppState>) {
    let symbols = state.config.symbols.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<MarketTick>();

    let sink: TickSink = tx;
    // Every venue streams into one sink. Ticks carry their venue, so the book
    // can mark a position from the venue that actually holds it.
    for connector in &state.connectors {
        if let Err(e) = connector.subscribe_trades(&symbols, sink.clone()).await {
            tracing::warn!(
                venue = connector.venue().as_str(),
                error = %e,
                "market data subscription failed"
            );
            state.log(
                "engine",
                "warn",
                &format!(
                    "market data subscription failed for {}: {e}",
                    connector.venue()
                ),
                serde_json::json!({
                    "venue": connector.venue().as_str(),
                    "error": e.to_string(),
                }),
            );
        }
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
        let mut ticker = tokio::time::interval(Duration::from_secs(ORDER_RECONCILE_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            account_state.refresh_account().await;
            account_state.reconcile_orders().await;
        }
    });

    let wallet_state = Arc::clone(&state);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(WALLET_REFRESH_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            wallet_state.refresh_wallet_holdings().await;
        }
    });

    if state.broadcaster.is_some() {
        let broadcast_state = Arc::clone(&state);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(BROADCAST_RECONCILE_SECS));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                broadcast_state.reconcile_broadcasts().await;
            }
        });
    }
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
    use crate::exchange::paper::{PaperConfig, PaperConnector};
    use crate::exchange::ExchangeResult;
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
        // A test must not pick up a quote source from the developer's shell.
        config.swap_quote_url = None;
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

    /// A quiet simulator: fills at the last mark, no slippage, so a test can
    /// assert exact cost basis and FIFO order.
    fn quiet_paper() -> Arc<PaperConnector> {
        Arc::new(PaperConnector::with_config(PaperConfig {
            seed: 7,
            starting_cash: 10_000.0,
            slippage_bps: 0.0,
            taker_fee_bps: 5.0,
            vol_bps: 0.0,
            ..PaperConfig::default()
        }))
    }

    async fn fill(state: &AppState, symbol: &str, side: Side, qty: f64) {
        let outcome = state
            .submit_order(
                OrderRequest::market(Venue::Paper, symbol, side, qty),
                "test",
            )
            .await;
        assert!(outcome.approved, "{outcome:?}");
    }

    #[tokio::test]
    async fn realized_pnl_closes_the_oldest_lot_first() {
        let (dir, db) = temp_db();
        let paper = quiet_paper();
        paper.set_price("SOL/USDT", 100.0);
        let state = AppState::with_connector(config_with_db(&db), paper.clone()).unwrap();

        fill(&state, "SOL/USDT", Side::Buy, 1.0).await;
        paper.set_price("SOL/USDT", 120.0);
        fill(&state, "SOL/USDT", Side::Buy, 1.0).await;
        let position = &state.portfolio().positions[0];
        assert!(
            (position.avg_entry - 110.0).abs() < 1e-9,
            "{}",
            position.avg_entry
        );

        paper.set_price("SOL/USDT", 130.0);
        fill(&state, "SOL/USDT", Side::Sell, 1.0).await;

        // Selling the oldest lot (100) is +30; a blended average would say +20.
        let sell = state
            .trades(5)
            .into_iter()
            .find(|t| t.side == Side::Sell)
            .unwrap();
        assert!(
            (sell.realized_pnl - 30.0).abs() < 1e-9,
            "{}",
            sell.realized_pnl
        );
        let position = &state.portfolio().positions[0];
        assert!((position.qty - 1.0).abs() < 1e-9);
        assert!(
            (position.avg_entry - 120.0).abs() < 1e-9,
            "{}",
            position.avg_entry
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_tick_moves_the_headline_equity_with_its_position_rows() {
        let (dir, db) = temp_db();
        let paper = quiet_paper();
        paper.set_price("SOL/USDT", 100.0);
        let state = AppState::with_connector(config_with_db(&db), paper.clone()).unwrap();
        state.refresh_account().await;
        fill(&state, "SOL/USDT", Side::Buy, 2.0).await;

        let before = state.portfolio();
        state.record_tick(MarketTick {
            venue: Venue::Paper,
            symbol: "SOL/USDT".into(),
            price: 110.0,
            size: 0.0,
            ts_ms: now_ms(),
        });
        let after = state.portfolio();

        assert!((after.positions[0].mark_price - 110.0).abs() < 1e-9);
        let gain = after.unrealized_pnl_usd - before.unrealized_pnl_usd;
        assert!((gain - 20.0).abs() < 1e-9, "{gain}");
        assert!(
            (after.equity_usd - before.equity_usd - gain).abs() < 1e-9,
            "one snapshot cannot show a ${gain} position gain next to \
             equity moving by {}",
            after.equity_usd - before.equity_usd
        );
        assert!(
            (state.risk_state().equity_usd - after.equity_usd).abs() < 1e-9,
            "the risk panel must read the same equity as the portfolio"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_crashing_mark_trips_the_breaker_between_account_refreshes() {
        let (dir, db) = temp_db();
        let paper = quiet_paper();
        paper.set_price("SOL/USDT", 100.0);
        let state = AppState::with_connector(config_with_db(&db), paper.clone()).unwrap();
        state.refresh_account().await;
        fill(&state, "SOL/USDT", Side::Buy, 19.0).await;
        assert_eq!(
            state.risk_state().breaker_state,
            BreakerState::Closed,
            "19% of equity is inside the position cap and must not trip"
        );

        // No account refresh in between: the halt has to come from the mark.
        state.record_tick(MarketTick {
            venue: Venue::Paper,
            symbol: "SOL/USDT".into(),
            price: 1.0,
            size: 0.0,
            ts_ms: now_ms(),
        });

        let risk = state.risk_state();
        assert_eq!(risk.breaker_state, BreakerState::Open);
        assert!(
            risk.breaker_reason
                .as_deref()
                .unwrap_or_default()
                .contains("drawdown"),
            "{}",
            risk.breaker_reason.unwrap_or_default()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_restart_keeps_the_fifo_lot_order() {
        let (dir, db) = temp_db();
        {
            let paper = quiet_paper();
            paper.set_price("SOL/USDT", 100.0);
            let state = AppState::with_connector(config_with_db(&db), paper.clone()).unwrap();
            fill(&state, "SOL/USDT", Side::Buy, 1.0).await;
            paper.set_price("SOL/USDT", 120.0);
            fill(&state, "SOL/USDT", Side::Buy, 1.0).await;
        }

        let paper = quiet_paper();
        let state = AppState::with_connector(config_with_db(&db), paper.clone()).unwrap();
        paper.set_price("SOL/USDT", 130.0);
        fill(&state, "SOL/USDT", Side::Sell, 1.0).await;

        let sell = state
            .trades(5)
            .into_iter()
            .find(|t| t.side == Side::Sell)
            .unwrap();
        assert!(
            (sell.realized_pnl - 30.0).abs() < 1e-9,
            "a restart must not forget which lot was bought first, got {}",
            sell.realized_pnl
        );
        assert!((state.portfolio().positions[0].avg_entry - 120.0).abs() < 1e-9);
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

    /// The wallet is reported, never traded. A million dollars of on-chain
    /// tokens must not widen a single limit of the account that can actually
    /// lose money.
    #[tokio::test]
    async fn the_wallet_value_is_reported_but_never_traded() {
        let (dir, db) = temp_db();
        let state = AppState::new(config_with_db(&db)).unwrap();
        state.lock_data().wallet_holdings = Some(holdings::wallet_of(
            vec![crate::holdings::ChainHoldings {
                chain: "solana".into(),
                address: "addr".into(),
                native_symbol: "SOL".into(),
                native_balance: 1.0,
                native_price_usd: 1_000_000.0,
                native_value_usd: 1_000_000.0,
                tokens: vec![],
                tokens_skipped: 0,
                value_usd: 1_000_000.0,
                gas_spent_usd: 0.0,
                fetched_at_ms: now_ms(),
            }],
            true,
        ));

        let portfolio = state.portfolio();
        assert_eq!(portfolio.onchain_value_usd, 1_000_000.0);
        assert_eq!(
            portfolio.equity_usd, 10_000.0,
            "the trading account is still the only equity the risk layer sees"
        );
        assert_eq!(state.risk_state().equity_usd, 10_000.0);

        // Sized against the wallet this would be trivially affordable. Against
        // the trading account it is far past every cap.
        let outcome = state
            .submit_order(
                OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Buy, 100.0),
                "test",
            )
            .await;
        assert!(!outcome.approved, "{outcome:?}");
        assert_eq!(state.equity(), 10_000.0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn an_engine_without_a_keystore_reports_an_empty_wallet() {
        let (dir, db) = temp_db();
        let state = AppState::new(config_with_db(&db)).unwrap();
        let wallet = state.wallet_holdings();
        assert!(!wallet.configured);
        assert!(wallet.chains.is_empty());
        assert_eq!(wallet.value_usd, 0.0);
        assert_eq!(state.portfolio().onchain_value_usd, 0.0);
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

    /// A venue whose answers the test controls.
    ///
    /// Reconciliation has to survive what a real exchange does: an order that
    /// rests, a fill that arrives in pieces, a fill reported only after the
    /// order left the open list, and an open-orders call that fails.
    struct ScriptedVenue {
        venue: Venue,
        open: std::sync::Mutex<Vec<OpenOrder>>,
        fills: std::sync::Mutex<Vec<OrderFill>>,
        open_orders_fail: std::sync::atomic::AtomicBool,
        next_id: std::sync::atomic::AtomicU64,
    }

    impl ScriptedVenue {
        fn new() -> Arc<Self> {
            Self::on(Venue::Paper)
        }

        fn on(venue: Venue) -> Arc<Self> {
            Arc::new(Self {
                venue,
                open: std::sync::Mutex::new(Vec::new()),
                fills: std::sync::Mutex::new(Vec::new()),
                open_orders_fail: std::sync::atomic::AtomicBool::new(false),
                next_id: std::sync::atomic::AtomicU64::new(1),
            })
        }

        fn report_fill(&self, order_id: &str, qty: f64, price: f64, ts_ms: i64) {
            self.fills.lock().unwrap().push(OrderFill {
                venue: self.venue,
                order_id: order_id.to_string(),
                symbol: "SOL/USDT".to_string(),
                side: Side::Buy,
                qty,
                price,
                fee: 0.0,
                ts_ms,
            });
        }

        fn stop_resting(&self) {
            self.open.lock().unwrap().clear();
        }
    }

    #[async_trait::async_trait]
    impl ExchangeConnector for ScriptedVenue {
        fn venue(&self) -> Venue {
            self.venue
        }

        async fn health(&self) -> ExchangeResult<()> {
            Ok(())
        }

        async fn account(&self) -> ExchangeResult<AccountSnapshot> {
            Ok(AccountSnapshot {
                venue: self.venue,
                equity_usd: 10_000.0,
                free_usd: 10_000.0,
                balances: Vec::new(),
                ts_ms: now_ms(),
            })
        }

        async fn place_order(&self, req: &OrderRequest) -> ExchangeResult<OrderAck> {
            let id = format!(
                "scripted-{}",
                self.next_id
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            );
            self.open.lock().unwrap().push(OpenOrder {
                venue: self.venue,
                order_id: id.clone(),
                symbol: req.symbol.clone(),
                side: req.side,
                qty: req.qty,
                price: req.limit_price.unwrap_or(100.0),
                ts_ms: now_ms(),
            });
            Ok(OrderAck {
                venue: self.venue,
                order_id: id,
                client_id: req.client_id.clone(),
                status: OrderStatus::Accepted,
                accepted_ts_ms: now_ms(),
                message: "resting".into(),
            })
        }

        async fn cancel_order(&self, order_id: &str) -> ExchangeResult<()> {
            self.open
                .lock()
                .unwrap()
                .retain(|order| order.order_id != order_id);
            Ok(())
        }

        async fn open_orders(&self) -> ExchangeResult<Vec<OpenOrder>> {
            if self
                .open_orders_fail
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(ExchangeError::Network("the venue did not answer".into()));
            }
            Ok(self.open.lock().unwrap().clone())
        }

        async fn fills_since(&self, since_ms: i64) -> ExchangeResult<Vec<OrderFill>> {
            Ok(self
                .fills
                .lock()
                .unwrap()
                .iter()
                .filter(|fill| fill.ts_ms >= since_ms)
                .cloned()
                .collect())
        }

        async fn subscribe_trades(
            &self,
            _symbols: &[String],
            _sink: TickSink,
        ) -> ExchangeResult<()> {
            Ok(())
        }

        async fn last_price(&self, _symbol: &str) -> ExchangeResult<f64> {
            Ok(100.0)
        }
    }

    fn limit_order(qty: f64, price: f64) -> OrderRequest {
        let mut req = OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Buy, qty);
        req.order_type = OrderType::Limit;
        req.limit_price = Some(price);
        req
    }

    /// A venue that quotes a price the test sets and fills immediately.
    ///
    /// Routing compares quotes and fees, and a fill has to come back from the
    /// venue the order actually reached, so the fake records what it filled.
    struct QuotedVenue {
        venue: Venue,
        fee_bps: f64,
        price: std::sync::Mutex<f64>,
        equity: f64,
        fills: std::sync::Mutex<Vec<OrderFill>>,
        next_id: std::sync::atomic::AtomicU64,
    }

    impl QuotedVenue {
        fn new(venue: Venue, price: f64, fee_bps: f64, equity: f64) -> Arc<Self> {
            Arc::new(Self {
                venue,
                fee_bps,
                price: std::sync::Mutex::new(price),
                equity,
                fills: std::sync::Mutex::new(Vec::new()),
                next_id: std::sync::atomic::AtomicU64::new(1),
            })
        }

        fn set_price(&self, price: f64) {
            *self.price.lock().unwrap() = price;
        }
    }

    #[async_trait::async_trait]
    impl ExchangeConnector for QuotedVenue {
        fn venue(&self) -> Venue {
            self.venue
        }

        fn taker_fee_bps(&self) -> f64 {
            self.fee_bps
        }

        async fn health(&self) -> ExchangeResult<()> {
            Ok(())
        }

        async fn account(&self) -> ExchangeResult<AccountSnapshot> {
            Ok(AccountSnapshot {
                venue: self.venue,
                equity_usd: self.equity,
                free_usd: self.equity,
                balances: Vec::new(),
                ts_ms: now_ms(),
            })
        }

        async fn place_order(&self, req: &OrderRequest) -> ExchangeResult<OrderAck> {
            let price = *self.price.lock().unwrap();
            let id = format!(
                "{}-{}",
                self.venue.as_str(),
                self.next_id
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            );
            self.fills.lock().unwrap().push(OrderFill {
                venue: self.venue,
                order_id: id.clone(),
                symbol: req.symbol.clone(),
                side: req.side,
                qty: req.qty,
                price,
                fee: req.qty * price * self.fee_bps / 10_000.0,
                ts_ms: now_ms(),
            });
            Ok(OrderAck {
                venue: self.venue,
                order_id: id,
                client_id: req.client_id.clone(),
                status: OrderStatus::Filled,
                accepted_ts_ms: now_ms(),
                message: "filled".into(),
            })
        }

        async fn cancel_order(&self, _order_id: &str) -> ExchangeResult<()> {
            Ok(())
        }

        async fn open_orders(&self) -> ExchangeResult<Vec<OpenOrder>> {
            Ok(Vec::new())
        }

        async fn fills_since(&self, since_ms: i64) -> ExchangeResult<Vec<OrderFill>> {
            Ok(self
                .fills
                .lock()
                .unwrap()
                .iter()
                .filter(|fill| fill.ts_ms >= since_ms)
                .cloned()
                .collect())
        }

        async fn subscribe_trades(
            &self,
            _symbols: &[String],
            _sink: TickSink,
        ) -> ExchangeResult<()> {
            Ok(())
        }

        async fn last_price(&self, _symbol: &str) -> ExchangeResult<f64> {
            Ok(*self.price.lock().unwrap())
        }
    }

    fn armed_state(cap: f64) -> crate::live_arm::LiveArmingState {
        let mut state = crate::live_arm::LiveArmingState::not_armed("test", "armed by the test");
        state.live_enabled = true;
        state.reason = None;
        state.detail = None;
        state.effective_max_order_notional_usd = cap;
        state
    }

    /// A config listing `venues`, each armed with `cap`.
    fn routed_config(db: &Path, venues: &[(Venue, f64)]) -> Config {
        let mut config = config_with_db(db);
        config.venues = venues.iter().map(|(venue, _)| *venue).collect();
        config.venue = config.venues[0];
        config.live_armings = venues
            .iter()
            .map(|(venue, cap)| crate::config::VenueArming {
                venue: *venue,
                state: armed_state(*cap),
            })
            .collect();
        config.live_trading = true;
        config.risk.venue_allowlist = config.venues.iter().copied().collect();
        config.risk.symbol_allowlist.insert("SOL/USDT".into());
        config.risk.max_order_notional_usd = 5_000.0;
        config
    }

    #[tokio::test]
    async fn a_buy_goes_to_the_cheaper_venue_after_its_fee() {
        let (dir, db) = temp_db();
        // Kraken quotes a lower price, but its taker fee is 26 bps against
        // Binance's 10: 99.90 * 1.0026 = 100.16 against 100 * 1.001 = 100.10.
        let binance = QuotedVenue::new(Venue::Binance, 100.0, 10.0, 10_000.0);
        let kraken = QuotedVenue::new(Venue::Kraken, 99.9, 26.0, 10_000.0);
        let state = AppState::with_connectors(
            routed_config(&db, &[(Venue::Binance, 500.0), (Venue::Kraken, 500.0)]),
            vec![binance.clone(), kraken.clone()],
        )
        .unwrap();

        let outcome = state
            .submit_order(
                OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Buy, 1.0),
                "test",
            )
            .await;
        assert!(outcome.approved, "{outcome:?}");
        assert_eq!(outcome.venue, Some(Venue::Binance));
        assert!(binance.fills.lock().unwrap().len() == 1);
        assert!(kraken.fills.lock().unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_sell_goes_to_the_venue_that_pays_more_after_its_fee() {
        let (dir, db) = temp_db();
        // Mirrored quotes: Kraken is cheaper to buy, and therefore worse to
        // sell. Binance nets 99.90 against Kraken's 99.64.
        let binance = QuotedVenue::new(Venue::Binance, 100.0, 10.0, 10_000.0);
        let kraken = QuotedVenue::new(Venue::Kraken, 99.9, 26.0, 10_000.0);
        let state = AppState::with_connectors(
            routed_config(&db, &[(Venue::Binance, 500.0), (Venue::Kraken, 500.0)]),
            vec![binance.clone(), kraken.clone()],
        )
        .unwrap();

        let outcome = state
            .submit_order(
                OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Sell, 1.0),
                "test",
            )
            .await;
        assert!(outcome.approved, "{outcome:?}");
        assert_eq!(outcome.venue, Some(Venue::Binance));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn inventory_pins_the_close_to_the_venue_that_holds_it() {
        let (dir, db) = temp_db();
        let binance = QuotedVenue::new(Venue::Binance, 100.0, 10.0, 10_000.0);
        let kraken = QuotedVenue::new(Venue::Kraken, 101.0, 26.0, 10_000.0);
        let state = AppState::with_connectors(
            routed_config(&db, &[(Venue::Binance, 500.0), (Venue::Kraken, 500.0)]),
            vec![binance.clone(), kraken.clone()],
        )
        .unwrap();

        let opened = state
            .submit_order(
                OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Buy, 1.0),
                "test",
            )
            .await;
        assert!(opened.approved, "{opened:?}");
        assert_eq!(opened.venue, Some(Venue::Binance));
        assert_eq!(state.portfolio().positions[0].venue, Venue::Binance);

        // Kraken now pays more for the coin. The close still has to land where
        // the coin is, otherwise Binance keeps the long and Kraken opens a
        // short — two positions where the book shows none.
        kraken.set_price(200.0);
        let closed = state
            .submit_order(
                OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Sell, 1.0),
                "test",
            )
            .await;
        assert!(closed.approved, "{closed:?}");
        assert_eq!(closed.venue, Some(Venue::Binance));
        assert_eq!(kraken.fills.lock().unwrap().len(), 0);
        assert!(state.portfolio().positions.is_empty(), "the book is flat");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn an_unarmed_venue_is_never_routed_to() {
        let (dir, db) = temp_db();
        let binance = QuotedVenue::new(Venue::Binance, 105.0, 10.0, 10_000.0);
        let kraken = QuotedVenue::new(Venue::Kraken, 100.0, 26.0, 10_000.0);
        let mut config = routed_config(&db, &[(Venue::Binance, 500.0)]);
        // Kraken is configured and quotes better, but nothing armed it.
        config.venues = vec![Venue::Binance, Venue::Kraken];
        config.risk.venue_allowlist = config.venues.iter().copied().collect();
        let state =
            AppState::with_connectors(config, vec![binance.clone(), kraken.clone()]).unwrap();

        let outcome = state
            .submit_order(
                OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Buy, 1.0),
                "test",
            )
            .await;
        assert!(outcome.approved, "{outcome:?}");
        assert_eq!(outcome.venue, Some(Venue::Binance));
        assert_eq!(kraken.fills.lock().unwrap().len(), 0);

        // With nothing armed at all, the order is refused rather than quietly
        // simulated. The database is fresh so the position booked above is not
        // restored onto a venue this state does not connect to.
        let (dir2, db2) = temp_db();
        let mut config = routed_config(&db2, &[(Venue::Kraken, 500.0)]);
        config.live_armings = vec![crate::config::VenueArming {
            venue: Venue::Kraken,
            state: crate::live_arm::LiveArmingState::not_armed("test", "not armed"),
        }];
        config.risk.venue_allowlist = config.venues.iter().copied().collect();
        let state = AppState::with_connectors(config, vec![kraken]).unwrap();
        let refused = state
            .submit_order(
                OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Buy, 1.0),
                "test",
            )
            .await;
        assert!(!refused.approved);
        assert_eq!(refused.reason, "venue_not_armed");
        assert_eq!(refused.venue, None);
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&dir2).ok();
    }

    #[tokio::test]
    async fn an_explicit_venue_is_a_constraint_not_a_suggestion() {
        let (dir, db) = temp_db();
        let binance = QuotedVenue::new(Venue::Binance, 100.0, 10.0, 10_000.0);
        // Kraken is cheaper after its fee, so a free choice would pick it.
        let kraken = QuotedVenue::new(Venue::Kraken, 99.0, 26.0, 10_000.0);
        let config = routed_config(&db, &[(Venue::Binance, 500.0), (Venue::Kraken, 500.0)]);
        let state =
            AppState::with_connectors(config, vec![binance.clone(), kraken.clone()]).unwrap();

        let pinned = state
            .submit_order(
                OrderRequest::market(Venue::Binance, "SOL/USDT", Side::Buy, 1.0),
                "test",
            )
            .await;
        assert!(pinned.approved, "{pinned:?}");
        assert_eq!(pinned.venue, Some(Venue::Binance));
        assert_eq!(binance.fills.lock().unwrap().len(), 1);
        assert_eq!(kraken.fills.lock().unwrap().len(), 0);

        // The position is now held on Binance, so asking for Kraken would open
        // a second book. That is refused, not quietly rerouted.
        let refused = state
            .submit_order(
                OrderRequest::market(Venue::Kraken, "SOL/USDT", Side::Sell, 1.0),
                "test",
            )
            .await;
        assert!(!refused.approved);
        assert_eq!(refused.reason, "venue_unavailable");
        assert!(
            refused.detail.contains("held on binance"),
            "{}",
            refused.detail
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_read_only_venue_is_never_routed_to() {
        let (dir, db) = temp_db();
        let binance = QuotedVenue::new(Venue::Binance, 100.0, 10.0, 10_000.0);
        // Hyperliquid quotes far cheaper, but it does not place orders in this
        // build, so routing must not hand it an approved order to fail later.
        let hyperliquid = QuotedVenue::new(Venue::Hyperliquid, 50.0, 4.5, 10_000.0);
        let config = routed_config(&db, &[(Venue::Binance, 500.0), (Venue::Hyperliquid, 500.0)]);
        let state =
            AppState::with_connectors(config, vec![binance.clone(), hyperliquid.clone()]).unwrap();

        let refused = state
            .submit_order(
                OrderRequest::market(Venue::Hyperliquid, "SOL/USDT", Side::Buy, 1.0),
                "test",
            )
            .await;
        assert!(!refused.approved);
        assert_eq!(refused.reason, "venue_unavailable");

        let outcome = state
            .submit_order(
                OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Buy, 1.0),
                "test",
            )
            .await;
        assert!(outcome.approved, "{outcome:?}");
        assert_eq!(outcome.venue, Some(Venue::Binance));
        assert_eq!(hyperliquid.fills.lock().unwrap().len(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn an_armed_cap_binds_only_its_own_venue() {
        let (dir, db) = temp_db();
        let binance = QuotedVenue::new(Venue::Binance, 100.0, 10.0, 10_000.0);
        let kraken = QuotedVenue::new(Venue::Kraken, 100.0, 10.0, 10_000.0);
        // Binance was armed with a $150 cap; Kraken with the risk layer's cap.
        let state = AppState::with_connectors(
            routed_config(&db, &[(Venue::Binance, 150.0), (Venue::Kraken, 5_000.0)]),
            vec![binance, kraken.clone()],
        )
        .unwrap();

        // 2 SOL at 100 is $200: above Binance's armed cap, and Binance is the
        // venue the tie routes to.
        let refused = state
            .submit_order(
                OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Buy, 2.0),
                "test",
            )
            .await;
        assert!(!refused.approved, "{refused:?}");
        assert_eq!(refused.reason, "notional_too_large");
        assert!(refused.detail.contains("150.00"), "{}", refused.detail);

        // The same order is fine once Binance is not in the picture.
        let mut config = routed_config(&db, &[(Venue::Kraken, 5_000.0)]);
        config.venues = vec![Venue::Kraken];
        config.venue = Venue::Kraken;
        config.risk.venue_allowlist = config.venues.iter().copied().collect();
        let state = AppState::with_connectors(config, vec![kraken]).unwrap();
        let approved = state
            .submit_order(
                OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Buy, 2.0),
                "test",
            )
            .await;
        assert!(approved.approved, "{approved:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn every_venue_account_is_summed_into_equity() {
        let (dir, db) = temp_db();
        let binance = QuotedVenue::new(Venue::Binance, 100.0, 10.0, 7_000.0);
        let kraken = QuotedVenue::new(Venue::Kraken, 100.0, 10.0, 4_000.0);
        let state = AppState::with_connectors(
            routed_config(&db, &[(Venue::Binance, 500.0), (Venue::Kraken, 500.0)]),
            vec![binance, kraken],
        )
        .unwrap();

        state.refresh_account().await;
        assert!(
            (state.equity() - 11_000.0).abs() < 1e-9,
            "{}",
            state.equity()
        );
        assert!((state.portfolio().cash_usd - 11_000.0).abs() < 1e-9);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_position_is_marked_by_the_venue_that_holds_it() {
        let (dir, db) = temp_db();
        let binance = QuotedVenue::new(Venue::Binance, 100.0, 0.0, 10_000.0);
        let kraken = QuotedVenue::new(Venue::Kraken, 300.0, 0.0, 10_000.0);
        let state = AppState::with_connectors(
            routed_config(&db, &[(Venue::Binance, 500.0), (Venue::Kraken, 500.0)]),
            vec![binance, kraken],
        )
        .unwrap();

        let opened = state
            .submit_order(
                OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Buy, 1.0),
                "test",
            )
            .await;
        assert!(opened.approved, "{opened:?}");
        state.refresh_account().await;
        let equity_before = state.equity();

        // Kraken's book is empty, so its price says nothing about this
        // position; a tick from it must not mark the long.
        state.record_tick(MarketTick {
            venue: Venue::Kraken,
            symbol: "SOL/USDT".into(),
            price: 300.0,
            size: 1.0,
            ts_ms: now_ms(),
        });
        assert_eq!(state.last_price("SOL/USDT"), Some(100.0));
        assert!((state.equity() - equity_before).abs() < 1e-9);

        // The holder's tick does.
        state.record_tick(MarketTick {
            venue: Venue::Binance,
            symbol: "SOL/USDT".into(),
            price: 110.0,
            size: 1.0,
            ts_ms: now_ms(),
        });
        assert_eq!(state.last_price("SOL/USDT"), Some(110.0));
        assert!((state.equity() - (equity_before + 10.0)).abs() < 1e-9);
        assert_eq!(state.portfolio().positions[0].venue, Venue::Binance);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The bug this whole task exists to fix: the engine used to invent a full
    /// fill whenever the venue reported none, so a resting limit order became a
    /// position that did not exist and a trade that never happened.
    #[tokio::test]
    async fn a_resting_order_books_nothing() {
        let (dir, db) = temp_db();
        let venue = ScriptedVenue::new();
        let state = AppState::with_connector(config_with_db(&db), venue.clone()).unwrap();
        let outcome = state.submit_order(limit_order(2.0, 100.0), "test").await;
        assert!(outcome.approved, "{outcome:?}");

        state.reconcile_orders().await;

        let orders = state.orders();
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].status, OrderStatus::Accepted);
        assert!(orders[0].fills.is_empty());
        assert_eq!(orders[0].filled_qty(), 0.0);
        assert!(
            state.portfolio().positions.is_empty(),
            "a resting order is not a position"
        );
        assert!(
            state.trades(10).is_empty(),
            "a resting order is not a trade"
        );
        assert_eq!(state.equity(), 10_000.0);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The same fix on the venue people actually run: a resting limit order in
    /// the simulator books nothing until the price crosses it.
    #[tokio::test]
    async fn a_paper_limit_order_books_only_when_the_price_crosses() {
        let (dir, db) = temp_db();
        let paper = Arc::new(crate::exchange::paper::PaperConnector::new());
        paper.set_price("SOL/USDT", 142.0);
        let state = AppState::with_connector(config_with_db(&db), paper.clone()).unwrap();

        let outcome = state.submit_order(limit_order(2.0, 130.0), "test").await;
        assert!(outcome.approved, "{outcome:?}");
        assert_eq!(outcome.status, Some(OrderStatus::Accepted));
        assert_eq!(outcome.fill_price, None, "nothing has filled yet");
        state.reconcile_orders().await;
        assert!(state.trades(10).is_empty());
        assert!(state.portfolio().positions.is_empty());

        paper.set_price("SOL/USDT", 129.0);
        state.reconcile_orders().await;

        let orders = state.orders();
        assert_eq!(orders[0].status, OrderStatus::Filled);
        assert!((orders[0].filled_qty() - 2.0).abs() < 1e-9);
        assert!(
            (orders[0].avg_fill_price() - 130.0).abs() < 1e-9,
            "a crossed limit fills at its limit price"
        );
        assert_eq!(state.trades(10).len(), 1);
        assert!((state.portfolio().positions[0].qty - 2.0).abs() < 1e-9);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_partial_fill_books_only_the_filled_part() {
        let (dir, db) = temp_db();
        let venue = ScriptedVenue::new();
        let state = AppState::with_connector(config_with_db(&db), venue.clone()).unwrap();
        let order_id = state
            .submit_order(limit_order(2.0, 100.0), "test")
            .await
            .order_id
            .unwrap();

        venue.report_fill(&order_id, 1.0, 100.0, now_ms());
        state.reconcile_orders().await;

        let orders = state.orders();
        assert_eq!(orders[0].status, OrderStatus::PartiallyFilled);
        assert!((orders[0].filled_qty() - 1.0).abs() < 1e-9);
        assert!((orders[0].remaining_qty() - 1.0).abs() < 1e-9);
        let positions = state.portfolio().positions;
        assert_eq!(positions.len(), 1);
        assert!(
            (positions[0].qty - 1.0).abs() < 1e-9,
            "only the filled half is a position"
        );
        assert_eq!(state.trades(10).len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn reconciling_the_same_window_again_does_not_double_book() {
        let (dir, db) = temp_db();
        let venue = ScriptedVenue::new();
        let state = AppState::with_connector(config_with_db(&db), venue.clone()).unwrap();
        let order_id = state
            .submit_order(limit_order(2.0, 100.0), "test")
            .await
            .order_id
            .unwrap();
        venue.report_fill(&order_id, 1.0, 100.0, now_ms());

        for _ in 0..4 {
            state.reconcile_orders().await;
        }

        assert_eq!(state.trades(10).len(), 1, "the fill is booked once");
        assert!((state.orders()[0].filled_qty() - 1.0).abs() < 1e-9);
        assert!((state.portfolio().positions[0].qty - 1.0).abs() < 1e-9);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn the_remaining_half_fills_at_its_own_price() {
        let (dir, db) = temp_db();
        let venue = ScriptedVenue::new();
        let state = AppState::with_connector(config_with_db(&db), venue.clone()).unwrap();
        let order_id = state
            .submit_order(limit_order(2.0, 100.0), "test")
            .await
            .order_id
            .unwrap();
        let ts = now_ms();
        venue.report_fill(&order_id, 1.0, 100.0, ts);
        state.reconcile_orders().await;
        venue.report_fill(&order_id, 1.0, 102.0, ts + 1);
        state.reconcile_orders().await;

        let orders = state.orders();
        assert_eq!(orders[0].status, OrderStatus::Filled);
        assert!((orders[0].filled_qty() - 2.0).abs() < 1e-9);
        assert_eq!(orders[0].remaining_qty(), 0.0);
        assert!(
            (orders[0].avg_fill_price() - 101.0).abs() < 1e-9,
            "the average is weighted by quantity, not the last price"
        );
        assert_eq!(state.trades(10).len(), 2);
        assert!((state.portfolio().positions[0].qty - 2.0).abs() < 1e-9);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Trade history lags the open list, so an order that vanishes from the
    /// open list has not necessarily been cancelled.
    #[tokio::test]
    async fn an_order_is_not_retired_the_moment_it_leaves_the_open_list() {
        let (dir, db) = temp_db();
        let venue = ScriptedVenue::new();
        let state = AppState::with_connector(config_with_db(&db), venue.clone()).unwrap();
        state.submit_order(limit_order(2.0, 100.0), "test").await;

        venue.stop_resting();
        state.reconcile_orders().await;
        assert!(
            state.orders()[0].is_active(),
            "the fill may still be on its way"
        );
        assert!(state.orders()[0].missing_since_ms.is_some());

        state.lock_data().orders[0].missing_since_ms = Some(now_ms() - ORDER_RETIRE_GRACE_MS - 1);
        state.reconcile_orders().await;
        assert_eq!(state.orders()[0].status, OrderStatus::Cancelled);
        assert!(state.trades(10).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The dangerous direction is under-reporting a position: the risk layer
    /// would then measure limits against exposure that is really there.
    #[tokio::test]
    async fn a_fill_reported_after_retirement_is_still_booked() {
        let (dir, db) = temp_db();
        let venue = ScriptedVenue::new();
        let state = AppState::with_connector(config_with_db(&db), venue.clone()).unwrap();
        let order_id = state
            .submit_order(limit_order(2.0, 100.0), "test")
            .await
            .order_id
            .unwrap();
        venue.stop_resting();
        state.reconcile_orders().await;
        state.lock_data().orders[0].missing_since_ms = Some(now_ms() - ORDER_RETIRE_GRACE_MS - 1);
        state.reconcile_orders().await;
        assert_eq!(state.orders()[0].status, OrderStatus::Cancelled);

        venue.report_fill(&order_id, 2.0, 100.0, now_ms());
        state.reconcile_orders().await;

        assert_eq!(state.trades(10).len(), 1, "the venue's fill wins");
        assert_eq!(state.orders()[0].status, OrderStatus::Filled);
        assert!((state.portfolio().positions[0].qty - 2.0).abs() < 1e-9);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_venue_that_cannot_list_open_orders_retires_nothing() {
        let (dir, db) = temp_db();
        let venue = ScriptedVenue::new();
        let state = AppState::with_connector(config_with_db(&db), venue.clone()).unwrap();
        state.submit_order(limit_order(2.0, 100.0), "test").await;
        state.lock_data().orders[0].missing_since_ms = Some(now_ms() - ORDER_RETIRE_GRACE_MS - 1);
        venue.stop_resting();
        venue
            .open_orders_fail
            .store(true, std::sync::atomic::Ordering::SeqCst);

        state.reconcile_orders().await;

        assert!(state.orders()[0].is_active(), "no answer is not no orders");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Two venues number their orders independently, so the same id can name
    /// two different orders. A fill must be matched by venue as well, or a fill
    /// on one exchange would book against — and retire — the other's order.
    #[tokio::test]
    async fn a_fill_is_matched_to_its_venue_not_just_its_order_id() {
        let (dir, db) = temp_db();
        let binance = ScriptedVenue::on(Venue::Binance);
        let kraken = ScriptedVenue::on(Venue::Kraken);
        let config = routed_config(&db, &[(Venue::Binance, 500.0), (Venue::Kraken, 500.0)]);
        let state =
            AppState::with_connectors(config, vec![binance.clone(), kraken.clone()]).unwrap();

        // Both venues would number their first order "scripted-1"; the order
        // under test rests on Binance, which is first in preference on a tie.
        let order_id = state
            .submit_order(limit_order(2.0, 100.0), "test")
            .await
            .order_id
            .unwrap();
        assert_eq!(order_id, "scripted-1");
        assert_eq!(state.orders()[0].venue, Venue::Binance);

        // Kraken reports a fill for its own order of that same id.
        kraken.report_fill("scripted-1", 1.0, 100.0, now_ms());
        state.reconcile_orders().await;

        assert!(
            state.trades(10).is_empty(),
            "a fill on another venue is not a fill of this order"
        );
        assert!(state.portfolio().positions.is_empty());
        let orders = state.orders();
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].venue, Venue::Binance);
        assert_eq!(
            orders[0].status,
            OrderStatus::Accepted,
            "the Binance order is still resting"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn cancelling_still_books_the_fill_that_landed_first() {
        let (dir, db) = temp_db();
        let venue = ScriptedVenue::new();
        let state = AppState::with_connector(config_with_db(&db), venue.clone()).unwrap();
        let order_id = state
            .submit_order(limit_order(2.0, 100.0), "test")
            .await
            .order_id
            .unwrap();
        // Filled at the venue just before the cancel, reported just after it.
        venue.report_fill(&order_id, 1.0, 100.0, now_ms());

        state.cancel_order(&order_id).await.unwrap();
        assert_eq!(state.orders()[0].status, OrderStatus::Cancelled);
        assert!(state.cancel_order("no-such-order").await.is_err());
        assert!(
            state.cancel_order(&order_id).await.is_err(),
            "a retired order cannot be cancelled again"
        );

        state.reconcile_orders().await;
        assert_eq!(state.trades(10).len(), 1);
        assert!((state.portfolio().positions[0].qty - 1.0).abs() < 1e-9);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn tracked_orders_and_the_fill_cursor_survive_a_restart() {
        let (dir, db) = temp_db();
        let venue = ScriptedVenue::new();
        let ts = now_ms();
        let order_id;
        {
            let state = AppState::with_connector(config_with_db(&db), venue.clone()).unwrap();
            order_id = state
                .submit_order(limit_order(2.0, 100.0), "test")
                .await
                .order_id
                .unwrap();
            venue.report_fill(&order_id, 1.0, 100.0, ts);
            state.reconcile_orders().await;
        }

        let state = AppState::with_connector(config_with_db(&db), venue.clone()).unwrap();
        let orders = state.orders();
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].order_id, order_id);
        assert_eq!(orders[0].status, OrderStatus::PartiallyFilled);
        assert!(
            (orders[0].filled_qty() - 1.0).abs() < 1e-9,
            "the fill ladder comes back with the order"
        );
        assert_eq!(state.lock_data().fill_cursor_ms, ts);

        state.reconcile_orders().await;
        assert_eq!(
            state.trades(10).len(),
            1,
            "a restart must not re-book what the last run journaled"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /* ---------------------------------------------------- on-chain transfer */

    fn armed_only_in_config(config: &mut Config) {
        config.onchain_live = true;
        config.onchain_arming = crate::live_arm::LiveArmingState {
            live_enabled: true,
            record: None,
            reason: None,
            detail: None,
            effective_max_order_notional_usd: 100.0,
        };
    }

    fn confirmed_broadcast(id: &str, chain: &str, fee_paid_raw: u64) -> Broadcast {
        Broadcast {
            id: id.to_string(),
            chain: chain.to_string(),
            from: "from".into(),
            to: "to".into(),
            nonce: 1,
            amount_raw: 1_000,
            fee_limit_raw: fee_paid_raw,
            fee_paid_raw,
            tx_hash: format!("0x{id}"),
            raw_tx: "0x02".into(),
            call_data: String::new(),
            block_number: 100,
            block_hash: "0xblock".into(),
            last_valid_block_height: 0,
            confirmations: 12,
            state: BroadcastState::Confirmed,
            note: String::new(),
            created_at_ms: now_ms(),
            updated_at_ms: now_ms(),
        }
    }

    #[tokio::test]
    async fn an_unarmed_core_never_broadcasts() {
        let (dir, db) = temp_db();
        let mut config = config_with_db(&db);
        config.onchain_live = false;
        config.wallet.dry_run = false;
        let state = AppState::new(config).unwrap();

        let error = state
            .broadcast_transfer(
                "solana",
                "11111111111111111111111111111111",
                1,
                None,
                "pw",
                Default::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, TransferError::NotArmed(_)), "{error:?}");
        assert!(state.broadcasts(10).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_dry_run_wallet_never_broadcasts_even_when_armed() {
        let (dir, db) = temp_db();
        let mut config = config_with_db(&db);
        armed_only_in_config(&mut config);
        config.wallet.dry_run = true;
        let state = AppState::new(config).unwrap();

        let error = state
            .broadcast_transfer(
                "solana",
                "11111111111111111111111111111111",
                1,
                None,
                "pw",
                Default::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, TransferError::DryRun), "{error:?}");
        assert!(
            state.broadcasts(10).is_empty(),
            "a refused transfer leaves no record and no signature"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn an_armed_core_refuses_a_network_it_cannot_reach() {
        let (dir, db) = temp_db();
        let mut config = config_with_db(&db);
        armed_only_in_config(&mut config);
        config.wallet.dry_run = false;
        config.solana_rpc_url = None;
        config.evm_rpc_urls.clear();
        let state = AppState::new(config).unwrap();

        let error = state
            .broadcast_transfer(
                "solana",
                "11111111111111111111111111111111",
                1,
                None,
                "pw",
                Default::default(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, TransferError::NetworkNotConfigured(_)),
            "{error:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn an_unknown_network_is_refused_before_anything_else() {
        let (dir, db) = temp_db();
        let mut config = config_with_db(&db);
        armed_only_in_config(&mut config);
        config.wallet.dry_run = false;
        let state = AppState::new(config).unwrap();

        let error = state
            .broadcast_transfer("dogecoin", "addr", 1, None, "pw", Default::default())
            .await
            .unwrap_err();
        assert!(
            matches!(error, TransferError::UnsupportedNetwork(_)),
            "{error:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /* ------------------------------------------------------- swaps */

    fn swap_request() -> crate::swap::SwapRequest {
        crate::swap::SwapRequest {
            network: "ethereum".into(),
            sell_token: "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".into(),
            buy_token: "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2".into(),
            sell_amount_raw: 1_000_000,
            slippage_bps: crate::swap::DEFAULT_SLIPPAGE_BPS,
        }
    }

    #[tokio::test]
    async fn an_unarmed_core_never_quotes_or_swaps() {
        let (dir, db) = temp_db();
        let mut config = config_with_db(&db);
        config.onchain_live = false;
        config.wallet.dry_run = false;
        let state = AppState::new(config).unwrap();

        let error = state
            .quote_swap(&swap_request(), &Default::default())
            .await
            .unwrap_err();
        assert!(
            matches!(error, SwapExecutionError::NotArmed(_)),
            "{error:?}"
        );
        assert!(state.broadcasts(10).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_dry_run_wallet_never_quotes_or_swaps() {
        let (dir, db) = temp_db();
        let mut config = config_with_db(&db);
        armed_only_in_config(&mut config);
        config.wallet.dry_run = true;
        let state = AppState::new(config).unwrap();

        let error = state
            .quote_swap(&swap_request(), &Default::default())
            .await
            .unwrap_err();
        assert!(matches!(error, SwapExecutionError::DryRun), "{error:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_core_without_a_quote_source_refuses_swaps() {
        let (dir, db) = temp_db();
        let mut config = config_with_db(&db);
        armed_only_in_config(&mut config);
        config.wallet.dry_run = false;
        config.swap_quote_url = None;
        let state = AppState::new(config).unwrap();

        let error = state
            .quote_swap(&swap_request(), &Default::default())
            .await
            .unwrap_err();
        assert!(matches!(error, SwapExecutionError::NoQuoter), "{error:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Gas is a cost the wallet paid. It is reported, derived from the final
    /// broadcasts, and never folded into the trading account's equity.
    #[tokio::test]
    async fn gas_is_booked_from_final_transfers_only() {
        let (dir, db) = temp_db();
        let state = AppState::new(config_with_db(&db)).unwrap();
        state.lock_data().wallet_holdings = Some(holdings::wallet_of(
            vec![crate::holdings::ChainHoldings {
                chain: "ethereum".into(),
                address: "addr".into(),
                native_symbol: "ETH".into(),
                native_balance: 1.0,
                native_price_usd: 2_000.0,
                native_value_usd: 2_000.0,
                tokens: vec![],
                tokens_skipped: 0,
                value_usd: 2_000.0,
                gas_spent_usd: 0.0,
                fetched_at_ms: now_ms(),
            }],
            true,
        ));

        // 0.001 ETH at $2,000 is $2 of gas.
        state.store_broadcast(confirmed_broadcast("a", "ethereum", 1_000_000_000_000_000));
        // A transfer that was replaced still carries a stale fee on its record;
        // it was never paid, so it must not be booked.
        let mut replaced = confirmed_broadcast("b", "ethereum", 5_000_000_000_000_000);
        replaced.state = BroadcastState::Reorged;
        state.store_broadcast(replaced);

        let portfolio = state.portfolio();
        assert!(
            (portfolio.wallet_gas_usd - 2.0).abs() < 1e-9,
            "only the final transfer's fee counts: {}",
            portfolio.wallet_gas_usd
        );
        assert_eq!(
            portfolio.equity_usd, 10_000.0,
            "gas is the wallet's cost, not the trading account's"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_broadcast_survives_a_restart() {
        let (dir, db) = temp_db();
        {
            let state = AppState::new(config_with_db(&db)).unwrap();
            state.store_broadcast(confirmed_broadcast("c", "solana", 5_000));
        }

        let state = AppState::new(config_with_db(&db)).unwrap();
        let broadcasts = state.broadcasts(10);
        assert_eq!(broadcasts.len(), 1);
        assert_eq!(broadcasts[0].id, "c");
        assert_eq!(broadcasts[0].state, BroadcastState::Confirmed);
        assert_eq!(broadcasts[0].fee_paid_raw, 5_000);
        std::fs::remove_dir_all(&dir).ok();
    }
}

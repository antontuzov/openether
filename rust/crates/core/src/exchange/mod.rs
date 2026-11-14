use crate::types::*;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Sink the connectors push market data into. The engine owns the receiving end
/// and fans the ticks out over the event bus.
pub type TickSink = tokio::sync::mpsc::UnboundedSender<MarketTick>;

#[derive(Debug, thiserror::Error)]
pub enum ExchangeError {
    #[error("network error: {0}")]
    Network(String),
    #[error("exchange rejected the order: {0}")]
    Rejected(String),
    #[error("insufficient balance for {asset}: need {needed}, have {available}")]
    InsufficientBalance {
        asset: String,
        needed: f64,
        available: f64,
    },
    #[error("unsupported operation: {0}")]
    Unsupported(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("rate limited: {0}")]
    RateLimited(String),
    #[error("configuration error: {0}")]
    Config(String),
    #[error("internal error: {0}")]
    Internal(String),
}

pub type ExchangeResult<T> = Result<T, ExchangeError>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenOrder {
    pub venue: Venue,
    pub order_id: String,
    pub symbol: String,
    pub side: Side,
    pub qty: f64,
    pub price: f64,
    pub ts_ms: i64,
}

/// A venue account captured for persistence and handed back after a restart.
///
/// A live venue keeps its balances on the exchange and re-reports them on
/// every call to `account()`, so it never needs this. The simulated venue has
/// no server to ask, so the engine captures its state here and restores it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AccountRestore {
    /// Spendable cash, i.e. the wallet balance minus the margin locked
    /// against open positions.
    pub cash_usd: f64,
    pub realized_pnl: f64,
    /// Symbol, signed quantity and average entry price.
    pub positions: Vec<(String, f64, f64)>,
}

/// Unified connector surface for every venue (CEX spot/perps and the paper simulator).
///
/// Implementations must never place live orders unless the process was started with
/// live trading explicitly enabled; see `crate::config::Config::live_trading_enabled`.
#[async_trait]
pub trait ExchangeConnector: Send + Sync {
    fn venue(&self) -> Venue;

    /// Cheap liveness probe used by the health endpoint.
    async fn health(&self) -> ExchangeResult<()>;

    /// Equity and balances for the configured account.
    async fn account(&self) -> ExchangeResult<AccountSnapshot>;

    /// Submit an order. The risk layer has already approved it; connectors must not
    /// re-interpret quantity semantics.
    async fn place_order(&self, req: &OrderRequest) -> ExchangeResult<OrderAck>;

    async fn cancel_order(&self, order_id: &str) -> ExchangeResult<()>;

    async fn open_orders(&self) -> ExchangeResult<Vec<OpenOrder>>;

    /// Stream trade ticks for `symbols` into `sink`. Returns once the stream is
    /// established; the task lives until the sink is closed.
    async fn subscribe_trades(&self, symbols: &[String], sink: TickSink) -> ExchangeResult<()>;

    /// Last traded price for a symbol, used by the risk layer for notional checks
    /// when a position is opened without a live quote.
    async fn last_price(&self, symbol: &str) -> ExchangeResult<f64> {
        let _ = symbol;
        Err(ExchangeError::Unsupported(
            "last_price is not implemented for this venue".into(),
        ))
    }

    /// Fills the venue has confirmed since the given timestamp.
    ///
    /// The engine books only what this returns. An empty answer means nothing
    /// filled — it is never turned into a synthetic fill, because a resting
    /// limit order would then become a position that does not exist. A venue
    /// that cannot report fills keeps the default and its orders stay
    /// unreconciled rather than invented.
    async fn fills_since(&self, _since_ms: i64) -> ExchangeResult<Vec<OrderFill>> {
        Ok(Vec::new())
    }

    /// Open positions as reported by the venue.
    async fn positions(&self) -> ExchangeResult<Vec<Position>> {
        Ok(Vec::new())
    }

    /// The venue's account, in the form it can be restored from.
    ///
    /// `None` means the venue keeps its own balances somewhere the engine
    /// cannot reach — a live exchange re-reports them on every call to
    /// `account()`. The simulated venue has no such server, so it hands the
    /// engine its state here instead.
    fn account_state(&self) -> Option<AccountRestore> {
        None
    }

    /// Hand a persisted account back to the venue after a restart.
    ///
    /// The default does nothing, because a live venue has nothing to restore:
    /// its balances never left the exchange.
    fn restore_account(&self, _account: &AccountRestore) -> ExchangeResult<()> {
        Ok(())
    }
}

/// Common knobs every REST connector honours.
#[derive(Debug, Clone)]
pub struct ConnectorConfig {
    pub base_url: String,
    pub ws_url: String,
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
    pub recv_timeout: Duration,
    pub max_inflight: usize,
}

impl Default for ConnectorConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            ws_url: String::new(),
            api_key: None,
            api_secret: None,
            recv_timeout: Duration::from_secs(10),
            max_inflight: 4,
        }
    }
}

impl ConnectorConfig {
    pub fn has_credentials(&self) -> bool {
        self.api_key.is_some() && self.api_secret.is_some()
    }
}

pub mod binance;
pub mod hyperliquid;
pub mod kraken;
pub mod paper;

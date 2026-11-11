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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenOrder {
    pub venue: Venue,
    pub order_id: String,
    pub symbol: String,
    pub side: Side,
    pub qty: f64,
    pub price: f64,
    pub ts_ms: i64,
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

    /// Fills produced since the given timestamp.
    ///
    /// Venues that stream fills should override this; the engine falls back to
    /// the last observed price when a venue reports no fills.
    async fn fills_since(&self, _since_ms: i64) -> ExchangeResult<Vec<OrderFill>> {
        Ok(Vec::new())
    }

    /// Open positions as reported by the venue.
    async fn positions(&self) -> ExchangeResult<Vec<Position>> {
        Ok(Vec::new())
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

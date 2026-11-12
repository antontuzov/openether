use serde::{Deserialize, Serialize};
use std::fmt;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Venue {
    Paper,
    Binance,
    Kraken,
    Hyperliquid,
}

impl Venue {
    pub const ALL: [Venue; 4] = [
        Venue::Paper,
        Venue::Binance,
        Venue::Kraken,
        Venue::Hyperliquid,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Venue::Paper => "paper",
            Venue::Binance => "binance",
            Venue::Kraken => "kraken",
            Venue::Hyperliquid => "hyperliquid",
        }
    }

    pub fn parse(s: &str) -> Option<Venue> {
        match s.to_ascii_lowercase().as_str() {
            "paper" => Some(Venue::Paper),
            "binance" => Some(Venue::Binance),
            "kraken" => Some(Venue::Kraken),
            "hyperliquid" => Some(Venue::Hyperliquid),
            _ => None,
        }
    }

    /// `true` when the venue can place real orders.
    ///
    /// The paper venue never trades live and Hyperliquid is read-only in this
    /// build, so neither of them can be armed.
    pub fn trades_live(self) -> bool {
        matches!(self, Venue::Binance | Venue::Kraken)
    }
}

impl fmt::Display for Venue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn sign(self) -> f64 {
        match self {
            Side::Buy => 1.0,
            Side::Sell => -1.0,
        }
    }

    pub fn opposite(self) -> Side {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Side::Buy => "buy",
            Side::Sell => "sell",
        }
    }

    pub fn parse(s: &str) -> Option<Side> {
        match s.to_ascii_lowercase().as_str() {
            "buy" | "long" => Some(Side::Buy),
            "sell" | "short" => Some(Side::Sell),
            _ => None,
        }
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrderType {
    #[default]
    Market,
    Limit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    Accepted,
    Filled,
    Rejected,
    Cancelled,
    PartiallyFilled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Chain {
    Evm,
    Solana,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarketTick {
    pub venue: Venue,
    pub symbol: String,
    pub price: f64,
    pub size: f64,
    pub ts_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderRequest {
    pub venue: Venue,
    pub symbol: String,
    pub side: Side,
    pub qty: f64,
    #[serde(default)]
    pub order_type: OrderType,
    #[serde(default)]
    pub limit_price: Option<f64>,
    #[serde(default)]
    pub reduce_only: bool,
    #[serde(default = "default_client_id")]
    pub client_id: String,
    /// Human-readable reasoning captured from the agent/strategy; stored with the trade.
    #[serde(default)]
    pub rationale: String,
    #[serde(default)]
    pub reduce_only_guard: bool,
}

fn default_client_id() -> String {
    format!("oe-{}", uuid::Uuid::new_v4().simple())
}

impl OrderRequest {
    pub fn market(venue: Venue, symbol: impl Into<String>, side: Side, qty: f64) -> Self {
        Self {
            venue,
            symbol: symbol.into(),
            side,
            qty,
            order_type: OrderType::Market,
            limit_price: None,
            reduce_only: false,
            client_id: default_client_id(),
            rationale: String::new(),
            reduce_only_guard: false,
        }
    }

    pub fn notional(&self, price: f64) -> f64 {
        price * self.qty.abs()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderAck {
    pub venue: Venue,
    pub order_id: String,
    pub client_id: String,
    pub status: OrderStatus,
    pub accepted_ts_ms: i64,
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderFill {
    pub venue: Venue,
    pub order_id: String,
    pub symbol: String,
    pub side: Side,
    pub qty: f64,
    pub price: f64,
    pub fee: f64,
    pub ts_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Balance {
    pub asset: String,
    pub free: f64,
    pub locked: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountSnapshot {
    pub venue: Venue,
    pub equity_usd: f64,
    pub free_usd: f64,
    pub balances: Vec<Balance>,
    pub ts_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub venue: Venue,
    pub symbol: String,
    /// Signed size: positive = long, negative = short.
    pub qty: f64,
    pub avg_entry: f64,
    pub mark_price: f64,
    pub unrealized_pnl: f64,
    pub notional_usd: f64,
    pub ts_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TradeRecord {
    pub id: String,
    pub venue: Venue,
    pub symbol: String,
    pub side: Side,
    pub qty: f64,
    pub price: f64,
    pub fee: f64,
    pub realized_pnl: f64,
    pub ts_ms: i64,
    pub rationale: String,
    /// `agent` | `strategy` | `manual`
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortfolioSnapshot {
    pub venue: Venue,
    pub equity_usd: f64,
    pub cash_usd: f64,
    pub unrealized_pnl_usd: f64,
    pub realized_pnl_usd: f64,
    pub total_pnl_pct: f64,
    pub positions: Vec<Position>,
    pub ts_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogEntry {
    pub id: String,
    pub ts_ms: i64,
    /// `agent` | `risk` | `strategy` | `engine` | `mcp`
    pub source: String,
    pub level: String,
    pub message: String,
    #[serde(default)]
    pub context: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn venue_roundtrip() {
        for v in Venue::ALL {
            assert_eq!(Venue::parse(v.as_str()), Some(v));
        }
        assert_eq!(Venue::parse("nope"), None);
    }

    #[test]
    fn side_sign_matches_opposite() {
        assert_eq!(Side::Buy.sign(), 1.0);
        assert_eq!(Side::Sell.sign(), -1.0);
        assert_eq!(Side::Buy.opposite(), Side::Sell);
        assert_eq!(Side::Sell.opposite(), Side::Buy);
    }

    #[test]
    fn market_order_builds_unique_client_id() {
        let a = OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Buy, 1.0);
        let b = OrderRequest::market(Venue::Paper, "SOL/USDT", Side::Buy, 1.0);
        assert_ne!(a.client_id, b.client_id);
        assert!(a.client_id.starts_with("oe-"));
    }
}

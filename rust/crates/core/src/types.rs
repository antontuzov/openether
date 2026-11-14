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
#[serde(rename_all = "snake_case")]
pub enum OrderType {
    #[default]
    Market,
    Limit,
    /// A resting order that fires as a market order once the price reaches a
    /// trigger: at or below it for a sell, at or above it for a buy.
    Stop,
    /// A resting order that fires as a market order once a profit target is
    /// reached: at or above it for a sell, at or below it for a buy.
    TakeProfit,
}

impl OrderType {
    pub fn as_str(self) -> &'static str {
        match self {
            OrderType::Market => "market",
            OrderType::Limit => "limit",
            OrderType::Stop => "stop",
            OrderType::TakeProfit => "take_profit",
        }
    }

    /// A conditional order waits for a price instead of resting at one.
    pub fn is_conditional(self) -> bool {
        matches!(self, OrderType::Stop | OrderType::TakeProfit)
    }

    /// Whether this conditional order fires at `price`.
    ///
    /// A stop protects a position by cutting a loss, so a sell stop fires when
    /// the price falls to the trigger and a buy stop when it rises. A
    /// take-profit is the mirror image. Anything else never fires.
    pub fn trigger_hit(self, side: Side, trigger: f64, price: f64) -> bool {
        match (self, side) {
            (OrderType::Stop, Side::Sell) => price <= trigger,
            (OrderType::Stop, Side::Buy) => price >= trigger,
            (OrderType::TakeProfit, Side::Sell) => price >= trigger,
            (OrderType::TakeProfit, Side::Buy) => price <= trigger,
            _ => false,
        }
    }
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

impl OrderStatus {
    /// The name the API serialises, so logs and wire payloads agree.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Filled => "filled",
            Self::Rejected => "rejected",
            Self::Cancelled => "cancelled",
            Self::PartiallyFilled => "partially_filled",
        }
    }
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
    /// The price a `stop` or `take_profit` order waits for.
    #[serde(default)]
    pub trigger_price: Option<f64>,
    /// Orders sharing a group are one-cancels-the-other: when one fills, the
    /// venue cancels the rest.
    #[serde(default)]
    pub oco_group: Option<String>,
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
            trigger_price: None,
            oco_group: None,
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

/// An order the engine submitted and is still reconciling with the venue.
///
/// The venue owns the truth about what filled. The engine books only fills the
/// venue reports, so a resting limit order is never mistaken for a position and
/// a partial fill never books the whole quantity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackedOrder {
    pub venue: Venue,
    pub order_id: String,
    pub client_id: String,
    pub symbol: String,
    pub side: Side,
    pub order_type: OrderType,
    #[serde(default)]
    pub limit_price: Option<f64>,
    /// The price a conditional order waits for, as submitted.
    #[serde(default)]
    pub trigger_price: Option<f64>,
    /// The one-cancels-the-other group this order belongs to, if any.
    #[serde(default)]
    pub oco_group: Option<String>,
    /// The quantity asked for.
    pub qty: f64,
    pub status: OrderStatus,
    #[serde(default)]
    pub rationale: String,
    pub submitted_ts_ms: i64,
    pub updated_ts_ms: i64,
    /// Every fill booked for this order, oldest first.
    ///
    /// This is also the dedupe key: reconciliation runs repeatedly over an
    /// overlapping window, and a fill must be booked exactly once.
    pub fills: Vec<OrderFill>,
    /// When the venue's open list first stopped containing this order.
    ///
    /// An order disappears from the open list before its fills show up in the
    /// trade history, so absence alone is not proof of cancellation. The engine
    /// waits out a grace window before retiring it, and books any fill the
    /// venue confirms in the meantime.
    #[serde(default)]
    pub missing_since_ms: Option<i64>,
}

impl TrackedOrder {
    /// Cumulative quantity the venue has confirmed.
    pub fn filled_qty(&self) -> f64 {
        self.fills.iter().fold(0.0, |total, fill| total + fill.qty)
    }

    /// Quantity still resting at the venue.
    pub fn remaining_qty(&self) -> f64 {
        (self.qty - self.filled_qty()).max(0.0)
    }

    /// Volume-weighted average of the confirmed fills, or 0 when none.
    pub fn avg_fill_price(&self) -> f64 {
        let notional = self
            .fills
            .iter()
            .fold(0.0, |total, fill| total + fill.qty * fill.price);
        let qty = self.filled_qty();
        if qty > 0.0 {
            notional / qty
        } else {
            0.0
        }
    }

    pub fn fee(&self) -> f64 {
        self.fills.iter().fold(0.0, |total, fill| total + fill.fee)
    }

    /// Still waiting on the venue: neither fully filled nor retired.
    pub fn is_active(&self) -> bool {
        matches!(
            self.status,
            OrderStatus::Accepted | OrderStatus::PartiallyFilled
        )
    }

    /// Whether this exact fill has already been booked.
    ///
    /// Venues give fills no stable id of their own, so identity is the
    /// timestamp, quantity and price together.
    pub fn knows_fill(&self, fill: &OrderFill) -> bool {
        self.fills.iter().any(|known| {
            known.ts_ms == fill.ts_ms && known.qty == fill.qty && known.price == fill.price
        })
    }
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
    /// The value of the on-chain wallet, in dollars.
    ///
    /// It is reported, not traded: the risk layer still measures equity on the
    /// trading account alone, because there is no broadcast path for the
    /// wallet yet. Mixing the two would let an unspendable balance widen the
    /// limits of the account that can actually lose money.
    #[serde(default)]
    pub onchain_value_usd: f64,
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

    #[test]
    fn a_stop_and_a_take_profit_fire_on_opposite_sides_of_the_market() {
        // A stop sells through a falling price and buys through a rising one.
        assert!(OrderType::Stop.trigger_hit(Side::Sell, 130.0, 129.9));
        assert!(!OrderType::Stop.trigger_hit(Side::Sell, 130.0, 130.1));
        assert!(OrderType::Stop.trigger_hit(Side::Buy, 150.0, 150.0));
        assert!(!OrderType::Stop.trigger_hit(Side::Buy, 150.0, 149.9));

        // A take-profit is the mirror image.
        assert!(OrderType::TakeProfit.trigger_hit(Side::Sell, 160.0, 161.0));
        assert!(!OrderType::TakeProfit.trigger_hit(Side::Sell, 160.0, 159.0));
        assert!(OrderType::TakeProfit.trigger_hit(Side::Buy, 90.0, 89.0));
        assert!(!OrderType::TakeProfit.trigger_hit(Side::Buy, 90.0, 91.0));

        // Only conditional orders wait for a trigger.
        assert!(!OrderType::Market.trigger_hit(Side::Sell, 130.0, 1.0));
        assert!(!OrderType::Limit.trigger_hit(Side::Sell, 130.0, 1.0));
    }

    #[test]
    fn order_types_serialise_with_the_names_the_api_uses() {
        for (order_type, wire) in [
            (OrderType::Market, "\"market\""),
            (OrderType::Limit, "\"limit\""),
            (OrderType::Stop, "\"stop\""),
            (OrderType::TakeProfit, "\"take_profit\""),
        ] {
            assert_eq!(serde_json::to_string(&order_type).unwrap(), wire);
            assert_eq!(serde_json::from_str::<OrderType>(wire).unwrap(), order_type);
        }
    }
}

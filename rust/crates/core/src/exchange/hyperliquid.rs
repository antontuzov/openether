//! Hyperliquid connector — **read-only**.
//!
//! # Limitation: no order placement
//!
//! Placing (or cancelling) an order on Hyperliquid requires the exchange
//! action to be signed with an EIP-712 payload over the secp256k1 private key
//! of the trading wallet. This connector does **not** implement that signer:
//! it deliberately refuses to place orders instead of pretending that a
//! transaction succeeded.
//!
//! `place_order` and `cancel_order` therefore always return
//! `ExchangeError::Unsupported("hyperliquid order placement requires an
//! EIP-712 signer; use the paper venue or a CEX connector")`. Use the paper
//! venue or one of the CEX connectors for execution.
//!
//! # What is implemented
//!
//! * Market data: websocket `trades` channel
//!   (`{"method":"subscribe","params":{"channel":"trades","coin":"SOL"}}`).
//! * Account: `POST /info {"type":"clearinghouseState","user":<address>}` —
////!   `marginSummary.accountValue` and `assetPositions` are decomposed into an
//!   [`AccountSnapshot`] and [`Position`]s.
//! * Open orders: `POST /info {"type":"openOrders","user":<address>}`.
//! * Last price: `POST /info {"type":"allMids"}`.
//!
//! # Symbol conventions
//!
//! Hyperliquid quotes perps in USDC while the rest of the core uses the
//! canonical `BASE/USDT` symbols. The connector maps `SOL/USDT` to the coin
//! `SOL` on the wire and reports positions, orders and ticks with the
//! canonical symbol so that the risk layer sees a single symbol namespace.
//!
//! # Live trading guard
//!
//! The guard is checked before the unsupported error: while live trading is
//! disabled the connector refuses order flow with a configuration error, which
//! makes the reason for the refusal unambiguous.

use crate::exchange::{
    ConnectorConfig, ExchangeConnector, ExchangeError, ExchangeResult, OpenOrder, TickSink,
};
use crate::types::*;
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use reqwest::Method;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

pub const DEFAULT_BASE_URL: &str = "https://api.hyperliquid.xyz";
pub const DEFAULT_WS_URL: &str = "wss://api.hyperliquid.xyz/ws";
/// Message returned when live trading is switched off.
pub const LIVE_TRADING_DISABLED: &str =
    "live trading is not armed; run `openether-live arm` and set OPENETHER_LIVE_TRADING=1";
/// Message returned for every order flow operation.
pub const UNSUPPORTED_ORDER_FLOW: &str = "hyperliquid order placement requires an EIP-712 signer; use the paper venue or a CEX connector";

/// Coins the core trades; used to map exchange coin names back to the
/// canonical `BASE/USDT` symbols.
const CANONICAL_COINS: [(&str, &str); 3] = [
    ("SOL", "SOL/USDT"),
    ("BTC", "BTC/USDT"),
    ("ETH", "ETH/USDT"),
];

/// `SOL/USDT` → `SOL`.
pub fn coin_for(symbol: &str) -> String {
    let upper = symbol.trim().to_ascii_uppercase();
    let base = upper.split('/').next().unwrap_or(&upper);
    let base = match base {
        "XBT" => "BTC",
        other => other,
    };
    base.to_string()
}

/// `SOL` → `SOL/USDT`.
pub fn canonical_symbol(coin: &str) -> String {
    let upper = coin.trim().to_ascii_uppercase();
    for (name, symbol) in CANONICAL_COINS {
        if name == upper {
            return symbol.to_string();
        }
    }
    format!("{upper}/USDT")
}

/// `true` when the process was started with live trading enabled.
/// Read-only Hyperliquid connector.
#[derive(Clone)]
pub struct HyperliquidConnector {
    http: reqwest::Client,
    cfg: ConnectorConfig,
    wallet_address: String,
    live_enabled: bool,
}

impl std::fmt::Debug for HyperliquidConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HyperliquidConnector")
            .field("base_url", &self.cfg.base_url)
            .field("ws_url", &self.cfg.ws_url)
            .field("wallet_address", &self.wallet_address)
            .field("live_enabled", &self.live_enabled)
            .finish()
    }
}

impl HyperliquidConnector {
    /// Build a connector from the environment.
    ///
    /// Reads `HYPERLIQUID_WALLET_ADDRESS`, `HYPERLIQUID_BASE_URL` (default
    /// `https://api.hyperliquid.xyz`), `HYPERLIQUID_WS_URL` (default
    /// `wss://api.hyperliquid.xyz/ws`). `live_enabled` is the decision of the
    /// arming ceremony, not of an environment variable.
    pub fn from_env(live_enabled: bool) -> ExchangeResult<Self> {
        let wallet_address = env_var("HYPERLIQUID_WALLET_ADDRESS").unwrap_or_default();
        let cfg = ConnectorConfig {
            base_url: env_var("HYPERLIQUID_BASE_URL")
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            ws_url: env_var("HYPERLIQUID_WS_URL").unwrap_or_else(|| DEFAULT_WS_URL.to_string()),
            api_key: None,
            api_secret: None,
            recv_timeout: Duration::from_millis(
                env_var("HYPERLIQUID_RECV_TIMEOUT_MS")
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(10_000),
            ),
            max_inflight: 4,
        };
        Self::new(cfg, wallet_address, live_enabled)
    }

    /// Build a connector with an explicit configuration.
    pub fn new(
        cfg: ConnectorConfig,
        wallet_address: String,
        live_enabled: bool,
    ) -> ExchangeResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(cfg.recv_timeout)
            .user_agent(concat!("openether/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| ExchangeError::Config(format!("could not build the http client: {e}")))?;
        Ok(Self {
            http,
            cfg,
            wallet_address,
            live_enabled,
        })
    }

    pub fn config(&self) -> &ConnectorConfig {
        &self.cfg
    }

    pub fn wallet_address(&self) -> &str {
        &self.wallet_address
    }

    pub fn live_enabled(&self) -> bool {
        self.live_enabled
    }

    fn ensure_live(&self) -> ExchangeResult<()> {
        if self.live_enabled {
            Ok(())
        } else {
            Err(ExchangeError::Config(LIVE_TRADING_DISABLED.to_string()))
        }
    }

    fn require_wallet(&self) -> ExchangeResult<&str> {
        if self.wallet_address.trim().is_empty() {
            return Err(ExchangeError::Config(
                "HYPERLIQUID_WALLET_ADDRESS is not configured".into(),
            ));
        }
        Ok(self.wallet_address.trim())
    }

    /// `POST /info` with a JSON body.
    async fn post_info(&self, payload: Value) -> ExchangeResult<Value> {
        let url = format!("{}/info", self.cfg.base_url.trim_end_matches('/'));
        let request = self
            .http
            .request(Method::POST, &url)
            .header("Content-Type", "application/json")
            .json(&payload);
        let response = tokio::time::timeout(self.cfg.recv_timeout, request.send())
            .await
            .map_err(|_| ExchangeError::Network("hyperliquid request timed out".into()))?
            .map_err(|e| ExchangeError::Network(format!("hyperliquid request failed: {e}")))?;
        let status = response.status();
        let text = tokio::time::timeout(self.cfg.recv_timeout, response.text())
            .await
            .map_err(|_| ExchangeError::Network("hyperliquid response timed out".into()))?
            .unwrap_or_default();
        if !status.is_success() {
            return Err(map_hyperliquid_http_error(status.as_u16(), &text));
        }
        serde_json::from_str(&text)
            .map_err(|e| ExchangeError::Internal(format!("hyperliquid returned invalid json: {e}")))
    }

    /// Mid prices of every coin.
    async fn all_mids(&self) -> ExchangeResult<HashMap<String, f64>> {
        let json = self.post_info(json!({ "type": "allMids" })).await?;
        let mut mids = HashMap::new();
        if let Some(map) = json.as_object() {
            for (coin, price) in map {
                let parsed = match price {
                    Value::String(s) => s.parse::<f64>().ok(),
                    Value::Number(n) => n.as_f64(),
                    _ => None,
                };
                if let Some(p) = parsed {
                    mids.insert(coin.to_ascii_uppercase(), p);
                }
            }
        }
        Ok(mids)
    }

    async fn consume_stream(
        &self,
        mut stream: tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        symbols: &[String],
        sink: &TickSink,
    ) -> ExchangeResult<()> {
        while let Some(next) = stream.next().await {
            let msg = match next {
                Ok(msg) => msg,
                Err(e) => {
                    tracing::warn!(venue = "hyperliquid", error = %e, "websocket error");
                    return Err(ExchangeError::Network(format!(
                        "hyperliquid websocket error: {e}"
                    )));
                }
            };
            let payload = match msg {
                Message::Text(text) => text.as_str().to_string(),
                Message::Binary(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                Message::Close(_) => {
                    tracing::warn!(venue = "hyperliquid", "websocket closed by the server");
                    return Ok(());
                }
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
            };
            match parse_trades_message(&payload) {
                Some(ticks) => {
                    for tick in ticks {
                        let wanted =
                            symbols.is_empty() || symbols.iter().any(|s| s == &tick.symbol);
                        if wanted && sink.send(tick).is_err() {
                            tracing::debug!(venue = "hyperliquid", "tick sink closed");
                            return Ok(());
                        }
                    }
                }
                None => {
                    tracing::debug!(
                        venue = "hyperliquid",
                        "ignoring unparsable websocket payload"
                    );
                }
            }
        }
        Ok(())
    }

    /// Reconnecting websocket loop; runs until the sink is closed.
    async fn stream_loop(&self, symbols: Vec<String>, sink: TickSink) {
        let mut backoff = Duration::from_secs(1);
        let max_backoff = Duration::from_secs(30);
        let coins: Vec<String> = symbols.iter().map(|s| coin_for(s)).collect();
        loop {
            let url = self.cfg.ws_url.trim_end_matches('/').to_string();
            let connect = tokio_tungstenite::connect_async(&url);
            match tokio::time::timeout(Duration::from_secs(15), connect).await {
                Ok(Ok((mut stream, _))) => {
                    tracing::info!(venue = "hyperliquid", url = %url, "market data stream connected");
                    for coin in &coins {
                        let subscribe = json!({
                            "method": "subscribe",
                            "params": { "channel": "trades", "coin": coin }
                        });
                        if let Err(e) = stream
                            .send(Message::Text(subscribe.to_string().into()))
                            .await
                        {
                            tracing::warn!(
                                venue = "hyperliquid",
                                coin = %coin,
                                error = %e,
                                "could not subscribe to the trades channel"
                            );
                        }
                    }
                    backoff = Duration::from_secs(1);
                    if let Err(err) = self.consume_stream(stream, &symbols, &sink).await {
                        tracing::warn!(venue = "hyperliquid", error = %err, "market data stream ended");
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!(venue = "hyperliquid", error = %e, "websocket connection failed");
                }
                Err(_) => {
                    tracing::warn!(venue = "hyperliquid", "websocket connection timed out");
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(max_backoff);
        }
    }
}

fn env_var(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Map an HTTP error to the connector error type.
pub fn map_hyperliquid_http_error(status: u16, body: &str) -> ExchangeError {
    match status {
        401 | 403 => ExchangeError::Auth(format!("hyperliquid auth error {status}: {body}")),
        429 => ExchangeError::RateLimited(format!("hyperliquid rate limited: {body}")),
        400..=499 => {
            ExchangeError::Rejected(format!("hyperliquid rejected the request {status}: {body}"))
        }
        500..=599 => ExchangeError::Network(format!("hyperliquid server error {status}: {body}")),
        _ => ExchangeError::Network(format!("hyperliquid returned http {status}: {body}")),
    }
}

/// Parse a Hyperliquid `trades` websocket payload into ticks.
pub fn parse_trades_message(payload: &str) -> Option<Vec<MarketTick>> {
    let root: Value = serde_json::from_str(payload).ok()?;
    if root.get("channel").and_then(Value::as_str) != Some("trades") {
        return None;
    }
    let data = root.get("data")?.as_array()?;
    let mut ticks = Vec::with_capacity(data.len());
    for entry in data {
        let coin = entry.get("coin").and_then(Value::as_str)?;
        let price = entry.get("px").and_then(value_as_f64)?;
        let size = entry.get("sz").and_then(value_as_f64).unwrap_or(0.0);
        // The payload carries a side (`A` = ask/sell, `B` = bid/buy); ticks
        // are side agnostic so it is only logged.
        if let Some(side) = entry.get("side").and_then(Value::as_str) {
            tracing::trace!(venue = "hyperliquid", coin = %coin, side = %side, "trade");
        }
        let ts_ms = entry
            .get("time")
            .and_then(value_as_f64)
            .map(|t| t as i64)
            .unwrap_or_else(now_ms);
        ticks.push(MarketTick {
            venue: Venue::Hyperliquid,
            symbol: canonical_symbol(coin),
            price,
            size,
            ts_ms,
        });
    }
    Some(ticks)
}

/// Decompose a `clearinghouseState` payload into an account snapshot.
pub fn parse_clearinghouse_state(
    json: &Value,
    mids: &HashMap<String, f64>,
) -> (AccountSnapshot, Vec<Position>) {
    let margin = json.get("marginSummary");
    let account_value = margin
        .and_then(|m| m.get("accountValue"))
        .and_then(value_as_f64)
        .unwrap_or(0.0);
    let margin_used = margin
        .and_then(|m| m.get("totalMarginUsed"))
        .and_then(value_as_f64)
        .unwrap_or(0.0);
    let free = (account_value - margin_used).max(0.0);

    let mut positions = Vec::new();
    if let Some(list) = json.get("assetPositions").and_then(Value::as_array) {
        for entry in list {
            let Some(position) = entry.get("position") else {
                continue;
            };
            let Some(coin) = position.get("coin").and_then(Value::as_str) else {
                continue;
            };
            let qty = position.get("szi").and_then(value_as_f64).unwrap_or(0.0);
            let avg_entry = position
                .get("entryPx")
                .and_then(value_as_f64)
                .unwrap_or(0.0);
            let upper_coin = coin.to_ascii_uppercase();
            let mark = mids
                .get(&upper_coin)
                .copied()
                .or_else(|| position.get("positionValue").and_then(value_as_f64))
                .unwrap_or(avg_entry);
            let unrealized = position
                .get("unrealizedPnl")
                .and_then(value_as_f64)
                .unwrap_or((mark - avg_entry) * qty);
            let notional = position
                .get("positionValue")
                .and_then(value_as_f64)
                .unwrap_or(qty.abs() * mark);
            positions.push(Position {
                venue: Venue::Hyperliquid,
                symbol: canonical_symbol(coin),
                qty,
                avg_entry,
                mark_price: mark,
                unrealized_pnl: unrealized,
                notional_usd: notional,
                ts_ms: now_ms(),
            });
        }
    }

    let snapshot = AccountSnapshot {
        venue: Venue::Hyperliquid,
        equity_usd: account_value,
        free_usd: free,
        balances: vec![Balance {
            asset: "USDC".to_string(),
            free,
            locked: margin_used,
        }],
        ts_ms: now_ms(),
    };
    (snapshot, positions)
}

/// Parse an `openOrders` payload into open orders.
pub fn parse_open_orders(json: &Value) -> Vec<OpenOrder> {
    let mut out = Vec::new();
    let Some(list) = json.as_array() else {
        return out;
    };
    for entry in list {
        let coin = entry
            .get("coin")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let side = match entry.get("side").and_then(Value::as_str) {
            Some("A") => Side::Sell,
            Some("B") => Side::Buy,
            Some(other) => Side::parse(other).unwrap_or(Side::Buy),
            None => Side::Buy,
        };
        let price = entry.get("limitPx").and_then(value_as_f64).unwrap_or(0.0);
        let qty = entry.get("sz").and_then(value_as_f64).unwrap_or(0.0);
        let order_id = entry
            .get("oid")
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default();
        let ts_ms = entry
            .get("timestamp")
            .and_then(value_as_f64)
            .map(|t| t as i64)
            .unwrap_or_else(now_ms);
        out.push(OpenOrder {
            venue: Venue::Hyperliquid,
            order_id,
            symbol: canonical_symbol(coin),
            side,
            qty,
            price,
            ts_ms,
        });
    }
    out
}

/// Numbers arrive either as JSON numbers or as strings.
fn value_as_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

#[async_trait]
impl ExchangeConnector for HyperliquidConnector {
    fn venue(&self) -> Venue {
        Venue::Hyperliquid
    }

    async fn health(&self) -> ExchangeResult<()> {
        self.post_info(json!({ "type": "allMids" }))
            .await
            .map(|_| ())
    }

    async fn account(&self) -> ExchangeResult<AccountSnapshot> {
        let user = self.require_wallet()?;
        let json = self
            .post_info(json!({ "type": "clearinghouseState", "user": user }))
            .await?;
        let mids = self.all_mids().await.unwrap_or_default();
        let (snapshot, _) = parse_clearinghouse_state(&json, &mids);
        Ok(snapshot)
    }

    async fn place_order(&self, _req: &OrderRequest) -> ExchangeResult<OrderAck> {
        // The guard comes first: while live trading is disabled the refusal is
        // a configuration error, afterwards it is the missing signer.
        self.ensure_live()?;
        Err(ExchangeError::Unsupported(
            UNSUPPORTED_ORDER_FLOW.to_string(),
        ))
    }

    async fn cancel_order(&self, _order_id: &str) -> ExchangeResult<()> {
        self.ensure_live()?;
        Err(ExchangeError::Unsupported(
            UNSUPPORTED_ORDER_FLOW.to_string(),
        ))
    }

    async fn open_orders(&self) -> ExchangeResult<Vec<OpenOrder>> {
        let user = self.require_wallet()?;
        let json = self
            .post_info(json!({ "type": "openOrders", "user": user }))
            .await?;
        Ok(parse_open_orders(&json))
    }

    async fn positions(&self) -> ExchangeResult<Vec<Position>> {
        let user = self.require_wallet()?;
        let json = self
            .post_info(json!({ "type": "clearinghouseState", "user": user }))
            .await?;
        let mids = self.all_mids().await.unwrap_or_default();
        let (_, positions) = parse_clearinghouse_state(&json, &mids);
        Ok(positions)
    }

    async fn subscribe_trades(&self, symbols: &[String], sink: TickSink) -> ExchangeResult<()> {
        if symbols.is_empty() {
            return Err(ExchangeError::InvalidRequest(
                "at least one symbol is required for the trade stream".into(),
            ));
        }
        let symbols = symbols.to_vec();
        let connector = self.clone();
        tokio::spawn(async move {
            connector.stream_loop(symbols, sink).await;
        });
        Ok(())
    }

    async fn last_price(&self, symbol: &str) -> ExchangeResult<f64> {
        let coin = coin_for(symbol);
        let mids = self.all_mids().await?;
        mids.get(&coin).copied().ok_or_else(|| {
            ExchangeError::Internal(format!("hyperliquid has no mid price for {coin}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coin_mapping() {
        assert_eq!(coin_for("SOL/USDT"), "SOL");
        assert_eq!(coin_for("BTC/USDT"), "BTC");
        assert_eq!(coin_for("SOL/USD"), "SOL");
        assert_eq!(canonical_symbol("SOL"), "SOL/USDT");
        assert_eq!(canonical_symbol("BTC"), "BTC/USDT");
    }

    #[test]
    fn trades_message_parsing() {
        let payload = r#"{"channel":"trades","data":[{"coin":"SOL","side":"A","px":"142.3","sz":"1.0","time":1700000000000,"tid":7}]}"#;
        let ticks = parse_trades_message(payload).expect("ticks");
        assert_eq!(ticks.len(), 1);
        assert_eq!(ticks[0].venue, Venue::Hyperliquid);
        assert_eq!(ticks[0].symbol, "SOL/USDT");
        assert!((ticks[0].price - 142.3).abs() < 1e-9);
        assert!((ticks[0].size - 1.0).abs() < 1e-9);
        assert_eq!(ticks[0].ts_ms, 1700000000000);
    }

    #[test]
    fn malformed_trades_messages_are_skipped() {
        assert!(parse_trades_message("not json").is_none());
        assert!(parse_trades_message(r#"{"channel":"l2Book"}"#).is_none());
        assert!(parse_trades_message(r#"{"channel":"trades","data":[]}"#)
            .expect("empty list")
            .is_empty());
    }

    #[test]
    fn clearinghouse_state_is_decomposed() {
        let payload = json!({
            "marginSummary": {"accountValue": "1000.0", "totalMarginUsed": "100.0", "totalNtlPos": "500.0"},
            "assetPositions": [{
                "position": {
                    "coin": "SOL", "szi": "-2.0", "entryPx": "142.0",
                    "positionValue": "284.0", "unrealizedPnl": "10.0"
                },
                "type": "oneWay"
            }]
        });
        let mids = HashMap::from([("SOL".to_string(), 150.0)]);
        let (snapshot, positions) = parse_clearinghouse_state(&payload, &mids);
        assert_eq!(snapshot.venue, Venue::Hyperliquid);
        assert!((snapshot.equity_usd - 1000.0).abs() < 1e-9);
        assert!((snapshot.free_usd - 900.0).abs() < 1e-9);
        assert_eq!(snapshot.balances.len(), 1);
        assert_eq!(snapshot.balances[0].asset, "USDC");
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].symbol, "SOL/USDT");
        assert!((positions[0].qty + 2.0).abs() < 1e-9);
        assert!((positions[0].avg_entry - 142.0).abs() < 1e-9);
        assert!((positions[0].mark_price - 150.0).abs() < 1e-9);
        assert!((positions[0].unrealized_pnl - 10.0).abs() < 1e-9);
    }

    #[test]
    fn open_orders_are_parsed() {
        let payload = json!([
            {"oid": 123, "coin": "SOL", "side": "B", "limitPx": "140.0", "sz": "1.0", "timestamp": 1700000000000i64}
        ]);
        let orders = parse_open_orders(&payload);
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].venue, Venue::Hyperliquid);
        assert_eq!(orders[0].order_id, "123");
        assert_eq!(orders[0].symbol, "SOL/USDT");
        assert_eq!(orders[0].side, Side::Buy);
        assert!((orders[0].price - 140.0).abs() < 1e-9);
        assert!((orders[0].qty - 1.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn order_placement_is_unsupported_and_never_faked() {
        let cfg = ConnectorConfig {
            base_url: "http://127.0.0.1:1".into(),
            ..ConnectorConfig::default()
        };
        let connector = HyperliquidConnector::new(cfg, "0xabc".into(), true).unwrap();
        let req = OrderRequest::market(Venue::Hyperliquid, "SOL/USDT", Side::Buy, 1.0);
        let err = connector
            .place_order(&req)
            .await
            .expect_err("must be unsupported");
        match err {
            ExchangeError::Unsupported(msg) => assert_eq!(msg, UNSUPPORTED_ORDER_FLOW),
            other => panic!("expected an unsupported error, got {other}"),
        }
        let err = connector
            .cancel_order("1")
            .await
            .expect_err("must be unsupported");
        assert!(matches!(err, ExchangeError::Unsupported(_)), "{err}");
    }

    #[tokio::test]
    async fn live_trading_guard_is_checked_before_the_unsupported_error() {
        let cfg = ConnectorConfig {
            base_url: "http://127.0.0.1:1".into(),
            ..ConnectorConfig::default()
        };
        let connector = HyperliquidConnector::new(cfg, "0xabc".into(), false).unwrap();
        let req = OrderRequest::market(Venue::Hyperliquid, "SOL/USDT", Side::Buy, 1.0);
        let err = connector
            .place_order(&req)
            .await
            .expect_err("must be blocked");
        assert!(matches!(err, ExchangeError::Config(_)), "{err}");
    }
}

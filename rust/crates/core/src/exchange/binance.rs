//! Binance spot connector.
//!
//! * REST: `GET /api/v3/account`, `POST /api/v3/order`, `DELETE /api/v3/order`,
//!   `GET /api/v3/openOrders`, `GET /api/v3/ticker/price`, `GET /api/v3/ping`.
//! * Signing: HMAC-SHA256 over the query string, hex encoded, sent in the
//!   `X-MBX-APIKEY` header together with the signed query string.
//! * WebSocket: combined `aggTrade` streams, reconnected with exponential
//!   backoff (1s → 30s).
//!
//! # Live trading guard
//!
//! The connector never talks to the order endpoint unless it was built with
//! `live_enabled = true`. That flag comes from the arming ceremony in
//! `crate::live_arm`, so it is only ever set when the operator armed live
//! trading for this venue and this risk configuration. Read-only calls
//! (account, open orders, ticker) are always allowed. The API secret is never
//! logged.

use crate::exchange::{
    ConnectorConfig, ExchangeConnector, ExchangeError, ExchangeResult, OpenOrder, TickSink,
};
use crate::types::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use hmac::{Hmac, Mac};
use reqwest::Method;
use serde_json::Value;
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

type HmacSha256 = Hmac<Sha256>;

pub const DEFAULT_BASE_URL: &str = "https://api.binance.com";
pub const DEFAULT_WS_URL: &str = "wss://stream.binance.com:9443/stream";
/// Message returned when live trading is not armed.
pub const LIVE_TRADING_DISABLED: &str =
    "live trading is not armed; run `openether-live arm` and set OPENETHER_LIVE_TRADING=1";

/// Canonical symbols the core trades; used to map Binance's compact symbol
/// names back to the `BASE/QUOTE` form used everywhere else.
pub const CANONICAL_SYMBOLS: [&str; 6] = [
    "SOL/USDT", "BTC/USDT", "ETH/USDT", "SOL/USD", "BTC/USD", "ETH/USD",
];

/// `SOL/USDT` → `SOLUSDT`.
pub fn to_binance_symbol(symbol: &str) -> String {
    symbol
        .trim()
        .to_ascii_uppercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

/// `SOLUSDT` → `SOL/USDT` when the pair is known, otherwise the raw symbol.
pub fn from_binance_symbol(raw: &str) -> String {
    let upper = raw.trim().to_ascii_uppercase();
    for canonical in CANONICAL_SYMBOLS {
        if to_binance_symbol(canonical) == upper {
            return canonical.to_string();
        }
    }
    raw.to_string()
}

/// Lowercase stream symbol, e.g. `solusdt`.
pub fn to_stream_symbol(symbol: &str) -> String {
    to_binance_symbol(symbol).to_ascii_lowercase()
}

/// Binance spot connector.
#[derive(Clone)]
pub struct BinanceConnector {
    http: reqwest::Client,
    cfg: ConnectorConfig,
    live_enabled: bool,
    /// Binance needs the symbol to cancel an order, but the trait only hands
    /// over the order id: remember the mapping for every order we placed.
    order_symbols: Arc<Mutex<HashMap<String, String>>>,
}

impl std::fmt::Debug for BinanceConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BinanceConnector")
            .field("base_url", &self.cfg.base_url)
            .field("ws_url", &self.cfg.ws_url)
            .field("has_credentials", &self.cfg.has_credentials())
            .field("live_enabled", &self.live_enabled)
            .finish()
    }
}

impl BinanceConnector {
    /// Build a connector from the environment.
    ///
    /// Reads `BINANCE_API_KEY`, `BINANCE_API_SECRET`, `BINANCE_BASE_URL`
    /// (default `https://api.binance.com`) and `BINANCE_WS_URL` (default
    /// `wss://stream.binance.com:9443/stream`).
    ///
    /// `live_enabled` is the decision of the arming ceremony, not of an
    /// environment variable: the engine passes it in.
    pub fn from_env(live_enabled: bool) -> ExchangeResult<Self> {
        let cfg = ConnectorConfig {
            base_url: env_var("BINANCE_BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            ws_url: env_var("BINANCE_WS_URL").unwrap_or_else(|| DEFAULT_WS_URL.to_string()),
            api_key: env_var("BINANCE_API_KEY"),
            api_secret: env_var("BINANCE_API_SECRET"),
            recv_timeout: Duration::from_millis(
                env_var("BINANCE_RECV_TIMEOUT_MS")
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(10_000),
            ),
            max_inflight: 4,
        };
        Self::new(cfg, live_enabled)
    }

    /// Build a connector with an explicit configuration.
    pub fn new(cfg: ConnectorConfig, live_enabled: bool) -> ExchangeResult<Self> {
        let timeout = cfg.recv_timeout;
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(concat!("openether/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| ExchangeError::Config(format!("could not build the http client: {e}")))?;
        Ok(Self {
            http,
            cfg,
            live_enabled,
            order_symbols: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn config(&self) -> &ConnectorConfig {
        &self.cfg
    }

    pub fn live_enabled(&self) -> bool {
        self.live_enabled
    }

    fn credentials(&self) -> ExchangeResult<(&str, &str)> {
        match (self.cfg.api_key.as_deref(), self.cfg.api_secret.as_deref()) {
            (Some(key), Some(secret)) if !key.is_empty() && !secret.is_empty() => Ok((key, secret)),
            _ => Err(ExchangeError::Config(
                "missing BINANCE_API_KEY/BINANCE_API_SECRET".into(),
            )),
        }
    }

    /// Reject live order flow when the guard is closed. Never touches the
    /// network.
    fn ensure_live(&self) -> ExchangeResult<()> {
        if self.live_enabled {
            Ok(())
        } else {
            Err(ExchangeError::Config(LIVE_TRADING_DISABLED.to_string()))
        }
    }

    /// HMAC-SHA256 of the query string, hex encoded.
    pub fn sign(query: &str, secret: &str) -> ExchangeResult<String> {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
            .map_err(|e| ExchangeError::Internal(format!("invalid signing key: {e}")))?;
        mac.update(query.as_bytes());
        Ok(hex::encode(mac.finalize().into_bytes()))
    }

    fn remember_order(&self, order_id: &str, symbol: &str) {
        let mut map = self
            .order_symbols
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.insert(order_id.to_string(), symbol.to_string());
    }

    fn symbol_of_order(&self, order_id: &str) -> Option<String> {
        let map = self
            .order_symbols
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(sym) = map.get(order_id) {
            return Some(sym.clone());
        }
        // Fall back to the `SYMBOL:ORDER_ID` convention.
        order_id
            .split_once(':')
            .map(|(sym, _)| sym.to_string())
            .filter(|s| !s.is_empty())
    }

    /// Signed request helper. `params` are the already URL-encoded pairs
    /// (without the signature); the timestamp and recv window are appended.
    async fn signed_request(
        &self,
        method: Method,
        path: &str,
        params: &[(&str, String)],
    ) -> ExchangeResult<Value> {
        let (api_key, secret) = self.credentials()?;
        let mut query = String::new();
        for (k, v) in params {
            if !query.is_empty() {
                query.push('&');
            }
            query.push_str(k);
            query.push('=');
            query.push_str(&urlencode(v));
        }
        if !query.is_empty() {
            query.push('&');
        }
        query.push_str("timestamp=");
        query.push_str(&now_ms().to_string());
        query.push_str("&recvWindow=5000");

        let signature = Self::sign(&query, secret)?;
        let signed_query = format!("{query}&signature={signature}");
        let url = format!("{}{}", self.cfg.base_url.trim_end_matches('/'), path);

        let request = self
            .http
            .request(method, &url)
            .header("X-MBX-APIKEY", api_key)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(signed_query);

        let response = tokio::time::timeout(self.cfg.recv_timeout, request.send())
            .await
            .map_err(|_| ExchangeError::Network(format!("binance request timed out: {path}")))?
            .map_err(|e| ExchangeError::Network(format!("binance request failed: {e}")))?;

        let status = response.status();
        let body = tokio::time::timeout(self.cfg.recv_timeout, response.text())
            .await
            .map_err(|_| ExchangeError::Network(format!("binance response timed out: {path}")))?
            .unwrap_or_default();

        if !status.is_success() {
            return Err(map_binance_error(status.as_u16(), &body));
        }

        let json: Value = serde_json::from_str(&body).map_err(|e| {
            ExchangeError::Internal(format!("binance returned invalid json for {path}: {e}"))
        })?;

        // Binance occasionally answers 200 with an error payload.
        if let Some(code) = json.get("code").and_then(Value::as_i64) {
            if code < 0 {
                let msg = json
                    .get("msg")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                return Err(map_binance_code(code, msg));
            }
        }

        Ok(json)
    }

    /// Public (unsigned) request helper.
    async fn public_request(&self, path: &str, params: &[(&str, String)]) -> ExchangeResult<Value> {
        let mut query = String::new();
        for (k, v) in params {
            if !query.is_empty() {
                query.push('&');
            }
            query.push_str(k);
            query.push('=');
            query.push_str(&urlencode(v));
        }
        let mut url = format!("{}{}", self.cfg.base_url.trim_end_matches('/'), path);
        if !query.is_empty() {
            url.push('?');
            url.push_str(&query);
        }
        let response = tokio::time::timeout(self.cfg.recv_timeout, self.http.get(&url).send())
            .await
            .map_err(|_| ExchangeError::Network(format!("binance request timed out: {path}")))?
            .map_err(|e| ExchangeError::Network(format!("binance request failed: {e}")))?;
        let status = response.status();
        let body = tokio::time::timeout(self.cfg.recv_timeout, response.text())
            .await
            .map_err(|_| ExchangeError::Network(format!("binance response timed out: {path}")))?
            .unwrap_or_default();
        if !status.is_success() {
            return Err(map_binance_error(status.as_u16(), &body));
        }
        serde_json::from_str(&body).map_err(|e| {
            ExchangeError::Internal(format!("binance returned invalid json for {path}: {e}"))
        })
    }

    fn stream_url(&self, symbols: &[String]) -> String {
        let streams: Vec<String> = symbols
            .iter()
            .map(|s| format!("{}@aggTrade", to_stream_symbol(s)))
            .collect();
        let base = self.cfg.ws_url.trim_end_matches('/');
        if base.contains('?') {
            format!("{base}&streams={}", streams.join("/"))
        } else {
            format!("{base}?streams={}", streams.join("/"))
        }
    }

    /// Consume the websocket stream until it closes or fails.
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
                    tracing::warn!(venue = "binance", error = %e, "websocket error");
                    return Err(ExchangeError::Network(format!(
                        "binance websocket error: {e}"
                    )));
                }
            };
            let payload = match msg {
                Message::Text(text) => text.as_str().to_string(),
                Message::Binary(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                Message::Close(_) => {
                    tracing::warn!(venue = "binance", "websocket closed by the server");
                    return Ok(());
                }
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
            };
            match parse_agg_trade(&payload) {
                Some(tick) => {
                    let wanted = symbols.is_empty() || symbols.iter().any(|s| s == &tick.symbol);
                    if wanted && sink.send(tick).is_err() {
                        tracing::debug!(venue = "binance", "tick sink closed");
                        return Ok(());
                    }
                }
                None => {
                    tracing::debug!(venue = "binance", "ignoring unparsable websocket payload");
                }
            }
        }
        Ok(())
    }

    /// Reconnecting websocket loop; runs until the sink is closed.
    async fn stream_loop(&self, symbols: Vec<String>, sink: TickSink) {
        let mut backoff = Duration::from_secs(1);
        let max_backoff = Duration::from_secs(30);
        loop {
            let url = self.stream_url(&symbols);
            let connect = tokio_tungstenite::connect_async(&url);
            let connected = tokio::time::timeout(Duration::from_secs(15), connect).await;
            match connected {
                Ok(Ok((stream, _))) => {
                    tracing::info!(venue = "binance", url = %url, "market data stream connected");
                    backoff = Duration::from_secs(1);
                    if let Err(err) = self.consume_stream(stream, &symbols, &sink).await {
                        tracing::warn!(venue = "binance", error = %err, "market data stream ended");
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!(venue = "binance", error = %e, "websocket connection failed");
                }
                Err(_) => {
                    tracing::warn!(venue = "binance", "websocket connection timed out");
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

/// Percent-encode everything outside the unreserved character set.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Map a Binance error payload to the connector error type.
pub fn map_binance_code(code: i64, msg: &str) -> ExchangeError {
    let lower = msg.to_ascii_lowercase();
    if lower.contains("insufficient balance") || lower.contains("insufficient funds") {
        return ExchangeError::InsufficientBalance {
            asset: "USDT".into(),
            needed: 0.0,
            available: 0.0,
        };
    }
    match code {
        -1002 | -2015 => ExchangeError::Auth(msg.to_string()),
        -1003 => ExchangeError::RateLimited(msg.to_string()),
        -1021 | -1100 | -1121 => ExchangeError::InvalidRequest(msg.to_string()),
        -2013 | -2014 => ExchangeError::Rejected(msg.to_string()),
        _ => ExchangeError::Rejected(format!("binance error {code}: {msg}")),
    }
}

/// Map an HTTP status plus body to the connector error type.
pub fn map_binance_error(status: u16, body: &str) -> ExchangeError {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let code = parsed
        .as_ref()
        .and_then(|v| v.get("code"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let msg = parsed
        .as_ref()
        .and_then(|v| v.get("msg"))
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .unwrap_or_else(|| body.trim().to_string());

    match status {
        401 | 403 => ExchangeError::Auth(format!("binance auth error {status}: {msg}")),
        429 => ExchangeError::RateLimited(format!("binance rate limited: {msg}")),
        400 | 404 | 409 => {
            if code != 0 {
                map_binance_code(code, &msg)
            } else {
                ExchangeError::Rejected(format!("binance rejected the request {status}: {msg}"))
            }
        }
        500..=599 => ExchangeError::Network(format!("binance server error {status}: {msg}")),
        _ => ExchangeError::Network(format!("binance returned http {status}: {msg}")),
    }
}

/// Map a Binance order status to the core's status enum.
pub fn map_order_status(status: &str) -> OrderStatus {
    match status.to_ascii_uppercase().as_str() {
        "FILLED" => OrderStatus::Filled,
        "PARTIALLY_FILLED" => OrderStatus::PartiallyFilled,
        "CANCELED" | "CANCELLED" | "EXPIRED" => OrderStatus::Cancelled,
        "REJECTED" => OrderStatus::Rejected,
        _ => OrderStatus::Accepted,
    }
}

/// Parse a combined-stream or raw `aggTrade` payload into a tick.
pub fn parse_agg_trade(payload: &str) -> Option<MarketTick> {
    let root: Value = serde_json::from_str(payload).ok()?;
    let data = match root.get("data") {
        Some(data) => data.clone(),
        None => root,
    };
    let event = data.get("e").and_then(Value::as_str).unwrap_or("aggTrade");
    if event != "aggTrade" && event != "trade" {
        return None;
    }
    let price = data.get("p").and_then(Value::as_str)?.parse::<f64>().ok()?;
    let size = data
        .get("q")
        .and_then(Value::as_str)
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.0);
    let ts_ms = data.get("T").and_then(Value::as_i64).unwrap_or_else(now_ms);
    let raw_symbol = data.get("s").and_then(Value::as_str)?;
    Some(MarketTick {
        venue: Venue::Binance,
        symbol: from_binance_symbol(raw_symbol),
        price,
        size,
        ts_ms,
    })
}

#[async_trait]
impl ExchangeConnector for BinanceConnector {
    fn venue(&self) -> Venue {
        Venue::Binance
    }

    async fn health(&self) -> ExchangeResult<()> {
        self.public_request("/api/v3/ping", &[]).await.map(|_| ())
    }

    async fn account(&self) -> ExchangeResult<AccountSnapshot> {
        let json = self
            .signed_request(Method::GET, "/api/v3/account", &[])
            .await?;
        let mut balances = Vec::new();
        let mut equity = 0.0;
        let mut free = 0.0;
        if let Some(list) = json.get("balances").and_then(Value::as_array) {
            for entry in list {
                let asset = entry
                    .get("asset")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let free_f = entry
                    .get("free")
                    .and_then(Value::as_str)
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(0.0);
                let locked_f = entry
                    .get("locked")
                    .and_then(Value::as_str)
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(0.0);
                if !asset.is_empty() {
                    balances.push(Balance {
                        asset: asset.clone(),
                        free: free_f,
                        locked: locked_f,
                    });
                }
                // Quote currencies make up the USD equity of a spot account.
                if is_quote_asset(&asset) {
                    equity += free_f + locked_f;
                    free += free_f;
                }
            }
        }
        Ok(AccountSnapshot {
            venue: Venue::Binance,
            equity_usd: equity,
            free_usd: free,
            balances,
            ts_ms: now_ms(),
        })
    }

    async fn place_order(&self, req: &OrderRequest) -> ExchangeResult<OrderAck> {
        // The guard is checked before anything touches the network.
        self.ensure_live()?;
        if req.venue != Venue::Binance {
            tracing::warn!(
                venue = "binance",
                requested_venue = %req.venue,
                "order routed to binance carries a different venue tag"
            );
        }
        if !req.qty.is_finite() || req.qty <= 0.0 {
            return Err(ExchangeError::InvalidRequest(format!(
                "order quantity must be a positive finite number (got {})",
                req.qty
            )));
        }

        let symbol = to_binance_symbol(&req.symbol);
        let mut params: Vec<(&str, String)> = vec![
            ("symbol", symbol.clone()),
            (
                "side",
                if req.side == Side::Buy { "BUY" } else { "SELL" }.to_string(),
            ),
            (
                "type",
                match req.order_type {
                    OrderType::Market => "MARKET",
                    OrderType::Limit => "LIMIT",
                }
                .to_string(),
            ),
            ("quantity", format_qty(req.qty)),
        ];

        match req.order_type {
            OrderType::Market => {}
            OrderType::Limit => {
                let Some(price) = req.limit_price else {
                    return Err(ExchangeError::InvalidRequest(
                        "limit orders require a limit price".into(),
                    ));
                };
                if !price.is_finite() || price <= 0.0 {
                    return Err(ExchangeError::InvalidRequest(format!(
                        "limit price must be a positive finite number (got {price})"
                    )));
                }
                params.push(("timeInForce", "GTC".to_string()));
                params.push(("price", format_qty(price)));
            }
        }

        if !req.client_id.is_empty() {
            params.push(("newClientOrderId", req.client_id.clone()));
        }
        if req.reduce_only {
            // The spot API has no reduceOnly parameter; a spot sell can never
            // exceed the balance, so the exchange enforces it for us.
            tracing::warn!(
                venue = "binance",
                symbol = %req.symbol,
                "reduce_only is not supported by the binance spot api and was dropped"
            );
        }

        let json = self
            .signed_request(Method::POST, "/api/v3/order", &params)
            .await?;

        let order_id = json
            .get("orderId")
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default();
        let client_id = json
            .get("clientOrderId")
            .and_then(Value::as_str)
            .unwrap_or(&req.client_id)
            .to_string();
        let status = json
            .get("status")
            .and_then(Value::as_str)
            .map(map_order_status)
            .unwrap_or(OrderStatus::Accepted);
        let accepted_ts_ms = json
            .get("transactTime")
            .and_then(Value::as_i64)
            .unwrap_or_else(now_ms);

        if !order_id.is_empty() {
            self.remember_order(&order_id, &req.symbol);
        }

        Ok(OrderAck {
            venue: Venue::Binance,
            order_id,
            client_id,
            status,
            accepted_ts_ms,
            message: format!("binance accepted {} {} {}", req.symbol, req.side, req.qty),
        })
    }

    async fn cancel_order(&self, order_id: &str) -> ExchangeResult<()> {
        self.ensure_live()?;
        let symbol = self.symbol_of_order(order_id).ok_or_else(|| {
            ExchangeError::InvalidRequest(format!(
                "unknown binance order id {order_id}: the symbol is required to cancel"
            ))
        })?;
        let params = vec![
            ("symbol", to_binance_symbol(&symbol)),
            ("orderId", order_id.to_string()),
        ];
        self.signed_request(Method::DELETE, "/api/v3/order", &params)
            .await
            .map(|_| ())
    }

    async fn open_orders(&self) -> ExchangeResult<Vec<OpenOrder>> {
        let json = self
            .signed_request(Method::GET, "/api/v3/openOrders", &[])
            .await?;
        let mut out = Vec::new();
        if let Some(list) = json.as_array() {
            for entry in list {
                let raw_symbol = entry
                    .get("symbol")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let order_id = entry
                    .get("orderId")
                    .map(|v| match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .unwrap_or_default();
                let side = entry
                    .get("side")
                    .and_then(Value::as_str)
                    .and_then(Side::parse)
                    .unwrap_or(Side::Buy);
                let qty = entry
                    .get("origQty")
                    .and_then(Value::as_str)
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(0.0);
                let price = entry
                    .get("price")
                    .and_then(Value::as_str)
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(0.0);
                let ts_ms = entry
                    .get("time")
                    .and_then(Value::as_i64)
                    .unwrap_or_else(now_ms);
                self.remember_order(&order_id, &from_binance_symbol(raw_symbol));
                out.push(OpenOrder {
                    venue: Venue::Binance,
                    order_id,
                    symbol: from_binance_symbol(raw_symbol),
                    side,
                    qty,
                    price,
                    ts_ms,
                });
            }
        }
        Ok(out)
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
        let json = self
            .public_request(
                "/api/v3/ticker/price",
                &[("symbol", to_binance_symbol(symbol))],
            )
            .await?;
        let price = json
            .get("price")
            .and_then(Value::as_str)
            .and_then(|v| v.parse::<f64>().ok())
            .ok_or_else(|| ExchangeError::Internal("binance ticker without a price".into()))?;
        if !price.is_finite() || price <= 0.0 {
            return Err(ExchangeError::Internal(format!(
                "binance returned an invalid price for {symbol}"
            )));
        }
        Ok(price)
    }
}

fn is_quote_asset(asset: &str) -> bool {
    matches!(asset, "USDT" | "USD" | "USDC" | "BUSD" | "FDUSD" | "TUSD")
}

/// Format a quantity without losing precision but without a trailing `.0`.
fn format_qty(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        let s = format!("{value:.8}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_mapping() {
        assert_eq!(to_binance_symbol("SOL/USDT"), "SOLUSDT");
        assert_eq!(to_binance_symbol("sol/usdt"), "SOLUSDT");
        assert_eq!(from_binance_symbol("SOLUSDT"), "SOL/USDT");
        assert_eq!(from_binance_symbol("XYZUSDT"), "XYZUSDT");
        assert_eq!(to_stream_symbol("SOL/USDT"), "solusdt");
    }

    #[test]
    fn signature_is_stable_and_hex() {
        let query = "symbol=SOLUSDT&side=BUY&type=MARKET&quantity=1.0&timestamp=1731200000000&recvWindow=5000";
        let sig = BinanceConnector::sign(query, "secret").unwrap();
        assert_eq!(sig.len(), 64);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
        // Deterministic.
        assert_eq!(sig, BinanceConnector::sign(query, "secret").unwrap());
        // A different payload yields a different signature.
        assert_ne!(
            sig,
            BinanceConnector::sign("symbol=BTCUSDT", "secret").unwrap()
        );
    }

    #[test]
    fn error_mapping() {
        match map_binance_error(401, "{\"code\":-2015,\"msg\":\"bad key\"}") {
            ExchangeError::Auth(_) => {}
            other => panic!("expected an auth error, got {other}"),
        }
        match map_binance_error(429, "{\"code\":-1003,\"msg\":\"too many requests\"}") {
            ExchangeError::RateLimited(_) => {}
            other => panic!("expected a rate limit error, got {other}"),
        }
        match map_binance_error(400, "{\"code\":-2010,\"msg\":\"New order rejected\"}") {
            ExchangeError::Rejected(_) => {}
            other => panic!("expected a rejection, got {other}"),
        }
        match map_binance_error(503, "gateway down") {
            ExchangeError::Network(_) => {}
            other => panic!("expected a network error, got {other}"),
        }
        match map_binance_code(-1121, "Invalid symbol.") {
            ExchangeError::InvalidRequest(_) => {}
            other => panic!("expected an invalid request, got {other}"),
        }
    }

    #[test]
    fn agg_trade_parsing() {
        let payload = r#"{"stream":"solusdt@aggTrade","data":{"e":"aggTrade","E":1731200000000,"s":"SOLUSDT","p":"142.31","q":"0.5","T":1731200000000}}"#;
        let tick = parse_agg_trade(payload).expect("tick");
        assert_eq!(tick.venue, Venue::Binance);
        assert_eq!(tick.symbol, "SOL/USDT");
        assert!((tick.price - 142.31).abs() < 1e-9);
        assert!((tick.size - 0.5).abs() < 1e-9);
        assert_eq!(tick.ts_ms, 1731200000000);
    }

    #[test]
    fn malformed_payloads_are_skipped() {
        assert!(parse_agg_trade("not json").is_none());
        assert!(parse_agg_trade("{\"e\":\"depth\"}").is_none());
        assert!(parse_agg_trade("{\"e\":\"aggTrade\",\"s\":\"SOLUSDT\"}").is_none());
    }

    #[tokio::test]
    async fn live_trading_guard_blocks_orders_without_touching_the_network() {
        let cfg = ConnectorConfig {
            base_url: "http://127.0.0.1:1".into(), // nothing listens here
            api_key: Some("key".into()),
            api_secret: Some("secret".into()),
            recv_timeout: Duration::from_millis(200),
            ..ConnectorConfig::default()
        };
        let connector = BinanceConnector::new(cfg, false).unwrap();

        let req = OrderRequest::market(Venue::Binance, "SOL/USDT", Side::Buy, 1.0);
        let err = connector
            .place_order(&req)
            .await
            .expect_err("must be blocked");
        match err {
            ExchangeError::Config(msg) => assert_eq!(msg, LIVE_TRADING_DISABLED),
            other => panic!("expected a config error, got {other}"),
        }
        // Cancelling is a live operation as well.
        let err = connector
            .cancel_order("123")
            .await
            .expect_err("must be blocked");
        assert!(matches!(err, ExchangeError::Config(_)), "{err}");
    }

    #[test]
    fn order_status_mapping() {
        assert_eq!(map_order_status("FILLED"), OrderStatus::Filled);
        assert_eq!(map_order_status("NEW"), OrderStatus::Accepted);
        assert_eq!(
            map_order_status("PARTIALLY_FILLED"),
            OrderStatus::PartiallyFilled
        );
        assert_eq!(map_order_status("CANCELED"), OrderStatus::Cancelled);
        assert_eq!(map_order_status("REJECTED"), OrderStatus::Rejected);
    }
}

//! Kraken spot connector.
//!
//! * REST: `POST /api/0/Private/AddOrder`, `/CancelOrder`, `/OpenOrders`,
//!   `/Balance` and the public `/api/0/public/Ticker`.
//! * Auth: `API-Sign = base64(HMAC-SHA512(path + SHA256(nonce + postdata),
//!   base64_decoded_secret))` with the `API-Key` / `API-Sign` headers and a
//!   `nonce=<n>` body.
//! * WebSocket: v2 `trade` channel, reconnected with exponential backoff
//!   (1s → 30s).
//!
//! # Symbol conventions
//!
//! Kraken uses compact asset pairs (`SOLUSD`, `XBTUSD`). [`pair_for`] maps the
//! canonical `BASE/QUOTE` symbols of the core onto Kraken pairs (USDT is
//! normalised to USD, BTC to XBT) and [`from_kraken_pair`] maps them back so
//! that every venue reports the same canonical symbols.
//!
//! # Live trading guard
//!
//! Order flow is refused unless the connector was built with
//! `live_enabled = true` (see `OPENETHER_LIVE_TRADING`). The API secret is
//! never logged.

use crate::exchange::{
    ConnectorConfig, ExchangeConnector, ExchangeError, ExchangeResult, OpenOrder, TickSink,
};
use crate::types::*;
use async_trait::async_trait;
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use reqwest::Method;
use serde_json::{json, Value};
use sha2::{Digest, Sha256, Sha512};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

type HmacSha512 = Hmac<Sha512>;

pub const DEFAULT_BASE_URL: &str = "https://api.kraken.com";
pub const DEFAULT_WS_URL: &str = "wss://ws.kraken.com/v2";
/// Message returned when live trading is switched off.
pub const LIVE_TRADING_DISABLED: &str =
    "live trading is disabled; set OPENETHER_LIVE_TRADING=1 to enable";

/// Canonical symbols the core trades (Kraken form → canonical form).
const KRAKEN_PAIRS: [(&str, &str); 6] = [
    ("SOLUSD", "SOL/USDT"),
    ("XBTUSD", "BTC/USDT"),
    ("ETHUSD", "ETH/USDT"),
    ("SOLUSD", "SOL/USD"),
    ("XBTUSD", "BTC/USD"),
    ("ETHUSD", "ETH/USD"),
];

/// `SOL/USDT` → `SOLUSD`, `BTC/USD` → `XBTUSD`.
pub fn pair_for(symbol: &str) -> String {
    let upper = symbol.trim().to_ascii_uppercase();
    let (base, quote) = match upper.split_once('/') {
        Some((b, q)) => (b.to_string(), q.to_string()),
        None => (upper.clone(), "USD".to_string()),
    };
    let base = match base.as_str() {
        "BTC" => "XBT".to_string(),
        other => other.to_string(),
    };
    let quote = match quote.as_str() {
        "USDT" | "USDC" => "USD".to_string(),
        other => other.to_string(),
    };
    format!("{base}{quote}")
}

/// `SOL/USDT` → `SOL/USD` (the symbol format used by the v2 websocket API).
pub fn ws_symbol(symbol: &str) -> String {
    let upper = symbol.trim().to_ascii_uppercase();
    let (base, _) = match upper.split_once('/') {
        Some((b, q)) => (b.to_string(), q.to_string()),
        None => (upper.clone(), "USD".to_string()),
    };
    let base = match base.as_str() {
        "BTC" => "XBT".to_string(),
        other => other.to_string(),
    };
    format!("{base}/USD")
}

/// `SOLUSD` → `SOL/USDT` (canonical form used by the rest of the core).
pub fn from_kraken_pair(pair: &str) -> String {
    let upper = pair.trim().to_ascii_uppercase();
    for (kraken, canonical) in KRAKEN_PAIRS {
        if kraken == upper {
            return canonical.to_string();
        }
    }
    // Unknown pair: rebuild a `BASE/QUOTE` symbol.
    if let Some(pos) = upper.find("USD") {
        let base = &upper[..pos];
        let quote = &upper[pos..];
        let base = if base == "XBT" { "BTC" } else { base };
        let quote = if quote == "USD" { "USD" } else { quote };
        return format!("{base}/{quote}");
    }
    pair.to_string()
}

/// `true` when the process was started with live trading enabled.
pub fn live_trading_enabled() -> bool {
    matches!(
        std::env::var("OPENETHER_LIVE_TRADING")
            .ok()
            .map(|v| v.trim().to_ascii_lowercase())
            .as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// Kraken spot connector.
#[derive(Clone)]
pub struct KrakenConnector {
    http: reqwest::Client,
    cfg: ConnectorConfig,
    live_enabled: bool,
    nonce: Arc<AtomicU64>,
    /// Kraken cancels by transaction id, the symbol is only needed for
    /// reporting; remember it for every order we placed.
    order_symbols: Arc<Mutex<HashMap<String, String>>>,
}

impl std::fmt::Debug for KrakenConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KrakenConnector")
            .field("base_url", &self.cfg.base_url)
            .field("ws_url", &self.cfg.ws_url)
            .field("has_credentials", &self.cfg.has_credentials())
            .field("live_enabled", &self.live_enabled)
            .finish()
    }
}

impl KrakenConnector {
    /// Build a connector from the environment.
    ///
    /// Reads `KRAKEN_API_KEY`, `KRAKEN_API_SECRET`, `KRAKEN_BASE_URL`
    /// (default `https://api.kraken.com`), `KRAKEN_WS_URL` (default
    /// `wss://ws.kraken.com/v2`) and `OPENETHER_LIVE_TRADING`.
    pub fn from_env() -> ExchangeResult<Self> {
        let cfg = ConnectorConfig {
            base_url: env_var("KRAKEN_BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            ws_url: env_var("KRAKEN_WS_URL").unwrap_or_else(|| DEFAULT_WS_URL.to_string()),
            api_key: env_var("KRAKEN_API_KEY"),
            api_secret: env_var("KRAKEN_API_SECRET"),
            recv_timeout: Duration::from_millis(
                env_var("KRAKEN_RECV_TIMEOUT_MS")
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(10_000),
            ),
            max_inflight: 4,
        };
        Self::new(cfg, live_trading_enabled())
    }

    /// Build a connector with an explicit configuration.
    pub fn new(cfg: ConnectorConfig, live_enabled: bool) -> ExchangeResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(cfg.recv_timeout)
            .user_agent(concat!("openether/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| ExchangeError::Config(format!("could not build the http client: {e}")))?;
        let nonce = Arc::new(AtomicU64::new(now_ms().max(1) as u64));
        Ok(Self {
            http,
            cfg,
            live_enabled,
            nonce,
            order_symbols: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn config(&self) -> &ConnectorConfig {
        &self.cfg
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

    fn credentials(&self) -> ExchangeResult<(&str, &str)> {
        match (self.cfg.api_key.as_deref(), self.cfg.api_secret.as_deref()) {
            (Some(key), Some(secret)) if !key.is_empty() && !secret.is_empty() => Ok((key, secret)),
            _ => Err(ExchangeError::Config(
                "missing KRAKEN_API_KEY/KRAKEN_API_SECRET".into(),
            )),
        }
    }

    /// Next strictly increasing nonce.
    pub fn next_nonce(&self) -> u64 {
        self.nonce.fetch_add(1, Ordering::SeqCst)
    }

    /// Compute the `API-Sign` header value for a private request.
    pub fn api_sign(path: &str, post_data: &str, secret_b64: &str) -> ExchangeResult<String> {
        let secret = base64::engine::general_purpose::STANDARD
            .decode(secret_b64.as_bytes())
            .map_err(|e| {
                ExchangeError::Config(format!("KRAKEN_API_SECRET is not valid base64: {e}"))
            })?;
        let hash = Sha256::digest(post_data.as_bytes());
        let mut mac = HmacSha512::new_from_slice(&secret)
            .map_err(|e| ExchangeError::Internal(format!("invalid signing key: {e}")))?;
        mac.update(path.as_bytes());
        mac.update(&hash);
        Ok(base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes()))
    }

    /// Signed private POST request. `params` must not contain the nonce.
    async fn private_post(&self, path: &str, params: &[(&str, String)]) -> ExchangeResult<Value> {
        let (api_key, secret) = self.credentials()?;
        let nonce = self.next_nonce();
        let mut body = format!("nonce={nonce}");
        for (k, v) in params {
            body.push('&');
            body.push_str(k);
            body.push('=');
            body.push_str(&urlencode(v));
        }

        let signature = Self::api_sign(path, &body, secret)?;
        let url = format!("{}{}", self.cfg.base_url.trim_end_matches('/'), path);

        let request = self
            .http
            .request(Method::POST, &url)
            .header("API-Key", api_key)
            .header("API-Sign", signature)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body);

        let response = tokio::time::timeout(self.cfg.recv_timeout, request.send())
            .await
            .map_err(|_| ExchangeError::Network(format!("kraken request timed out: {path}")))?
            .map_err(|e| ExchangeError::Network(format!("kraken request failed: {e}")))?;

        let status = response.status();
        let text = tokio::time::timeout(self.cfg.recv_timeout, response.text())
            .await
            .map_err(|_| ExchangeError::Network(format!("kraken response timed out: {path}")))?
            .unwrap_or_default();

        if !status.is_success() {
            return Err(map_kraken_http_error(status.as_u16(), &text));
        }

        let json: Value = serde_json::from_str(&text).map_err(|e| {
            ExchangeError::Internal(format!("kraken returned invalid json for {path}: {e}"))
        })?;

        let errors: Vec<String> = json
            .get("error")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        if !errors.is_empty() {
            return Err(map_kraken_error(&errors));
        }

        Ok(json.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Public (unsigned) GET request.
    async fn public_get(&self, path: &str, params: &[(&str, String)]) -> ExchangeResult<Value> {
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
            .map_err(|_| ExchangeError::Network(format!("kraken request timed out: {path}")))?
            .map_err(|e| ExchangeError::Network(format!("kraken request failed: {e}")))?;
        let status = response.status();
        let text = tokio::time::timeout(self.cfg.recv_timeout, response.text())
            .await
            .map_err(|_| ExchangeError::Network(format!("kraken response timed out: {path}")))?
            .unwrap_or_default();
        if !status.is_success() {
            return Err(map_kraken_http_error(status.as_u16(), &text));
        }
        let json: Value = serde_json::from_str(&text).map_err(|e| {
            ExchangeError::Internal(format!("kraken returned invalid json for {path}: {e}"))
        })?;
        let errors: Vec<String> = json
            .get("error")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        if !errors.is_empty() {
            return Err(map_kraken_error(&errors));
        }
        Ok(json.get("result").cloned().unwrap_or(Value::Null))
    }

    fn remember_order(&self, txid: &str, symbol: &str) {
        let mut map = self
            .order_symbols
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.insert(txid.to_string(), symbol.to_string());
    }

    /// Last traded price of a Kraken pair.
    async fn ticker_price(&self, pair: &str) -> ExchangeResult<f64> {
        let result = self
            .public_get("/api/0/public/Ticker", &[("pair", pair.to_string())])
            .await?;
        let entry = result
            .get(pair)
            .or_else(|| result.as_object().and_then(|m| m.values().next()))
            .ok_or_else(|| {
                ExchangeError::Internal(format!("kraken ticker has no data for {pair}"))
            })?;
        let last = entry
            .get("c")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(Value::as_str)
            .and_then(|v| v.parse::<f64>().ok())
            .ok_or_else(|| {
                ExchangeError::Internal(format!("kraken ticker has no close price for {pair}"))
            })?;
        Ok(last)
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
                    tracing::warn!(venue = "kraken", error = %e, "websocket error");
                    return Err(ExchangeError::Network(format!(
                        "kraken websocket error: {e}"
                    )));
                }
            };
            let payload = match msg {
                Message::Text(text) => text.as_str().to_string(),
                Message::Binary(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                Message::Close(_) => {
                    tracing::warn!(venue = "kraken", "websocket closed by the server");
                    return Ok(());
                }
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
            };
            match parse_trade_message(&payload) {
                Some(tick) => {
                    let wanted = symbols.is_empty() || symbols.iter().any(|s| s == &tick.symbol);
                    if wanted && sink.send(tick).is_err() {
                        tracing::debug!(venue = "kraken", "tick sink closed");
                        return Ok(());
                    }
                }
                None => {
                    tracing::debug!(venue = "kraken", "ignoring unparsable websocket payload");
                }
            }
        }
        Ok(())
    }

    /// Reconnecting websocket loop; runs until the sink is closed.
    async fn stream_loop(&self, symbols: Vec<String>, sink: TickSink) {
        let mut backoff = Duration::from_secs(1);
        let max_backoff = Duration::from_secs(30);
        let ws_symbols: Vec<String> = symbols.iter().map(|s| ws_symbol(s)).collect();
        loop {
            let url = self.cfg.ws_url.trim_end_matches('/').to_string();
            let connect = tokio_tungstenite::connect_async(&url);
            match tokio::time::timeout(Duration::from_secs(15), connect).await {
                Ok(Ok((mut stream, _))) => {
                    tracing::info!(venue = "kraken", url = %url, "market data stream connected");
                    // Subscribe to the trade channel.
                    let subscribe = json!({
                        "method": "subscribe",
                        "params": { "channel": "trade", "symbol": ws_symbols }
                    });
                    if let Err(e) = stream
                        .send(Message::Text(subscribe.to_string().into()))
                        .await
                    {
                        tracing::warn!(venue = "kraken", error = %e, "could not subscribe to the trade channel");
                    }
                    backoff = Duration::from_secs(1);
                    if let Err(err) = self.consume_stream(stream, &symbols, &sink).await {
                        tracing::warn!(venue = "kraken", error = %err, "market data stream ended");
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!(venue = "kraken", error = %e, "websocket connection failed");
                }
                Err(_) => {
                    tracing::warn!(venue = "kraken", "websocket connection timed out");
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
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Map the `error` array of a Kraken response to the connector error type.
pub fn map_kraken_error(errors: &[String]) -> ExchangeError {
    let joined = errors.join("; ");
    let upper = joined.to_ascii_uppercase();
    if upper.contains("INSUFFICIENT FUNDS") || upper.contains("INSUFFICIENT BALANCE") {
        return ExchangeError::InsufficientBalance {
            asset: "USD".into(),
            needed: 0.0,
            available: 0.0,
        };
    }
    if upper.contains("INVALID KEY")
        || upper.contains("INVALID SIGNATURE")
        || upper.contains("PERMISSION DENIED")
        || upper.contains("ESESSION")
        || upper.contains("INVALID NONCE")
    {
        return ExchangeError::Auth(joined);
    }
    if upper.contains("RATE LIMIT") || upper.contains("TOO MANY REQUESTS") {
        return ExchangeError::RateLimited(joined);
    }
    if upper.contains("UNKNOWN METHOD") || upper.contains("UNKNOWN ASSET") {
        return ExchangeError::InvalidRequest(joined);
    }
    if upper.contains("ORDER")
        || upper.contains("PRICE")
        || upper.contains("VOLUME")
        || upper.contains("MARGIN")
        || upper.contains("POSITION")
    {
        return ExchangeError::Rejected(joined);
    }
    ExchangeError::Rejected(joined)
}

/// Map an HTTP error of a Kraken request to the connector error type.
pub fn map_kraken_http_error(status: u16, body: &str) -> ExchangeError {
    match status {
        401 | 403 => ExchangeError::Auth(format!("kraken auth error {status}: {body}")),
        429 => ExchangeError::RateLimited(format!("kraken rate limited: {body}")),
        500..=599 => ExchangeError::Network(format!("kraken server error {status}: {body}")),
        _ => ExchangeError::Rejected(format!("kraken rejected the request {status}: {body}")),
    }
}

/// Numbers arrive either as JSON numbers or as strings.
fn value_as_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

/// Parse a v2 websocket trade message into a tick.
pub fn parse_trade_message(payload: &str) -> Option<MarketTick> {
    let root: Value = serde_json::from_str(payload).ok()?;
    if root.get("channel").and_then(Value::as_str) != Some("trade") {
        return None;
    }
    if let Some(kind) = root.get("type").and_then(Value::as_str) {
        if kind != "update" {
            return None;
        }
    }
    let data = root.get("data")?.as_array()?;
    let entry = data.first()?;
    let price = entry.get("price").and_then(value_as_f64)?;
    let size = entry.get("qty").and_then(value_as_f64).unwrap_or(0.0);
    let raw_symbol = entry.get("symbol").and_then(Value::as_str)?;
    // The payload carries a side; ticks are side agnostic so it is only logged.
    if let Some(side) = entry.get("side").and_then(Value::as_str) {
        tracing::trace!(venue = "kraken", symbol = %raw_symbol, side = %side, "trade");
    }
    Some(MarketTick {
        venue: Venue::Kraken,
        symbol: from_kraken_ws_symbol(raw_symbol),
        price,
        size,
        ts_ms: now_ms(),
    })
}

/// `SOL/USD` → `SOL/USDT` (canonical form).
pub fn from_kraken_ws_symbol(symbol: &str) -> String {
    let upper = symbol.trim().to_ascii_uppercase();
    let base = upper.split('/').next().unwrap_or(&upper).to_string();
    let base = if base == "XBT" { "BTC" } else { base.as_str() };
    match base {
        "SOL" => "SOL/USDT".to_string(),
        "BTC" => "BTC/USDT".to_string(),
        "ETH" => "ETH/USDT".to_string(),
        other => format!("{other}/USDT"),
    }
}

#[async_trait]
impl ExchangeConnector for KrakenConnector {
    fn venue(&self) -> Venue {
        Venue::Kraken
    }

    async fn health(&self) -> ExchangeResult<()> {
        self.public_get("/api/0/public/Time", &[]).await.map(|_| ())
    }

    async fn account(&self) -> ExchangeResult<AccountSnapshot> {
        let result = self.private_post("/api/0/Private/Balance", &[]).await?;
        let mut balances = Vec::new();
        let mut equity = 0.0;
        let mut free = 0.0;
        if let Some(map) = result.as_object() {
            for (asset, value) in map {
                let amount = value
                    .as_str()
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(0.0);
                balances.push(Balance {
                    asset: asset.clone(),
                    free: amount,
                    locked: 0.0,
                });
                if is_fiat_asset(asset) {
                    equity += amount;
                    free += amount;
                }
            }
        }
        Ok(AccountSnapshot {
            venue: Venue::Kraken,
            equity_usd: equity,
            free_usd: free,
            balances,
            ts_ms: now_ms(),
        })
    }

    async fn place_order(&self, req: &OrderRequest) -> ExchangeResult<OrderAck> {
        // The guard is checked before anything touches the network.
        self.ensure_live()?;
        if req.venue != Venue::Kraken {
            tracing::warn!(
                venue = "kraken",
                requested_venue = %req.venue,
                "order routed to kraken carries a different venue tag"
            );
        }
        if !req.qty.is_finite() || req.qty <= 0.0 {
            return Err(ExchangeError::InvalidRequest(format!(
                "order quantity must be a positive finite number (got {})",
                req.qty
            )));
        }

        let pair = pair_for(&req.symbol);
        let order_type = match req.order_type {
            OrderType::Market => "market",
            OrderType::Limit => "limit",
        };
        let mut params: Vec<(&str, String)> = vec![
            ("pair", pair.clone()),
            (
                "type",
                if req.side == Side::Buy { "buy" } else { "sell" }.to_string(),
            ),
            ("ordertype", order_type.to_string()),
            ("volume", format_qty(req.qty)),
        ];

        match req.order_type {
            OrderType::Market => {
                // Kraken needs a price for market orders: it is the worst
                // acceptable price, so use the last trade with a small buffer.
                let last = self.ticker_price(&pair).await?;
                let bound = if req.side == Side::Buy {
                    last * 1.01
                } else {
                    last * 0.99
                };
                params.push(("price", format_qty(bound)));
            }
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
                params.push(("price", format_qty(price)));
            }
        }

        if req.reduce_only {
            params.push(("reduce_only", "true".to_string()));
        }

        let result = self
            .private_post("/api/0/Private/AddOrder", &params)
            .await?;
        let txid = result
            .get("txid")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .ok_or_else(|| ExchangeError::Internal("kraken did not return a txid".into()))?;

        self.remember_order(&txid, &req.symbol);

        Ok(OrderAck {
            venue: Venue::Kraken,
            order_id: txid,
            client_id: req.client_id.clone(),
            status: OrderStatus::Accepted,
            accepted_ts_ms: now_ms(),
            message: format!("kraken accepted {} {} {}", req.symbol, req.side, req.qty),
        })
    }

    async fn cancel_order(&self, order_id: &str) -> ExchangeResult<()> {
        self.ensure_live()?;
        let result = self
            .private_post(
                "/api/0/Private/CancelOrder",
                &[("txid", order_id.to_string())],
            )
            .await?;
        let count = result
            .get("count")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        if count == 0 {
            return Err(ExchangeError::Rejected(format!(
                "kraken did not cancel the order {order_id}"
            )));
        }
        Ok(())
    }

    async fn open_orders(&self) -> ExchangeResult<Vec<OpenOrder>> {
        let result = self.private_post("/api/0/Private/OpenOrders", &[]).await?;
        let mut out = Vec::new();
        if let Some(open) = result.get("open").and_then(Value::as_object) {
            for (txid, entry) in open {
                let pair = entry
                    .get("descr")
                    .and_then(|d| d.get("pair"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let side = entry
                    .get("descr")
                    .and_then(|d| d.get("type"))
                    .and_then(Value::as_str)
                    .and_then(Side::parse)
                    .unwrap_or(Side::Buy);
                let qty = entry
                    .get("vol")
                    .and_then(Value::as_str)
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(0.0);
                let price = entry
                    .get("price")
                    .and_then(Value::as_str)
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(0.0);
                let ts_ms = entry
                    .get("opentm")
                    .and_then(Value::as_f64)
                    .map(|t| (t * 1000.0) as i64)
                    .unwrap_or_else(now_ms);
                self.remember_order(txid, &from_kraken_pair(pair));
                out.push(OpenOrder {
                    venue: Venue::Kraken,
                    order_id: txid.clone(),
                    symbol: from_kraken_pair(pair),
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
        let pair = pair_for(symbol);
        let price = self.ticker_price(&pair).await?;
        if !price.is_finite() || price <= 0.0 {
            return Err(ExchangeError::Internal(format!(
                "kraken returned an invalid price for {symbol}"
            )));
        }
        Ok(price)
    }
}

fn is_fiat_asset(asset: &str) -> bool {
    matches!(
        asset,
        "USD" | "USDC" | "ZUSD" | "USDT" | "EUR" | "GBP" | "JPY" | "CAD"
    )
}

/// Format a quantity without a trailing `.0`.
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
    fn pair_mapping() {
        assert_eq!(pair_for("SOL/USD"), "SOLUSD");
        assert_eq!(pair_for("SOL/USDT"), "SOLUSD");
        assert_eq!(pair_for("BTC/USD"), "XBTUSD");
        assert_eq!(pair_for("BTC/USDT"), "XBTUSD");
        assert_eq!(pair_for("ETH/USD"), "ETHUSD");
        assert_eq!(ws_symbol("SOL/USDT"), "SOL/USD");
        assert_eq!(from_kraken_pair("XBTUSD"), "BTC/USDT");
        assert_eq!(from_kraken_pair("SOLUSD"), "SOL/USDT");
    }

    #[test]
    fn api_sign_is_deterministic() {
        let secret = base64::engine::general_purpose::STANDARD.encode(b"0123456789abcdef");
        let a = KrakenConnector::api_sign("/api/0/Private/Balance", "nonce=1", &secret).unwrap();
        let b = KrakenConnector::api_sign("/api/0/Private/Balance", "nonce=1", &secret).unwrap();
        assert_eq!(a, b);
        assert!(!a.is_empty());
        let c = KrakenConnector::api_sign("/api/0/Private/Balance", "nonce=2", &secret).unwrap();
        assert_ne!(a, c);
        // A wrong path changes the signature.
        let d = KrakenConnector::api_sign("/api/0/Private/Balance2", "nonce=1", &secret).unwrap();
        assert_ne!(a, d);
    }

    #[test]
    fn api_sign_rejects_a_non_base64_secret() {
        let err =
            KrakenConnector::api_sign("/x", "nonce=1", "not base64!!").expect_err("must fail");
        assert!(matches!(err, ExchangeError::Config(_)), "{err}");
    }

    #[test]
    fn error_mapping() {
        assert!(matches!(
            map_kraken_error(&["EGeneral:Invalid key".to_string()]),
            ExchangeError::Auth(_)
        ));
        assert!(matches!(
            map_kraken_error(&["EOrder:Insufficient funds".to_string()]),
            ExchangeError::InsufficientBalance { .. }
        ));
        assert!(matches!(
            map_kraken_error(&["EGeneral:Order minimum not met".to_string()]),
            ExchangeError::Rejected(_)
        ));
        assert!(matches!(
            map_kraken_error(&["EAPI:Rate limit exceeded".to_string()]),
            ExchangeError::RateLimited(_)
        ));
    }

    #[test]
    fn trade_message_parsing() {
        let payload = r#"{"channel":"trade","type":"update","data":[{"symbol":"SOL/USD","side":"buy","qty":1.25,"price":142.3,"ord_type":"limit","trade_id":7,"timestamp":"2026-01-01T00:00:00.000000Z"}]}"#;
        let tick = parse_trade_message(payload).expect("tick");
        assert_eq!(tick.venue, Venue::Kraken);
        assert_eq!(tick.symbol, "SOL/USDT");
        assert!((tick.price - 142.3).abs() < 1e-9);
        assert!((tick.size - 1.25).abs() < 1e-9);
    }

    #[test]
    fn malformed_messages_are_skipped() {
        assert!(parse_trade_message("not json").is_none());
        assert!(parse_trade_message(r#"{"channel":"heartbeat"}"#).is_none());
        assert!(parse_trade_message(r#"{"channel":"trade","type":"update","data":[]}"#).is_none());
    }

    #[tokio::test]
    async fn live_trading_guard_blocks_orders_without_touching_the_network() {
        let cfg = ConnectorConfig {
            base_url: "http://127.0.0.1:1".into(), // nothing listens here
            api_key: Some("key".into()),
            api_secret: Some(base64::engine::general_purpose::STANDARD.encode(b"0123456789abcdef")),
            recv_timeout: Duration::from_millis(200),
            ..ConnectorConfig::default()
        };
        let connector = KrakenConnector::new(cfg, false).unwrap();

        let req = OrderRequest::market(Venue::Kraken, "SOL/USDT", Side::Buy, 1.0);
        let err = connector
            .place_order(&req)
            .await
            .expect_err("must be blocked");
        match err {
            ExchangeError::Config(msg) => assert_eq!(msg, LIVE_TRADING_DISABLED),
            other => panic!("expected a config error, got {other}"),
        }
        let err = connector
            .cancel_order("TXID")
            .await
            .expect_err("must be blocked");
        assert!(matches!(err, ExchangeError::Config(_)), "{err}");
    }
}

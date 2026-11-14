use serde_json::{json, Value};

pub struct Context {
    pub core_url: String,
    pub http: reqwest::Client,
}

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "execute_trade",
            "description": "Submit a buy or sell order to the configured venue. The order passes through a non-bypassable risk layer; a rejection is final and must not be retried without a new plan. In paper mode the trade is simulated. An accepted order is not a fill: a limit order rests until the price crosses it, so verify the outcome with get_orders. A stop or take_profit is a reduce-only exit the venue holds: it fires even if this process is down, so use one to protect an open position, and give two exits the same oco_group on the paper venue so filling one cancels the other.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": {"type": "string", "description": "Trading pair, e.g. SOL/USDT"},
                    "side": {"type": "string", "enum": ["buy", "sell"]},
                    "qty": {"type": "number", "description": "Order size in units of the base asset"},
                    "venue": {"type": "string", "description": "Optional venue override; defaults to the engine venue"},
                    "order_type": {"type": "string", "enum": ["market", "limit", "stop", "take_profit"], "description": "market (default) fills now; limit rests at limit_price; stop and take_profit wait for trigger_price"},
                    "limit_price": {"type": "number", "description": "Required for a limit order"},
                    "trigger_price": {"type": "number", "description": "Required for stop and take_profit: the price that fires the exit"},
                    "reduce_only": {"type": "boolean", "description": "Required for stop and take_profit: the order may only close a position"},
                    "oco_group": {"type": "string", "description": "Orders sharing a group cancel each other when one fills (paper venue)"},
                    "rationale": {"type": "string", "description": "Why this trade is being made; stored with the trade"}
                },
                "required": ["symbol", "side", "qty"]
            }
        }),
        json!({
            "name": "get_portfolio",
            "description": "Current equity, cash, realized and unrealized PnL, and open positions.",
            "inputSchema": {"type": "object", "properties": {}}
        }),
        json!({
            "name": "get_orders",
            "description": "Orders the engine is tracking, newest first, with venue-confirmed fills. An accepted or partially_filled order is still resting and is not yet a position; only the fills in this list are real. Check here to verify an execution instead of assuming it filled.",
            "inputSchema": {"type": "object", "properties": {}}
        }),
        json!({
            "name": "cancel_order",
            "description": "Cancel a resting order by its order_id from get_orders. Only a resting order can be cancelled, and the venue must confirm. Cancelling reduces exposure so it is never rejected by the risk layer; a fill that landed just before the cancel is still booked.",
            "inputSchema": {
                "type": "object",
                "properties": {"order_id": {"type": "string"}},
                "required": ["order_id"]
            }
        }),
        json!({
            "name": "get_positions",
            "description": "Open positions with entry, mark and unrealized PnL.",
            "inputSchema": {"type": "object", "properties": {}}
        }),
        json!({
            "name": "get_market_data",
            "description": "Recent market ticks for the requested symbol, newest last.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": {"type": "string"},
                    "lookback": {"type": "number", "description": "How many ticks to return (default 50)"}
                }
            }
        }),
        json!({
            "name": "get_risk_state",
            "description": "Circuit breaker state, drawdown, daily PnL, exposure and the configured limits.",
            "inputSchema": {"type": "object", "properties": {}}
        }),
        json!({
            "name": "get_trade_history",
            "description": "Recent executed trades with prices, fees and the agent rationale.",
            "inputSchema": {
                "type": "object",
                "properties": {"limit": {"type": "number"}}
            }
        }),
        json!({
            "name": "get_journal",
            "description": "Query the durable trade journal: fills with realized PnL, fees and the rationale that placed them, plus totals for the filtered range. Filter by symbol, side, venue, source and time. The summary covers every matching fill; `fills` is only the page, so use it to answer 'how did SOL do yesterday' rather than paging through the ring.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": {"type": "string", "description": "Trading pair, e.g. SOL/USDT"},
                    "side": {"type": "string", "enum": ["buy", "sell"]},
                    "venue": {"type": "string", "description": "paper, binance, kraken or hyperliquid"},
                    "source": {"type": "string", "description": "Who placed the fill, e.g. paper or engine"},
                    "from_ms": {"type": "number", "description": "Earliest fill timestamp, inclusive"},
                    "to_ms": {"type": "number", "description": "Latest fill timestamp, inclusive"},
                    "limit": {"type": "number", "description": "Page size, default 100, at most 1000"}
                }
            }
        }),
        json!({
            "name": "fetch_onchain_data",
            "description": "Read-only on-chain data: native balance and token holdings on Solana or EVM chains.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chain": {"type": "string", "enum": ["solana", "ethereum", "base"]},
                    "address": {"type": "string"},
                    "token": {"type": "string", "description": "Optional ERC-20 contract address"}
                },
                "required": ["chain", "address"]
            }
        }),
        json!({
            "name": "set_strategy",
            "description": "Hot-swap the sandboxed WASM strategy. The new module is validated and loaded without restarting the engine.",
            "inputSchema": {
                "type": "object",
                "properties": {"path": {"type": "string", "description": "Absolute path to the .wasm module"}},
                "required": ["path"]
            }
        }),
        json!({
            "name": "get_agent_logs",
            "description": "Engine and agent logs, newest first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": {"type": "number"},
                    "source": {"type": "string"}
                }
            }
        }),
    ]
}

pub async fn call(ctx: &Context, name: &str, args: Value) -> Result<String, String> {
    let result = match name {
        "execute_trade" => execute_trade(ctx, args).await,
        "get_portfolio" => get(ctx, "/api/portfolio").await,
        "get_orders" => get(ctx, "/api/orders").await,
        "cancel_order" => cancel_order(ctx, args).await,
        "get_positions" => get(ctx, "/api/positions").await,
        "get_market_data" => market_data(ctx, args).await,
        "get_risk_state" => get(ctx, "/api/risk").await,
        "get_trade_history" => trade_history(ctx, args).await,
        "get_journal" => journal(ctx, args).await,
        "fetch_onchain_data" => onchain(ctx, args).await,
        "set_strategy" => set_strategy(ctx, args).await,
        "get_agent_logs" => logs(ctx, args).await,
        other => return Err(format!("unknown tool: {other}")),
    }?;
    Ok(result)
}

async fn execute_trade(ctx: &Context, args: Value) -> Result<String, String> {
    let symbol = args
        .get("symbol")
        .and_then(|v| v.as_str())
        .ok_or("execute_trade requires a symbol")?
        .to_string();
    let side = args
        .get("side")
        .and_then(|v| v.as_str())
        .ok_or("execute_trade requires a side")?
        .to_string();
    let qty = args
        .get("qty")
        .and_then(|v| v.as_f64())
        .ok_or("execute_trade requires a numeric qty")?;
    if !qty.is_finite() || qty <= 0.0 {
        return Err("qty must be a positive finite number".into());
    }

    let mut body = json!({
        "symbol": symbol,
        "side": side,
        "qty": qty,
        "order_type": args.get("order_type").and_then(|v| v.as_str()).unwrap_or("market"),
        "rationale": args.get("rationale").and_then(|v| v.as_str()).unwrap_or("")
    });
    if let Some(venue) = args.get("venue").and_then(|v| v.as_str()) {
        body["venue"] = json!(venue);
    }
    if let Some(limit_price) = args.get("limit_price").and_then(|v| v.as_f64()) {
        body["limit_price"] = json!(limit_price);
    }
    if let Some(trigger_price) = args.get("trigger_price").and_then(|v| v.as_f64()) {
        body["trigger_price"] = json!(trigger_price);
    }
    if let Some(reduce_only) = args.get("reduce_only").and_then(|v| v.as_bool()) {
        body["reduce_only"] = json!(reduce_only);
    }
    if let Some(oco_group) = args.get("oco_group").and_then(|v| v.as_str()) {
        body["oco_group"] = json!(oco_group);
    }

    let response = ctx
        .http
        .post(format!("{}/api/orders", ctx.core_url))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("core unreachable: {e}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| format!("could not read the core response: {e}"))?;
    if status.is_success() {
        Ok(text)
    } else {
        // A rejected order is a normal outcome, not a transport failure: return
        // the verdict so the agent can reason about it.
        Ok(json!({"approved": false, "status": status.as_u16(), "response": text}).to_string())
    }
}

async fn cancel_order(ctx: &Context, args: Value) -> Result<String, String> {
    let order_id = args
        .get("order_id")
        .and_then(|v| v.as_str())
        .ok_or("cancel_order requires an order_id")?;
    if order_id.trim().is_empty() {
        return Err("cancel_order requires a non-empty order_id".into());
    }
    let response = ctx
        .http
        .delete(format!(
            "{}/api/orders/{}",
            ctx.core_url,
            urlencode(order_id)
        ))
        .send()
        .await
        .map_err(|e| format!("core unreachable: {e}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| format!("could not read the core response: {e}"))?;
    if status.is_success() {
        Ok(text)
    } else {
        // A refused cancel leaves the order live. That is an outcome the agent
        // has to reason about, not a transport failure.
        Ok(json!({"ok": false, "status": status.as_u16(), "response": text}).to_string())
    }
}

/// Percent-encode an order id for a path segment. Venue ids are alphanumeric in
/// practice, but a client id may not be, and a raw `/` or `?` would change which
/// order the request addresses.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

async fn get(ctx: &Context, path: &str) -> Result<String, String> {
    let response = ctx
        .http
        .get(format!("{}{}", ctx.core_url, path))
        .send()
        .await
        .map_err(|e| format!("core unreachable: {e}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| format!("could not read the core response: {e}"))?;
    if status.is_success() {
        Ok(text)
    } else {
        Err(format!("core returned {status}: {text}"))
    }
}

async fn market_data(ctx: &Context, args: Value) -> Result<String, String> {
    let lookback = args
        .get("lookback")
        .and_then(|v| v.as_f64())
        .unwrap_or(50.0)
        .clamp(1.0, 1_000.0) as usize;
    let ticks = get(ctx, &format!("/api/ticks?limit={lookback}")).await?;
    let parsed: Value = serde_json::from_str(&ticks).map_err(|e| e.to_string())?;
    let symbol = args.get("symbol").and_then(|v| v.as_str());
    let filtered: Vec<Value> = match symbol {
        Some(symbol) => parsed
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter(|t| t.get("symbol").and_then(|s| s.as_str()) == Some(symbol))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default(),
        None => parsed.as_array().cloned().unwrap_or_default(),
    };
    Ok(json!({"symbol": symbol, "ticks": filtered}).to_string())
}

async fn trade_history(ctx: &Context, args: Value) -> Result<String, String> {
    let limit = args
        .get("limit")
        .and_then(|v| v.as_f64())
        .unwrap_or(50.0)
        .clamp(1.0, 500.0) as usize;
    get(ctx, &format!("/api/trades?limit={limit}")).await
}

async fn journal(ctx: &Context, args: Value) -> Result<String, String> {
    let mut query: Vec<(&str, String)> = Vec::new();
    for key in ["symbol", "side", "venue", "source"] {
        if let Some(value) = args.get(key).and_then(|v| v.as_str()) {
            let value = value.trim();
            if !value.is_empty() {
                query.push((key, value.to_string()));
            }
        }
    }
    for key in ["from_ms", "to_ms"] {
        if let Some(value) = args.get(key).and_then(|v| v.as_i64()) {
            query.push((key, value.to_string()));
        }
    }
    let limit = args
        .get("limit")
        .and_then(|v| v.as_f64())
        .unwrap_or(100.0)
        .clamp(1.0, 1_000.0) as i64;
    query.push(("limit", limit.to_string()));

    let response = ctx
        .http
        .get(format!("{}/api/journal", ctx.core_url))
        .query(&query)
        .send()
        .await
        .map_err(|e| format!("core unreachable: {e}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| format!("could not read the core response: {e}"))?;
    if status.is_success() {
        Ok(text)
    } else {
        Err(format!("core returned {status}: {text}"))
    }
}

async fn logs(ctx: &Context, args: Value) -> Result<String, String> {
    let limit = args
        .get("limit")
        .and_then(|v| v.as_f64())
        .unwrap_or(100.0)
        .clamp(1.0, 500.0) as usize;
    let source = args.get("source").and_then(|v| v.as_str());
    let path = match source {
        Some(source) => format!("/api/logs?limit={limit}&source={source}"),
        None => format!("/api/logs?limit={limit}"),
    };
    get(ctx, &path).await
}

async fn onchain(ctx: &Context, args: Value) -> Result<String, String> {
    let chain = args
        .get("chain")
        .and_then(|v| v.as_str())
        .ok_or("fetch_onchain_data requires a chain")?
        .to_string();
    let address = args
        .get("address")
        .and_then(|v| v.as_str())
        .ok_or("fetch_onchain_data requires an address")?
        .to_string();
    let mut body = json!({"chain": chain, "address": address});
    if let Some(token) = args.get("token").and_then(|v| v.as_str()) {
        body["token"] = json!(token);
    }
    let response = ctx
        .http
        .post(format!("{}/api/onchain", ctx.core_url))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("core unreachable: {e}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| format!("could not read the core response: {e}"))?;
    if status.is_success() {
        Ok(text)
    } else {
        Err(format!("core returned {status}: {text}"))
    }
}

async fn set_strategy(ctx: &Context, args: Value) -> Result<String, String> {
    let path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("set_strategy requires a path")?
        .to_string();
    let response = ctx
        .http
        .post(format!("{}/api/strategy/reload", ctx.core_url))
        .json(&json!({"path": path}))
        .send()
        .await
        .map_err(|e| format!("core unreachable: {e}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| format!("could not read the core response: {e}"))?;
    if status.is_success() {
        Ok(text)
    } else {
        Err(format!("core returned {status}: {text}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing listens here, so a call that reaches the transport fails fast.
    fn unreachable() -> Context {
        Context {
            core_url: "http://127.0.0.1:1".into(),
            http: reqwest::Client::new(),
        }
    }

    fn minimal_args(name: &str) -> Value {
        match name {
            "execute_trade" => json!({"symbol": "SOL/USDT", "side": "buy", "qty": 1.0}),
            "cancel_order" => json!({"order_id": "o-1"}),
            "fetch_onchain_data" => {
                json!({"chain": "solana", "address": "11111111111111111111111111111111"})
            }
            "set_strategy" => json!({"path": "/tmp/strategy.wasm"}),
            _ => json!({}),
        }
    }

    #[tokio::test]
    async fn every_advertised_tool_is_dispatchable() {
        let ctx = unreachable();
        for definition in definitions() {
            let name = definition["name"].as_str().expect("a tool has a name");
            let error = call(&ctx, name, minimal_args(name))
                .await
                .expect_err("an unreachable core cannot answer");
            assert!(
                error.contains("core unreachable"),
                "{name} is advertised but not wired up: {error}"
            );
        }
    }

    #[tokio::test]
    async fn an_unknown_tool_is_refused() {
        let error = call(&unreachable(), "transfer_everything", json!({}))
            .await
            .expect_err("an unknown tool must not run");
        assert!(error.contains("unknown tool"));
    }

    #[tokio::test]
    async fn arguments_are_validated_before_the_core_is_called() {
        let ctx = unreachable();
        let error = call(
            &ctx,
            "execute_trade",
            json!({"symbol": "SOL/USDT", "side": "buy"}),
        )
        .await
        .expect_err("a trade without a size must not be sent");
        assert!(error.contains("numeric qty"), "{error}");

        let error = call(&ctx, "cancel_order", json!({"order_id": "  "}))
            .await
            .expect_err("a cancel without an order must not be sent");
        assert!(error.contains("non-empty order_id"), "{error}");

        let error = call(
            &ctx,
            "execute_trade",
            json!({"symbol": "SOL/USDT", "side": "buy", "qty": 0.0}),
        )
        .await
        .expect_err("a zero-sized trade must not be sent");
        assert!(error.contains("positive finite"), "{error}");
    }

    #[test]
    fn an_order_id_cannot_change_the_path_it_addresses() {
        assert_eq!(urlencode("o-1"), "o-1");
        assert_eq!(urlencode("abc_123.~"), "abc_123.~");
        assert_eq!(urlencode("../../orders"), "..%2F..%2Forders");
        assert_eq!(urlencode("o 1?x=1"), "o%201%3Fx%3D1");
        assert_eq!(urlencode("o%2F1"), "o%252F1");
    }

    #[test]
    fn cancelling_requires_an_order_id() {
        let definitions = definitions();
        let cancel = definitions
            .iter()
            .find(|tool| tool["name"] == "cancel_order")
            .expect("cancel_order is advertised");
        assert_eq!(cancel["inputSchema"]["required"], json!(["order_id"]));
    }

    /// The model can only ask for a protective exit if the schema names it and
    /// carries the field that arms it.
    #[test]
    fn the_trade_schema_offers_the_conditional_order_types() {
        let definitions = definitions();
        let trade = definitions
            .iter()
            .find(|tool| tool["name"] == "execute_trade")
            .expect("execute_trade is advertised");
        let properties = &trade["inputSchema"]["properties"];
        assert_eq!(
            properties["order_type"]["enum"],
            json!(["market", "limit", "stop", "take_profit"])
        );
        for field in ["trigger_price", "reduce_only", "oco_group"] {
            assert!(
                properties.get(field).is_some(),
                "the trade schema does not carry {field}"
            );
        }
    }
}

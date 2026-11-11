use serde_json::{json, Value};

pub struct Context {
    pub core_url: String,
    pub http: reqwest::Client,
}

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "execute_trade",
            "description": "Submit a buy or sell order to the configured venue. The order passes through a non-bypassable risk layer; a rejection is final and must not be retried without a new plan. In paper mode the trade is simulated.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": {"type": "string", "description": "Trading pair, e.g. SOL/USDT"},
                    "side": {"type": "string", "enum": ["buy", "sell"]},
                    "qty": {"type": "number", "description": "Order size in units of the base asset"},
                    "venue": {"type": "string", "description": "Optional venue override; defaults to the engine venue"},
                    "order_type": {"type": "string", "enum": ["market", "limit"]},
                    "limit_price": {"type": "number"},
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
        "get_positions" => get(ctx, "/api/positions").await,
        "get_market_data" => market_data(ctx, args).await,
        "get_risk_state" => get(ctx, "/api/risk").await,
        "get_trade_history" => trade_history(ctx, args).await,
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

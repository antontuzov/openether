//! MCP server exposing the openEther trading core to an LLM agent.
//!
//! The server speaks JSON-RPC 2.0 over stdio and implements the Model Context
//! Protocol tool surface. Every tool call is forwarded to the core engine,
//! where the risk layer evaluates it before any order is placed.

use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::sync::Arc;

mod tools;

#[derive(Debug, thiserror::Error)]
enum ServerError {
    #[error("unknown method: {0}")]
    UnknownMethod(String),
    #[error("invalid params: {0}")]
    InvalidParams(String),
    #[error("tool error: {0}")]
    Tool(String),
}

impl From<String> for ServerError {
    fn from(message: String) -> Self {
        ServerError::Tool(message)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let core_url = std::env::var("CORE_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into());
    let admin_token = std::env::var("OPENETHER_ADMIN_TOKEN").ok();
    let http = tools::http_client(admin_token.as_deref()).map_err(anyhow::Error::msg)?;
    let ctx = Arc::new(tools::Context { core_url, http });

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(e) => {
                let error = json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": {"code": -32700, "message": format!("parse error: {e}")}
                });
                writeln!(stdout, "{error}")?;
                stdout.flush()?;
                continue;
            }
        };

        let id = request.get("id").cloned();
        let method = request
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or_default()
            .to_string();
        let params = request.get("params").cloned().unwrap_or(json!({}));

        // Notifications carry no id and expect no response.
        if id.is_none() {
            continue;
        }
        let id = id.unwrap();

        let result = handle(&ctx, &method, params).await;
        let response = match result {
            Ok(value) => json!({"jsonrpc": "2.0", "id": id, "result": value}),
            Err(ServerError::UnknownMethod(m)) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("unknown method: {m}")}
            }),
            Err(ServerError::InvalidParams(m)) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32602, "message": m}
            }),
            Err(ServerError::Tool(m)) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32000, "message": m}
            }),
        };
        writeln!(stdout, "{response}")?;
        stdout.flush()?;
    }

    Ok(())
}

async fn handle(
    ctx: &Arc<tools::Context>,
    method: &str,
    params: Value,
) -> Result<Value, ServerError> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "openether", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "openEther exposes a risk-gated crypto trading core. Every order passes through a non-bypassable risk layer; a rejected trade must never be retried without a new plan."
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools::definitions()})),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(|n| n.as_str())
                .ok_or_else(|| ServerError::InvalidParams("missing tool name".into()))?
                .to_string();
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let result = tools::call(ctx, &name, arguments).await?;
            Ok(json!({
                "content": [{"type": "text", "text": result}],
                "isError": false
            }))
        }
        other => Err(ServerError::UnknownMethod(other.to_string())),
    }
}

//! Mean-reversion strategy for the openEther WASM sandbox.
//!
//! The guest keeps a short price history and buys when the price falls a set
//! distance below its own average, selling once it has reverted, hit a target
//! or broken the stop. Like every guest it proposes; the host's risk layer
//! decides.
//!
//! Guest ABI (see `openether_core::strategy`):
//!   `openether_alloc(len) -> ptr`
//!   `openether_dealloc(ptr, len)`
//!   `openether_on_tick(ctx_ptr, ctx_len) -> ptr`  (NUL-terminated JSON out)

use serde_json::{json, Value};
use std::cell::RefCell;

const HISTORY: usize = 128;

thread_local! {
    static INPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static OUTPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static PRICES: RefCell<Vec<f64>> = const { RefCell::new(Vec::new()) };
}

#[no_mangle]
pub extern "C" fn openether_alloc(len: i32) -> i32 {
    if len < 0 {
        return 0;
    }
    INPUT.with(|buffer| {
        let mut buffer = buffer.borrow_mut();
        buffer.clear();
        buffer.resize(len as usize, 0);
        buffer.as_ptr() as i32
    })
}

#[no_mangle]
pub extern "C" fn openether_dealloc(_ptr: i32, _len: i32) {}

#[no_mangle]
pub extern "C" fn openether_on_tick(_ctx_ptr: i32, ctx_len: i32) -> i32 {
    let raw = INPUT.with(|buffer| {
        let buffer = buffer.borrow();
        let len = (ctx_len.max(0) as usize).min(buffer.len());
        buffer[..len].to_vec()
    });

    let ctx: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
    let signals = evaluate(&ctx);
    let encoded = serde_json::to_vec(&json!({ "signals": signals })).unwrap_or_default();

    OUTPUT.with(|buffer| {
        let mut buffer = buffer.borrow_mut();
        buffer.clear();
        let mut bytes = encoded;
        bytes.push(0);
        buffer.extend_from_slice(&bytes);
        buffer.as_ptr() as i32
    })
}

fn evaluate(ctx: &Value) -> Vec<Value> {
    let symbol = ctx
        .get("symbol")
        .and_then(|v| v.as_str())
        .unwrap_or("SOL/USDT")
        .to_string();
    let price = ctx.get("price").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let equity = ctx.get("equity_usd").and_then(|v| v.as_f64()).unwrap_or(0.0);

    if !price.is_finite() || price <= 0.0 || equity <= 0.0 {
        return Vec::new();
    }

    let params = ctx.get("params").cloned().unwrap_or(json!({}));
    let window = params
        .get("window")
        .and_then(|v| v.as_u64())
        .unwrap_or(20)
        .clamp(2, 120) as usize;
    let entry_pct = params
        .get("entry_pct")
        .and_then(|v| v.as_f64())
        .unwrap_or(1.5);
    let exit_pct = params
        .get("exit_pct")
        .and_then(|v| v.as_f64())
        .unwrap_or(1.0);
    let stop_loss_pct = params
        .get("stop_loss_pct")
        .and_then(|v| v.as_f64())
        .unwrap_or(2.0);
    let max_position_pct = params
        .get("max_position_pct")
        .and_then(|v| v.as_f64())
        .unwrap_or(20.0);
    let min_notional = params
        .get("min_notional_usd")
        .and_then(|v| v.as_f64())
        .unwrap_or(10.0);

    PRICES.with(|history| {
        let mut history = history.borrow_mut();
        history.push(price);
        if history.len() > HISTORY {
            history.remove(0);
        }

        let (position_qty, avg_entry) = position(ctx, &symbol);

        // Exits first: capital preservation beats a new entry.
        if position_qty > 0.0 && avg_entry > 0.0 {
            let change = (price - avg_entry) / avg_entry * 100.0;
            if change <= -stop_loss_pct {
                return vec![sell(&symbol, position_qty, 1.0, format!(
                    "stop loss hit: {change:.2}% below the entry price"
                ))];
            }
            if change >= exit_pct {
                return vec![sell(&symbol, position_qty, 0.8, format!(
                    "mean reverted: {change:.2}% above the entry price"
                ))];
            }
        }

        if history.len() < window || position_qty > 0.0 {
            return Vec::new();
        }

        let mean = average(&history[history.len() - window..]);
        if mean <= 0.0 {
            return Vec::new();
        }
        let deviation = (price - mean) / mean * 100.0;

        // Only a real discount to the average is worth buying.
        if deviation > -entry_pct {
            return Vec::new();
        }

        let budget = equity * (max_position_pct / 100.0) * 0.5;
        let qty = (budget / price * 10_000.0).floor() / 10_000.0;
        if qty <= 0.0 || qty * price < min_notional {
            return Vec::new();
        }

        let confidence = (deviation.abs() / (entry_pct * 2.0)).clamp(0.05, 1.0);
        vec![json!({
            "symbol": symbol,
            "side": "buy",
            "qty": qty,
            "confidence": confidence,
            "reason": format!("mean reversion: price {deviation:.2}% below its {window}-bar average"),
        })]
    })
}

fn position(ctx: &Value, symbol: &str) -> (f64, f64) {
    let entry = ctx
        .get("positions")
        .and_then(|p| p.as_array())
        .and_then(|positions| {
            positions
                .iter()
                .find(|p| p.get("symbol").and_then(|s| s.as_str()) == Some(symbol))
        });
    let qty = entry
        .and_then(|p| p.get("qty").and_then(|q| q.as_f64()))
        .unwrap_or(0.0);
    let avg_entry = entry
        .and_then(|p| p.get("avg_entry").and_then(|q| q.as_f64()))
        .unwrap_or(0.0);
    (qty, avg_entry)
}

fn sell(symbol: &str, qty: f64, confidence: f64, reason: String) -> Value {
    json!({
        "symbol": symbol,
        "side": "sell",
        "qty": qty,
        "confidence": confidence,
        "reason": reason,
    })
}

fn average(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

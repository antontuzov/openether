//! Breakout strategy for the openEther WASM sandbox.
//!
//! Buys when the price clears the highest close of the previous `lookback`
//! ticks by a small buffer, and leaves on a stop, a target or a breakdown
//! through the matching low. Like every guest it proposes; the host's risk
//! layer decides.
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
    let lookback = params
        .get("lookback")
        .and_then(|v| v.as_u64())
        .unwrap_or(20)
        .clamp(2, 120) as usize;
    let buffer_pct = params
        .get("buffer_pct")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.1);
    let stop_loss_pct = params
        .get("stop_loss_pct")
        .and_then(|v| v.as_f64())
        .unwrap_or(2.0);
    let take_profit_pct = params
        .get("take_profit_pct")
        .and_then(|v| v.as_f64())
        .unwrap_or(4.0);
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

        // The previous `lookback` closes, excluding the tick in hand. A level
        // that includes the current price could never be broken by it.
        let prior = if history.len() > lookback {
            &history[history.len() - lookback - 1..history.len() - 1]
        } else {
            &[][..]
        };

        if position_qty > 0.0 && avg_entry > 0.0 {
            let change = (price - avg_entry) / avg_entry * 100.0;
            if change <= -stop_loss_pct {
                return vec![sell(&symbol, position_qty, 1.0, format!(
                    "stop loss hit: {change:.2}% below the entry price"
                ))];
            }
            if change >= take_profit_pct {
                return vec![sell(&symbol, position_qty, 0.9, format!(
                    "take profit hit: {change:.2}% above the entry price"
                ))];
            }
            if let Some(low) = prior.iter().cloned().reduce(f64::min) {
                if price < low {
                    return vec![sell(&symbol, position_qty, 0.8, format!(
                        "breakdown: price closed below the {lookback}-bar low"
                    ))];
                }
            }
        }

        if prior.is_empty() || position_qty > 0.0 {
            return Vec::new();
        }

        let Some(high) = prior.iter().cloned().reduce(f64::max) else {
            return Vec::new();
        };
        let trigger = high * (1.0 + buffer_pct / 100.0);
        if price <= trigger {
            return Vec::new();
        }

        let budget = equity * (max_position_pct / 100.0) * 0.5;
        let qty = (budget / price * 10_000.0).floor() / 10_000.0;
        if qty <= 0.0 || qty * price < min_notional {
            return Vec::new();
        }

        let extension = (price - high) / high * 100.0;
        let confidence = (extension * 2.0).clamp(0.05, 1.0);
        vec![json!({
            "symbol": symbol,
            "side": "buy",
            "qty": qty,
            "confidence": confidence,
            "reason": format!("breakout: price {extension:.2}% above the {lookback}-bar high"),
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

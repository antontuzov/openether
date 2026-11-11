//! Momentum strategy for the openEther WASM sandbox.
//!
//! The guest is deliberately dumb: it receives a JSON snapshot of the market and
//! the portfolio, and returns JSON trade proposals. It never touches the
//! network, the clock or the filesystem, and it cannot place an order — every
//! proposal is evaluated by the risk layer in the host process.
//!
//! Guest ABI (see `openether_core::strategy`):
//!   `openether_alloc(len) -> ptr`
//!   `openether_dealloc(ptr, len)`
//!   `openether_on_tick(ctx_ptr, ctx_len) -> ptr`  (NUL-terminated JSON out)

use serde_json::{json, Value};
use std::cell::RefCell;

const HISTORY: usize = 64;
const FAST_WINDOW: usize = 5;
const SLOW_WINDOW: usize = 20;

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
        buffer.extend_from_slice(&encoded);
        buffer.push(0);
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
    let max_position_pct = params
        .get("max_position_pct")
        .and_then(|v| v.as_f64())
        .unwrap_or(20.0);
    let min_notional = params
        .get("min_notional_usd")
        .and_then(|v| v.as_f64())
        .unwrap_or(10.0);
    let stop_loss_pct = params
        .get("stop_loss_pct")
        .and_then(|v| v.as_f64())
        .unwrap_or(2.0);
    let take_profit_pct = params
        .get("take_profit_pct")
        .and_then(|v| v.as_f64())
        .unwrap_or(4.0);

    PRICES.with(|history| {
        let mut history = history.borrow_mut();
        history.push(price);
        if history.len() > HISTORY {
            history.remove(0);
        }

        // Existing position on this symbol.
        let position_qty = ctx
            .get("positions")
            .and_then(|p| p.as_array())
            .and_then(|positions| {
                positions
                    .iter()
                    .find(|p| p.get("symbol").and_then(|s| s.as_str()) == Some(symbol.as_str()))
            })
            .and_then(|p| p.get("qty").and_then(|q| q.as_f64()))
            .unwrap_or(0.0);
        let avg_entry = ctx
            .get("positions")
            .and_then(|p| p.as_array())
            .and_then(|positions| {
                positions
                    .iter()
                    .find(|p| p.get("symbol").and_then(|s| s.as_str()) == Some(symbol.as_str()))
            })
            .and_then(|p| p.get("avg_entry").and_then(|q| q.as_f64()))
            .unwrap_or(0.0);

        // Exits come first: capital preservation beats new entries.
        if position_qty > 0.0 && avg_entry > 0.0 {
            let change = (price - avg_entry) / avg_entry * 100.0;
            if change <= -stop_loss_pct {
                return vec![json!({
                    "symbol": symbol,
                    "side": "sell",
                    "qty": position_qty,
                    "confidence": 1.0,
                    "reason": format!("stop loss hit: {change:.2}% below the entry price")
                })];
            }
            if change >= take_profit_pct {
                return vec![json!({
                    "symbol": symbol,
                    "side": "sell",
                    "qty": position_qty,
                    "confidence": 0.9,
                    "reason": format!("take profit hit: {change:.2}% above the entry price")
                })];
            }
        }

        if history.len() < SLOW_WINDOW {
            return Vec::new();
        }

        let fast = average(&history[history.len() - FAST_WINDOW..]);
        let slow = average(&history[history.len() - SLOW_WINDOW..]);
        if fast <= 0.0 || slow <= 0.0 {
            return Vec::new();
        }
        let spread = (fast - slow) / slow * 100.0;

        // A flat or noisy market is not a trade.
        if spread.abs() < 0.15 {
            return Vec::new();
        }

        if spread < 0.0 && position_qty > 0.0 {
            return vec![json!({
                "symbol": symbol,
                "side": "sell",
                "qty": position_qty,
                "confidence": 0.8,
                "reason": format!("momentum reversed: fast SMA {spread:.2}% below the slow SMA")
            })];
        }
        if spread < 0.0 {
            return Vec::new();
        }

        // Cap the position at the configured share of the equity, and skip the
        // trade when the resulting order would be too small to be meaningful.
        let budget = equity * (max_position_pct / 100.0) * 0.5;
        let qty = (budget / price * 10_000.0).floor() / 10_000.0;
        if qty <= 0.0 || qty * price < min_notional {
            return Vec::new();
        }
        let confidence = (spread / 2.0).clamp(0.05, 1.0);

        vec![json!({
            "symbol": symbol,
            "side": "buy",
            "qty": qty,
            "confidence": confidence,
            "reason": format!("momentum breakout: fast SMA {spread:.2}% above the slow SMA")
        })]
    })
}

fn average(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

//! The built-in strategy registry.
//!
//! A registry entry says what a strategy is for and which artifact it expects.
//! The selector resolves a name against the artifact directory instead of
//! taking an arbitrary absolute path from a caller.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub struct StrategyDef {
    pub name: &'static str,
    pub description: &'static str,
    /// File name inside the strategy directory. The file stem is also the
    /// strategy's name once loaded.
    pub wasm: &'static str,
}

pub const STRATEGIES: &[StrategyDef] = &[
    StrategyDef {
        name: "momentum",
        description:
            "Follows a fast/slow moving-average crossover and leaves on a stop or a target.",
        wasm: "momentum.wasm",
    },
    StrategyDef {
        name: "mean-reversion",
        description: "Buys a set distance below the recent average and exits when it reverts.",
        wasm: "mean-reversion.wasm",
    },
    StrategyDef {
        name: "breakout",
        description:
            "Buys a close above the recent high by a small buffer and leaves on a breakdown.",
        wasm: "breakout.wasm",
    },
];

pub fn find(name: &str) -> Option<&'static StrategyDef> {
    let name = name.trim();
    STRATEGIES
        .iter()
        .find(|def| def.name.eq_ignore_ascii_case(name))
}

pub fn path_for(dir: &Path, def: &StrategyDef) -> PathBuf {
    dir.join(def.wasm)
}

/// Strategy-specific parameters.
///
/// The risk-derived keys (`max_position_pct`, `min_notional_usd`) are not in
/// here on purpose: the engine fills those in from the risk limits on every
/// tick, so selecting a strategy cannot widen a limit.
pub fn defaults(name: &str) -> Value {
    match name {
        "mean-reversion" => json!({
            "window": 20,
            "entry_pct": 1.5,
            "exit_pct": 1.0,
            "stop_loss_pct": 2.0,
        }),
        "breakout" => json!({
            "lookback": 20,
            "buffer_pct": 0.1,
            "stop_loss_pct": 2.0,
            "take_profit_pct": 4.0,
        }),
        _ => json!({
            "stop_loss_pct": 2.0,
            "take_profit_pct": 4.0,
        }),
    }
}

/// The defaults with caller-supplied keys layered on top.
pub fn merge(defaults: Value, overrides: &Value) -> Value {
    let mut merged = if defaults.is_object() {
        defaults
    } else {
        json!({})
    };
    if let Some(overrides) = overrides.as_object() {
        for (key, value) in overrides {
            merged[key] = value.clone();
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registered_strategy_resolves_and_has_defaults() {
        for def in STRATEGIES {
            assert_eq!(find(def.name).map(|d| d.name), Some(def.name));
            assert!(defaults(def.name).is_object());
            assert!(def.wasm.ends_with(".wasm"));
        }
    }

    #[test]
    fn names_are_matched_without_case_or_padding() {
        assert_eq!(find("  BREAKOUT ").map(|d| d.name), Some("breakout"));
        assert!(find("martingale").is_none());
    }

    #[test]
    fn overrides_are_layered_on_the_defaults() {
        let merged = merge(defaults("breakout"), &json!({"lookback": 55}));
        assert_eq!(merged["lookback"], 55);
        assert_eq!(merged["take_profit_pct"], 4.0);
    }

    #[test]
    fn defaults_never_carry_a_risk_limit() {
        for def in STRATEGIES {
            let params = defaults(def.name);
            assert!(params.get("max_position_pct").is_none());
            assert!(params.get("min_notional_usd").is_none());
        }
    }

    #[test]
    fn the_registry_names_the_directory_the_build_writes_to() {
        let dir = Path::new("target/wasm");
        let path = path_for(dir, find("momentum").unwrap());
        assert_eq!(path, Path::new("target/wasm/momentum.wasm"));
        assert_eq!(
            path.file_stem().unwrap().to_string_lossy(),
            "momentum",
            "the loaded name is the file stem the registry advertises"
        );
    }
}

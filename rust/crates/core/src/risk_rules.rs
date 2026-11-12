//! The canonical risk rules.
//!
//! The Rust risk layer is the only place the limits, the order the checks run
//! in and the reason codes are defined. The browser dashboard reads the same
//! description from `ts/packages/ui/src/lib/risk-rules.json`, which is
//! generated from this module, so the public simulator refuses exactly what
//! the engine refuses — with the same words.
//!
//! The contract has two flavours:
//!
//! * [`contract`] describes the running engine, including the allowlists it
//!   was configured with. `GET /api/risk/rules` serves it.
//! * [`browser_contract`] describes the simulator that runs without a core.
//!   Its limits are the engine's defaults and its allowlists are empty: the
//!   simulator is its own venue with its own market, so it does not inherit
//!   the engine's trading universe.

use crate::risk::RiskConfig;
use serde::{Deserialize, Serialize};

/// The version of the contract format.
pub const CONTRACT_VERSION: u32 = 1;

/// The order the risk layer applies its checks in.
///
/// A refusal is reported with the first check that failed, so the order is
/// part of the contract: the browser simulator walks the same list.
pub const CHECK_ORDER: [&str; 15] = [
    "breaker_open",
    "invalid_quantity",
    "invalid_price",
    "invalid_equity",
    "invalid_symbol",
    "symbol_not_allowed",
    "venue_not_allowed",
    "contract_not_allowed",
    "notional_too_small",
    "notional_too_large",
    "reduce_only_violation",
    "position_size_exceeds_max_pct",
    "gross_exposure_exceeded",
    "max_open_positions",
    "half_open_probe_too_large",
];

/// The reason codes the venue can produce, as opposed to the risk layer.
///
/// The paper venue refuses an order it cannot fill: selling a position that
/// was never opened, selling more than is held, or spending money the account
/// does not have. The browser simulator is a venue, so it reports these too.
pub const VENUE_REASONS: [&str; 3] = ["no_position", "reduce_only", "insufficient_cash"];

/// The reason code an approved order carries.
pub const APPROVED: &str = "approved";

/// The limits that decide how much money can be lost.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuleLimits {
    pub max_position_pct_of_equity: f64,
    pub max_gross_exposure_pct: f64,
    pub max_daily_loss_pct: f64,
    pub max_drawdown_pct: f64,
    pub max_open_positions: usize,
    pub max_order_notional_usd: f64,
    pub min_order_notional_usd: f64,
    pub breaker_cooldown_ms: i64,
    /// An order larger than the position cap halts trading instead of only
    /// refusing that one order.
    pub hard_halt_on_oversized_order: bool,
}

/// The symbols, venues and contracts the engine may touch.
///
/// An empty list means no restriction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RuleAllowlists {
    pub symbols: Vec<String>,
    pub venues: Vec<String>,
    pub contracts: Vec<String>,
}

/// The whole contract: limits, allowlists, check order and reason codes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RiskContract {
    pub version: u32,
    pub limits: RuleLimits,
    pub allowlists: RuleAllowlists,
    /// The risk-layer checks, in the order they run.
    pub check_order: Vec<String>,
    /// The checks a venue applies on top of the risk layer.
    pub venue_checks: Vec<String>,
    /// Every reason code an order can be refused with.
    pub reasons: Vec<String>,
}

impl From<&RiskConfig> for RuleLimits {
    fn from(c: &RiskConfig) -> Self {
        Self {
            max_position_pct_of_equity: c.max_position_pct_of_equity,
            max_gross_exposure_pct: c.max_gross_exposure_pct,
            max_daily_loss_pct: c.max_daily_loss_pct,
            max_drawdown_pct: c.max_drawdown_pct,
            max_open_positions: c.max_open_positions,
            max_order_notional_usd: c.max_order_notional_usd,
            min_order_notional_usd: c.min_order_notional_usd,
            breaker_cooldown_ms: c.breaker_cooldown_ms,
            hard_halt_on_oversized_order: c.hard_halt_on_oversized_order,
        }
    }
}

impl From<&RiskConfig> for RuleAllowlists {
    fn from(c: &RiskConfig) -> Self {
        Self {
            symbols: sorted(&c.symbol_allowlist),
            venues: sorted(
                &c.venue_allowlist
                    .iter()
                    .map(|v| v.as_str().to_string())
                    .collect(),
            ),
            contracts: sorted(&c.contract_allowlist),
        }
    }
}

/// Every reason code an order can be refused with.
pub fn reasons() -> Vec<String> {
    let mut all: Vec<String> = CHECK_ORDER.iter().map(|r| (*r).to_string()).collect();
    all.extend(VENUE_REASONS.iter().map(|r| (*r).to_string()));
    all.push(APPROVED.to_string());
    all
}

/// The contract of a running engine, including the allowlists it was
/// configured with.
pub fn contract(config: &RiskConfig) -> RiskContract {
    RiskContract {
        version: CONTRACT_VERSION,
        limits: RuleLimits::from(config),
        allowlists: RuleAllowlists::from(config),
        check_order: CHECK_ORDER.iter().map(|r| (*r).to_string()).collect(),
        venue_checks: VENUE_REASONS.iter().map(|r| (*r).to_string()).collect(),
        reasons: reasons(),
    }
}

/// The contract of the simulator that runs without a core.
///
/// The limits are the engine's defaults. The allowlists are empty: the
/// simulator is its own venue with its own market, so it does not inherit the
/// engine's trading universe.
pub fn browser_contract() -> RiskContract {
    let mut config = RiskConfig::default();
    config.symbol_allowlist.clear();
    config.venue_allowlist.clear();
    config.contract_allowlist.clear();
    contract(&config)
}

/// The contract as pretty JSON, with a trailing newline.
pub fn to_json_pretty(contract: &RiskContract) -> String {
    let mut text = serde_json::to_string_pretty(contract).unwrap_or_else(|_| "{}".to_string());
    text.push('\n');
    text
}

fn sorted(values: &std::collections::HashSet<String>) -> Vec<String> {
    let mut items: Vec<String> = values.iter().cloned().collect();
    items.sort_unstable();
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Venue;

    #[test]
    fn the_contract_carries_the_configured_limits() {
        let config = RiskConfig::default();
        let contract = contract(&config);
        assert_eq!(contract.limits.max_position_pct_of_equity, 20.0);
        assert_eq!(contract.limits.max_gross_exposure_pct, 60.0);
        assert_eq!(contract.limits.max_daily_loss_pct, 3.0);
        assert_eq!(contract.limits.max_drawdown_pct, 10.0);
        assert_eq!(contract.limits.max_open_positions, 5);
        assert_eq!(contract.limits.max_order_notional_usd, 5000.0);
        assert_eq!(contract.limits.min_order_notional_usd, 10.0);
        assert!(contract.limits.hard_halt_on_oversized_order);
    }

    #[test]
    fn the_contract_carries_the_configured_allowlists() {
        let config = RiskConfig::default();
        let contract = contract(&config);
        assert_eq!(contract.allowlists.symbols, vec!["SOL/USDT".to_string()]);
        assert_eq!(contract.allowlists.venues, vec!["paper".to_string()]);
        assert!(contract.allowlists.contracts.is_empty());
    }

    #[test]
    fn the_browser_contract_has_no_allowlists() {
        let contract = browser_contract();
        assert!(contract.allowlists.symbols.is_empty());
        assert!(contract.allowlists.venues.is_empty());
        assert!(contract.allowlists.contracts.is_empty());
        assert_eq!(contract.limits, RuleLimits::from(&RiskConfig::default()));
    }

    #[test]
    fn every_check_has_a_reason_and_a_reason_is_a_check_or_a_venue_reason() {
        let contract = browser_contract();
        for check in &contract.check_order {
            assert!(
                contract.reasons.contains(check),
                "{check} is not in the reason vocabulary"
            );
        }
        for reason in &contract.reasons {
            assert!(
                contract.check_order.contains(reason)
                    || VENUE_REASONS.contains(&reason.as_str())
                    || reason == APPROVED,
                "{reason} is neither a check nor a venue reason"
            );
        }
        for venue_check in &contract.venue_checks {
            assert!(
                contract.reasons.contains(venue_check),
                "{venue_check} is not in the reason vocabulary"
            );
            assert!(
                !contract.check_order.contains(venue_check),
                "{venue_check} is both a risk check and a venue check"
            );
        }
    }

    #[test]
    fn the_allowlists_are_sorted_so_the_contract_is_stable() {
        let config = RiskConfig {
            symbol_allowlist: ["B/USDT".to_string(), "A/USDT".to_string()]
                .into_iter()
                .collect(),
            venue_allowlist: [Venue::Kraken, Venue::Paper].into_iter().collect(),
            ..Default::default()
        };
        let contract = contract(&config);
        assert_eq!(contract.allowlists.symbols, vec!["A/USDT", "B/USDT"]);
        assert_eq!(contract.allowlists.venues, vec!["kraken", "paper"]);
    }

    /// The dashboard's rules file is generated from this module. If the two
    /// drift apart, the public simulator would refuse different orders than
    /// the engine, so the build fails instead.
    #[test]
    fn the_dashboard_rules_file_is_in_sync() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../ts/packages/ui/src/lib/risk-rules.json");
        let expected = to_json_pretty(&browser_contract());
        let actual = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!("cannot read {}: {e}", path.display());
        });
        assert_eq!(
            actual,
            expected,
            "{} is out of date — regenerate it with `cargo run -p openether-core --bin openether-rules`",
            path.display()
        );
    }
}

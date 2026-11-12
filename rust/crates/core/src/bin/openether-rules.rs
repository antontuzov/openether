//! Emits the canonical risk contract as JSON.
//!
//! The dashboard's rules file is generated from the Rust risk layer, so the
//! browser simulator and the engine cannot drift apart. Regenerate it with:
//!
//! ```text
//! cargo run -p openether-core --bin openether-rules \
//!   > ../../ts/packages/ui/src/lib/risk-rules.json
//! ```
//!
//! A test in `risk_rules.rs` fails the build if the file is out of date.

use openether_core::risk_rules;

fn main() {
    let contract = risk_rules::browser_contract();
    print!("{}", risk_rules::to_json_pretty(&contract));
}

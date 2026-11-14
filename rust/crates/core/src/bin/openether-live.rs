//! `openether-live` — the live trading arming ceremony.
//!
//! Arming live trading is deliberately not a configuration value. The
//! environment variable `OPENETHER_LIVE_TRADING=1` only *asks* for live
//! trading; it arms nothing. The proof is a record on disk that this binary
//! writes after the operator has confirmed what they are about to do.
//!
//! ```text
//! openether-live arm    --venue binance --max-notional 500 --hours 4
//! openether-live arm    --target onchain --max-notional 50 --hours 1
//! openether-live status
//! openether-live disarm
//! ```
//!
//! `--target onchain` arms the wallet: a transfer out of it is a different act
//! from placing an order, so it needs its own record.
//!
//! The record is bound to the effective risk configuration, so changing a
//! limit after arming invalidates the arming. It expires. And the notional
//! cap it carries may only *tighten* the cap of the risk layer, never raise
//! it.

use openether_core::config::Config;
use openether_core::live_arm::{
    arm, arm_onchain, arming_path, disarm, evaluate, evaluate_onchain, live_requested, LiveArming,
    CONFIRMATION_PHRASE, ONCHAIN_TARGET,
};
use openether_core::types::Venue;
use std::process::ExitCode;

const USAGE: &str = "\
openether-live — arm live trading

usage:
  openether-live arm    --venue <binance|kraken> [--max-notional <usd>] [--hours <n>] [--note <text>]
  openether-live arm    --target onchain [--max-notional <usd>] [--hours <n>] [--note <text>]
  openether-live status
  openether-live disarm

The arming record lives at the path in OPENETHER_LIVE_ARMING_FILE.
Live trading also needs OPENETHER_LIVE_TRADING=1 in the core's environment.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let Some(command) = args.first() else {
        print!("{USAGE}");
        return Err("a command is required".into());
    };
    if matches!(command.as_str(), "-h" | "--help" | "help") {
        print!("{USAGE}");
        return Ok(());
    }

    let options = parse_options(&args[1..])?;
    match command.as_str() {
        "arm" => arm_live(&options),
        "status" => status(),
        "disarm" => disarm_live(),
        other => {
            print!("{USAGE}");
            Err(format!("unknown command: {other}"))
        }
    }
}

/// Writes the arming record.
fn arm_live(options: &Options) -> Result<(), String> {
    let path = arming_path().ok_or_else(|| {
        "OPENETHER_LIVE_ARMING_FILE is not set: the core needs to know where the arming record lives"
            .to_string()
    })?;
    let config = Config::from_env();

    // `--venue onchain` and `--target onchain` mean the same thing: nothing is
    // being armed on an exchange, the wallet is.
    let onchain = ["target", "venue"].iter().any(|key| {
        options
            .get(key)
            .map(|value| value.trim().eq_ignore_ascii_case(ONCHAIN_TARGET))
            .unwrap_or(false)
    });
    let venue = if onchain {
        None
    } else {
        let venue = match options.get("venue") {
            Some(value) => Venue::parse(&value).ok_or_else(|| format!("unknown venue: {value}"))?,
            None => config.venue,
        };
        if !venue.trades_live() {
            return Err(format!(
                "the {venue} venue cannot place real orders in this build"
            ));
        }
        Some(venue)
    };

    let max_notional = match options.get("max-notional") {
        Some(value) => value
            .trim()
            .parse::<f64>()
            .map_err(|_| format!("the notional cap is not a number: {value}"))?,
        None => config.risk.max_order_notional_usd,
    };
    let hours: i64 = match options.get("hours") {
        Some(value) => value
            .trim()
            .parse()
            .map_err(|_| format!("the arming window is not a number: {value}"))?,
        None => 4,
    };
    let note = options.get("note").unwrap_or_default();

    if onchain {
        println!("openEther live wallet arming");
    } else {
        println!("openEther live trading arming");
    }
    println!();
    match venue {
        Some(venue) => println!("  venue          {venue}"),
        None => println!("  target         the wallet (on-chain transfers)"),
    }
    if !onchain {
        println!("  symbols        {}", {
            let mut symbols: Vec<&str> = config
                .risk
                .symbol_allowlist
                .iter()
                .map(|s| s.as_str())
                .collect();
            symbols.sort_unstable();
            symbols.join(", ")
        });
    }
    println!(
        "  position cap   {:.1}% of equity",
        config.risk.max_position_pct_of_equity
    );
    println!(
        "  gross cap      {:.1}% of equity",
        config.risk.max_gross_exposure_pct
    );
    println!("  daily loss     {:.1}%", config.risk.max_daily_loss_pct);
    println!("  drawdown       {:.1}%", config.risk.max_drawdown_pct);
    println!(
        "  order cap      ${max_notional:.2} (the risk layer allows ${:.2})",
        config.risk.max_order_notional_usd
    );
    println!("  valid for      {hours} hour(s)");
    println!();
    match venue {
        Some(venue) => {
            println!("Live trading sends real orders to {venue}. There is no undo.");
        }
        None => {
            println!("Live transfers move real coins out of the wallet. There is no undo.");
            println!("The cap is the dollar value of one transfer, priced from the feed.");
        }
    }
    println!();
    print!("Type the confirmation phrase to arm it: ");
    let confirmation = read_line()?;
    if confirmation.trim() != CONFIRMATION_PHRASE {
        return Err("the confirmation phrase did not match; nothing was armed".into());
    }

    let record = match venue {
        Some(venue) => arm(
            &path,
            venue,
            &config.risk,
            max_notional,
            hours * 60 * 60 * 1000,
            CONFIRMATION_PHRASE,
            &note,
        ),
        None => arm_onchain(
            &path,
            &config.risk,
            max_notional,
            hours * 60 * 60 * 1000,
            CONFIRMATION_PHRASE,
            &note,
        ),
    }
    .map_err(|e| e.to_string())?;

    println!();
    println!(
        "{} is armed until {} (unix ms).",
        if onchain {
            "Live wallet transfers"
        } else {
            "Live trading"
        },
        record.expires_at_ms
    );
    println!("The record was written to {}", path.display());
    println!();
    println!("The core also needs OPENETHER_LIVE_TRADING=1. Without it, live");
    println!("trading and transfers stay off even with a valid arming record.");
    Ok(())
}

/// Prints the arming state the core would see.
fn status() -> Result<(), String> {
    let config = Config::from_env();
    let state = evaluate(config.venue, &config.risk);
    let wallet = evaluate_onchain(&config.risk);
    println!(
        "{}",
        serde_json::to_string_pretty(&state).map_err(|e| e.to_string())?
    );
    println!();
    println!("venue:      {}", config.venue);
    println!(
        "requested:  {}",
        if live_requested() { "yes" } else { "no" }
    );
    match arming_path() {
        Some(path) => println!("record:     {}", path.display()),
        None => println!("record:     (OPENETHER_LIVE_ARMING_FILE is not set)"),
    }
    if let Some(record) = &state.record {
        print_record(record);
    }
    println!();
    println!(
        "wallet:     {}",
        if wallet.live_enabled {
            "armed for on-chain transfers".to_string()
        } else {
            wallet
                .detail
                .clone()
                .unwrap_or_else(|| "not armed".to_string())
        }
    );
    Ok(())
}

fn print_record(record: &LiveArming) {
    println!();
    println!("armed at:   {}", record.armed_at_ms);
    println!("expires at: {}", record.expires_at_ms);
    println!("cap:        ${:.2}", record.max_order_notional_usd);
    if !record.operator_note.is_empty() {
        println!("note:       {}", record.operator_note);
    }
}

/// Removes the arming record.
fn disarm_live() -> Result<(), String> {
    let Some(path) = arming_path() else {
        return Err("OPENETHER_LIVE_ARMING_FILE is not set".into());
    };
    disarm(&path).map_err(|e| e.to_string())?;
    println!("The arming record {} was removed.", path.display());
    Ok(())
}

/// Reads one line from stdin.
fn read_line() -> Result<String, String> {
    let mut buffer = String::new();
    std::io::stdin()
        .read_line(&mut buffer)
        .map_err(|e| format!("could not read the confirmation: {e}"))?;
    Ok(buffer)
}

/// The parsed `--key value` options of one command.
struct Options {
    values: Vec<(String, String)>,
}

impl Options {
    fn get(&self, key: &str) -> Option<String> {
        self.values
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut values = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let Some(name) = arg.strip_prefix("--") else {
            return Err(format!("unexpected argument: {arg}"));
        };
        if let Some((key, value)) = name.split_once('=') {
            values.push((key.to_string(), value.to_string()));
            index += 1;
            continue;
        }
        let Some(value) = args.get(index + 1) else {
            return Err(format!("--{name} needs a value"));
        };
        values.push((name.to_string(), value.clone()));
        index += 2;
    }
    Ok(Options { values })
}

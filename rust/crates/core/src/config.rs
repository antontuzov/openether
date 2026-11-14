use crate::live_arm::LiveArmingState;
use crate::risk::RiskConfig;
use crate::types::{Chain, Venue};
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub venue: Venue,
    pub symbols: Vec<String>,
    pub live_trading: bool,
    /// Why live trading is on or off. The core refuses to trade live unless
    /// the arming ceremony succeeded.
    pub live_arming: LiveArmingState,
    /// Why on-chain transfers are on or off. The wallet has its own arming
    /// record: arming a venue does not arm the wallet.
    pub onchain_arming: LiveArmingState,
    /// `true` only when the on-chain arming record is valid *and* live
    /// operation was requested.
    pub onchain_live: bool,
    pub risk: RiskConfig,
    /// Starting equity of the paper venue, used as the PnL baseline.
    pub paper_start_equity: f64,
    pub strategy_wasm: Option<PathBuf>,
    pub wasm_fuel: u64,
    pub wasm_memory_pages: u32,
    pub contract_allowlist: HashSet<String>,
    pub solana_rpc_url: Option<String>,
    pub evm_rpc_urls: Vec<(String, String)>,
    /// The most this process will ever pay per EVM gas unit, in wei.
    ///
    /// A fee spike is a reason to refuse a transfer, not to overpay it. The
    /// broadcaster compares this ceiling against both the network's fee and
    /// any smaller per-request ceiling.
    pub broadcast_max_fee_per_gas_wei: u128,
    /// A DEX quote source. The template must name `{sell_token}`, `{buy_token}`
    /// and `{sell_amount}`; see `swap::HttpQuoter`. `None` disables swaps.
    pub swap_quote_url: Option<String>,
    /// Where the engine keeps its durable state. `None` keeps everything in
    /// memory, which is only sensible for a throwaway run: a restart would
    /// reset equity to zero and silently re-open every risk limit.
    pub state_db_path: Option<PathBuf>,
    pub wallet: WalletConfig,
}

/// Wallet configuration.
///
/// The core never stores a private key in clear text. The keystore is an
/// encrypted file; it is only unlocked for the duration of a signature and the
/// unlocked key is wiped again when the lease expires.
#[derive(Debug, Clone)]
pub struct WalletConfig {
    /// Path to the encrypted keystore. `None` disables the wallet entirely.
    pub keystore_path: Option<PathBuf>,
    /// Chain the keystore is expected to hold. `None` means "whatever the
    /// keystore says".
    pub chain: Option<Chain>,
    /// When `false` the core refuses to sign, because live broadcast is not
    /// implemented in this build. Only dry-run signing exists.
    pub dry_run: bool,
    /// How long an unlocked key may stay in memory before it is re-locked.
    pub unlock_timeout_ms: i64,
    /// How many accounts per chain an HD keystore holds.
    pub accounts: u32,
    /// ERC-20 contracts to value on every configured EVM chain.
    ///
    /// ERC-20 balances cannot be enumerated over standard JSON-RPC, so the
    /// list of contracts has to be configured.
    pub evm_token_watchlist: Vec<String>,
    /// The free price feed used to value the wallet's holdings.
    pub price_feed_url: Option<String>,
}

impl Default for WalletConfig {
    fn default() -> Self {
        Self {
            keystore_path: None,
            chain: None,
            dry_run: true,
            unlock_timeout_ms: 300_000,
            accounts: 5,
            evm_token_watchlist: Vec::new(),
            price_feed_url: None,
        }
    }
}

impl Config {
    pub fn from_env() -> Self {
        let venue = std::env::var("OPENETHER_VENUE")
            .ok()
            .and_then(|v| Venue::parse(&v))
            .unwrap_or(Venue::Paper);

        let symbols = env_list("OPENETHER_SYMBOLS", &["SOL/USDT", "BTC/USDT", "ETH/USDT"]);
        let allowlist = env_list(
            "OPENETHER_SYMBOL_ALLOWLIST",
            &["SOL/USDT", "BTC/USDT", "ETH/USDT"],
        );
        let venue_allowlist = env_list("OPENETHER_VENUE_ALLOWLIST", &["paper"])
            .iter()
            .filter_map(|v| Venue::parse(v))
            .collect();

        let mut risk = RiskConfig {
            max_position_pct_of_equity: env_f64("OPENETHER_MAX_POSITION_PCT", 20.0),
            max_gross_exposure_pct: env_f64("OPENETHER_MAX_GROSS_EXPOSURE_PCT", 60.0),
            max_daily_loss_pct: env_f64("OPENETHER_MAX_DAILY_LOSS_PCT", 3.0),
            max_drawdown_pct: env_f64("OPENETHER_MAX_DRAWDOWN_PCT", 10.0),
            max_open_positions: env_usize("OPENETHER_MAX_OPEN_POSITIONS", 5),
            max_order_notional_usd: env_f64("OPENETHER_MAX_ORDER_NOTIONAL_USD", 5000.0),
            min_order_notional_usd: env_f64("OPENETHER_MIN_ORDER_NOTIONAL_USD", 10.0),
            symbol_allowlist: allowlist.into_iter().collect(),
            venue_allowlist,
            contract_allowlist: env_list("OPENETHER_CONTRACT_ALLOWLIST", &[])
                .into_iter()
                .collect(),
            breaker_cooldown_ms: env_i64("OPENETHER_BREAKER_COOLDOWN_MS", 300_000),
            hard_halt_on_oversized_order: env_flag_or(
                "OPENETHER_BREAKER_HALT_ON_OVERSIZED_ORDER",
                true,
            ),
        };

        // The arming record decides whether live trading may run. The
        // environment variable only asks for it; the record is the proof.
        let live_arming = crate::live_arm::evaluate(venue, &risk);
        if live_arming.live_enabled {
            // An arming may only tighten the per-order cap, never raise it.
            risk.max_order_notional_usd = live_arming.effective_max_order_notional_usd;
        }
        let live_trading = live_arming.live_enabled;
        // Separate record, separate decision. The wallet's cap is measured in
        // dollars of the coin being moved, priced at broadcast time.
        let onchain_arming = crate::live_arm::evaluate_onchain(&risk);
        let onchain_live = onchain_arming.live_enabled;

        Self {
            host: std::env::var("CORE_HOST").unwrap_or_else(|_| "0.0.0.0".into()),
            port: env_u16("CORE_PORT", 8080),
            venue,
            symbols,
            live_trading,
            live_arming,
            onchain_arming,
            onchain_live,
            risk,
            paper_start_equity: env_f64("OPENETHER_PAPER_CASH", 10_000.0),
            strategy_wasm: std::env::var("OPENETHER_STRATEGY_WASM")
                .ok()
                .map(PathBuf::from),
            wasm_fuel: env_u64("OPENETHER_WASM_FUEL", 200_000_000),
            wasm_memory_pages: env_u64("OPENETHER_WASM_MEMORY_PAGES", 32) as u32,
            contract_allowlist: env_list("OPENETHER_CONTRACT_ALLOWLIST", &[])
                .into_iter()
                .collect(),
            solana_rpc_url: std::env::var("SOLANA_RPC_URL")
                .ok()
                .filter(|v| !v.is_empty()),
            broadcast_max_fee_per_gas_wei: env_u128(
                "OPENETHER_BROADCAST_MAX_FEE_PER_GAS_WEI",
                200_000_000_000, // 200 gwei
            ),
            swap_quote_url: std::env::var("OPENETHER_SWAP_QUOTE_URL")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
            evm_rpc_urls: [("ethereum", "ETHEREUM_RPC_URL"), ("base", "BASE_RPC_URL")]
                .iter()
                .filter_map(|(name, key)| {
                    std::env::var(key)
                        .ok()
                        .filter(|v| !v.is_empty())
                        .map(|url| ((*name).to_string(), url))
                })
                .collect(),
            state_db_path: std::env::var("OPENETHER_STATE_DB")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty() && !v.eq_ignore_ascii_case("memory"))
                .map(PathBuf::from),
            wallet: WalletConfig {
                keystore_path: std::env::var("OPENETHER_WALLET_KEYSTORE")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                    .map(PathBuf::from),
                chain: std::env::var("OPENETHER_WALLET_CHAIN")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                    .and_then(|v| crate::onchain::parse_chain(&v)),
                dry_run: env_flag_or("OPENETHER_WALLET_DRY_RUN", true),
                unlock_timeout_ms: env_i64("OPENETHER_WALLET_UNLOCK_TIMEOUT_MS", 300_000),
                accounts: env_u32("OPENETHER_WALLET_ACCOUNTS", 5),
                evm_token_watchlist: env_list("OPENETHER_EVM_TOKEN_WATCHLIST", &[])
                    .into_iter()
                    .collect(),
                price_feed_url: std::env::var("OPENETHER_PRICE_FEED_URL")
                    .ok()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty()),
            },
        }
    }

    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

fn env_flag(key: &str) -> bool {
    matches!(
        std::env::var(key)
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn env_flag_or(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => env_flag(key),
        _ => default,
    }
}

fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .unwrap_or(default)
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
}

fn env_u128(key: &str, default: u128) -> u128 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u128>().ok())
        .unwrap_or(default)
}

fn env_usize(key: &str, default: usize) -> usize {
    env_u64(key, default as u64) as usize
}

fn env_u16(key: &str, default: u16) -> u16 {
    env_u64(key, default as u64) as u16
}

fn env_i64(key: &str, default: i64) -> i64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(default)
}

fn env_u32(key: &str, default: u32) -> u32 {
    env_u64(key, default as u64) as u32
}

fn env_list(key: &str, default: &[&str]) -> Vec<String> {
    let raw = std::env::var(key).unwrap_or_default();
    let source: Vec<String> = if raw.trim().is_empty() {
        default.iter().map(|s| s.to_string()).collect()
    } else {
        raw.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };
    source
}

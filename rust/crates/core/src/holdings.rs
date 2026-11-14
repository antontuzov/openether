//! What the wallet holds, in dollars.
//!
//! The engine can read a real multi-chain wallet — the one whose keys live in
//! the encrypted keystore — and value what is in it: the native coin of each
//! chain plus the ERC-20 and SPL tokens. Balances come from the public RPC
//! endpoints, prices from a free public feed.
//!
//! This is a read. Nothing here signs, nothing here broadcasts, and no key is
//! ever unlocked: the balances are public information and the addresses come
//! from the keystore's public metadata.

use crate::onchain::{OnchainClient, OnchainResult, TokenBalance};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// The default price feed. It needs no key and no account.
pub const DEFAULT_PRICE_FEED: &str = "https://api.dexscreener.com";

/// How many addresses the feed accepts in one request.
const PRICE_BATCH: usize = 30;

/// How many tokens one chain read values.
///
/// A busy address can hold thousands of dust tokens. Pricing every one of them
/// would exhaust the free feed's rate limit long before the next refresh, so
/// the read is bounded and the number left out is reported rather than hidden.
const MAX_VALUED_TOKENS: usize = 300;

/// The wrapped native coin of a chain, used to price the gas token itself.
const WRAPPED_NATIVE: &[(&str, &str, &str)] = &[
    (
        "ethereum",
        "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
        "ETH",
    ),
    ("base", "0x4200000000000000000000000000000000000006", "ETH"),
    (
        "solana",
        "So11111111111111111111111111111111111111112",
        "SOL",
    ),
];

/// The address of a chain's wrapped native coin, if the chain has one.
pub fn wrapped_native_address(chain: &str) -> Option<&'static str> {
    WRAPPED_NATIVE
        .iter()
        .find(|(name, _, _)| name.eq_ignore_ascii_case(chain))
        .map(|(_, address, _)| *address)
}

/// A token the wallet holds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenHolding {
    /// The chain the token lives on.
    pub chain: String,
    /// The contract address (EVM) or the mint (Solana).
    pub asset: String,
    /// The token's own symbol.
    pub symbol: String,
    /// How many tokens are held.
    pub balance: f64,
    /// The token's decimals.
    pub decimals: u32,
    /// The price of one token in dollars.
    pub price_usd: f64,
    /// What the holding is worth.
    pub value_usd: f64,
    /// `false` when no price source had this token. The value is then
    /// unknown, not zero, and the holding is reported as unpriced.
    pub priced: bool,
}

/// Everything one chain's address holds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainHoldings {
    pub chain: String,
    pub address: String,
    /// The symbol of the chain's gas token.
    pub native_symbol: String,
    /// How much of the gas token is held.
    pub native_balance: f64,
    /// The price of the gas token in dollars.
    pub native_price_usd: f64,
    /// What the gas token is worth.
    pub native_value_usd: f64,
    /// The tokens in the account, minus the empty ones.
    pub tokens: Vec<TokenHolding>,
    /// How many held tokens were left out of the valuation because the read
    /// is bounded. Zero for an ordinary wallet.
    pub tokens_skipped: usize,
    /// The total value of everything on this chain.
    pub value_usd: f64,
    /// What this address has spent on gas, in dollars, at the current price
    /// of the chain's gas token. Reported separately: gas is a cost the
    /// wallet paid, not part of the balance being valued.
    #[serde(default)]
    pub gas_spent_usd: f64,
    pub fetched_at_ms: i64,
}

/// The wallet of the engine, valued across every chain it holds keys for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletHoldings {
    /// `true` when a keystore is configured and could be read.
    pub configured: bool,
    /// One entry per chain that was read.
    pub chains: Vec<ChainHoldings>,
    /// The total value of every chain.
    pub value_usd: f64,
    pub fetched_at_ms: i64,
}

/// The price of one token.
#[derive(Debug, Clone, PartialEq)]
pub struct TokenPrice {
    pub symbol: String,
    pub price_usd: f64,
    /// The liquidity of the pair the price came from. A thin pair is a price
    /// nobody should trade against.
    pub liquidity_usd: f64,
}

/// A free price source, keyed by token address.
///
/// DexScreener quotes every token that has a pool, on every chain, without a
/// key. The token has to be the *base* of a pair: a pair where it is the
/// quote token prices the other side.
pub struct PriceFeed {
    client: reqwest::Client,
    base_url: String,
}

impl PriceFeed {
    pub fn new(base_url: Option<String>) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()?;
        Ok(Self {
            client,
            base_url: base_url
                .filter(|url| !url.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_PRICE_FEED.to_string()),
        })
    }

    /// The price of a token, by its chain and address.
    pub async fn token(&self, chain: &str, address: &str) -> Option<TokenPrice> {
        self.tokens(chain, &[address.to_string()])
            .await
            .remove(address)
    }

    /// The prices of many tokens at once.
    ///
    /// The feed takes a comma separated list, so a wallet with thirty tokens
    /// costs one request instead of thirty. Tokens with no pool are simply
    /// absent from the answer.
    pub async fn tokens(&self, chain: &str, addresses: &[String]) -> HashMap<String, TokenPrice> {
        let mut prices = HashMap::new();
        for batch in addresses.chunks(PRICE_BATCH) {
            let url = format!(
                "{}/tokens/v1/{}/{}",
                self.base_url.trim_end_matches('/'),
                chain,
                batch.join(",")
            );
            let Ok(response) = self.client.get(url).send().await else {
                continue;
            };
            let Ok(body) = response.json::<serde_json::Value>().await else {
                continue;
            };
            let Some(pairs) = body.as_array() else {
                continue;
            };
            for address in batch {
                if let Some(price) = best_price(pairs, address) {
                    prices.insert(address.clone(), price);
                }
            }
        }
        prices
    }

    /// The price of a chain's gas token.
    pub async fn native(&self, chain: &str) -> Option<TokenPrice> {
        let (_, address, symbol) = WRAPPED_NATIVE
            .iter()
            .find(|(name, _, _)| name.eq_ignore_ascii_case(chain))?;
        let mut price = self.token(chain, address).await?;
        // The wrapped coin is quoted under its wrapper name; the gas token is
        // not a wrapper.
        if price.symbol.starts_with('W') && price.symbol.len() > 1 {
            price.symbol = symbol.to_string();
        }
        Some(price)
    }
}

/// Reads a number that the feed may send either as a JSON number or as a
/// numeric string. DexScreener quotes `priceUsd` as a string and `liquidity.usd`
/// as a number, so accepting only one of the two silently unprices the wallet.
fn number(value: Option<&serde_json::Value>) -> Option<f64> {
    match value? {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// Picks the quote for a token out of a list of pairs.
///
/// Only pairs where the token is the *base* count: a pair where it is the quote
/// token prices the other side. Of those, the deepest pool is the one worth
/// quoting — a thin pool is a price nobody should trade against.
fn best_price(pairs: &[serde_json::Value], address: &str) -> Option<TokenPrice> {
    let mut best: Option<TokenPrice> = None;
    for pair in pairs {
        let Some(base) = pair.get("baseToken") else {
            continue;
        };
        let base_address = base
            .get("address")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if !base_address.eq_ignore_ascii_case(address) {
            continue;
        }
        let Some(price_usd) = number(pair.get("priceUsd")) else {
            continue;
        };
        if !price_usd.is_finite() || price_usd <= 0.0 {
            continue;
        }
        let liquidity_usd = pair
            .get("liquidity")
            .and_then(|l| number(l.get("usd")))
            .unwrap_or(0.0);
        let symbol = base
            .get("symbol")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if best
            .as_ref()
            .is_none_or(|current| liquidity_usd > current.liquidity_usd)
        {
            best = Some(TokenPrice {
                symbol,
                price_usd,
                liquidity_usd,
            });
        }
    }
    best
}

/// Reads and values the wallet.
pub struct HoldingsService {
    onchain: Arc<OnchainClient>,
    prices: PriceFeed,
}

impl HoldingsService {
    pub fn new(
        onchain: Arc<OnchainClient>,
        price_feed_url: Option<String>,
    ) -> Result<Self, reqwest::Error> {
        Ok(Self {
            onchain,
            prices: PriceFeed::new(price_feed_url)?,
        })
    }

    /// The price of a chain's gas token, or `None` when no feed has it.
    ///
    /// A transfer is sized in dollars of the coin being moved, so the price
    /// is a precondition, not a nicety: without one the core refuses.
    pub async fn native_price_usd(&self, chain: &str) -> Option<f64> {
        self.prices
            .native(chain)
            .await
            .map(|price| price.price_usd)
            .filter(|price| price.is_finite() && *price > 0.0)
    }

    /// The dollar price of one ERC-20 token, by its contract address.
    pub async fn token_price_usd(&self, chain: &str, address: &str) -> Option<f64> {
        self.prices
            .token(chain, address)
            .await
            .map(|price| price.price_usd)
            .filter(|price| price.is_finite() && *price > 0.0)
    }

    /// Everything a Solana address holds, valued.
    ///
    /// The SPL token list comes from the chain itself, so every token the
    /// account holds is counted, not only the ones an operator listed.
    pub async fn solana(&self, address: &str) -> OnchainResult<ChainHoldings> {
        let account = self.onchain.solana_account(address).await?;
        let native_price = self.prices.native("solana").await;
        let held: Vec<TokenBalance> = account
            .tokens
            .into_iter()
            .filter(|token| token.balance > 0.0)
            .collect();
        let (tokens, tokens_skipped) = self.value_tokens("solana", held).await;
        Ok(chain_holdings(
            "solana",
            address,
            &account.native_symbol,
            account.native_balance,
            native_price,
            tokens,
            tokens_skipped,
            account.fetched_at_ms,
        ))
    }

    /// Everything an EVM address holds, valued.
    ///
    /// ERC-20 balances cannot be enumerated over standard JSON-RPC, so the
    /// tokens to look for come from the operator's watch list. The native
    /// balance is always included.
    pub async fn evm(
        &self,
        chain: &str,
        address: &str,
        watch: &[String],
    ) -> OnchainResult<ChainHoldings> {
        let account = self.onchain.evm_balance(chain, address).await?;
        let native_price = self.prices.native(chain).await;

        let mut held = Vec::new();
        for contract in watch {
            let Ok(balance) = self
                .onchain
                .evm_token_balance(chain, contract, address)
                .await
            else {
                continue;
            };
            if balance.balance > 0.0 {
                held.push(balance);
            }
        }
        let (tokens, tokens_skipped) = self.value_tokens(chain, held).await;
        Ok(chain_holdings(
            chain,
            address,
            &account.native_symbol,
            account.native_balance,
            native_price,
            tokens,
            tokens_skipped,
            account.fetched_at_ms,
        ))
    }

    /// Prices a set of held tokens in as few requests as the feed allows.
    async fn value_tokens(
        &self,
        chain: &str,
        held: Vec<TokenBalance>,
    ) -> (Vec<TokenHolding>, usize) {
        let tokens_skipped = held.len().saturating_sub(MAX_VALUED_TOKENS);
        let held = &held[..held.len().min(MAX_VALUED_TOKENS)];
        let addresses: Vec<String> = held.iter().map(|token| token.asset.clone()).collect();
        let prices = self.prices.tokens(chain, &addresses).await;

        let tokens = held
            .iter()
            .map(|token| {
                let price = prices.get(&token.asset);
                let price_usd = price.map(|p| p.price_usd).unwrap_or(0.0);
                let symbol = token
                    .symbol
                    .clone()
                    .filter(|symbol| !symbol.is_empty())
                    .or_else(|| price.map(|p| p.symbol.clone()))
                    .unwrap_or_else(|| short_address(&token.asset));
                TokenHolding {
                    chain: chain.to_string(),
                    asset: token.asset.clone(),
                    symbol,
                    balance: token.balance,
                    decimals: token.decimals,
                    price_usd,
                    value_usd: token.balance * price_usd,
                    priced: price.is_some(),
                }
            })
            .collect();
        (tokens, tokens_skipped)
    }
}

/// Assembles one chain's reading.
#[allow(clippy::too_many_arguments)]
fn chain_holdings(
    chain: &str,
    address: &str,
    native_symbol: &str,
    native_balance: f64,
    native_price: Option<TokenPrice>,
    tokens: Vec<TokenHolding>,
    tokens_skipped: usize,
    fetched_at_ms: i64,
) -> ChainHoldings {
    let native_price_usd = native_price.as_ref().map(|p| p.price_usd).unwrap_or(0.0);
    let native_value_usd = native_balance * native_price_usd;
    let value_usd = native_value_usd + tokens.iter().map(|token| token.value_usd).sum::<f64>();
    ChainHoldings {
        chain: chain.to_string(),
        address: address.to_string(),
        native_symbol: native_symbol.to_string(),
        native_balance,
        native_price_usd,
        native_value_usd,
        tokens,
        tokens_skipped,
        value_usd,
        gas_spent_usd: 0.0,
        fetched_at_ms,
    }
}

/// A short, readable form of a token address.
fn short_address(address: &str) -> String {
    let trimmed = address.trim();
    if trimmed.len() <= 10 {
        return trimmed.to_string();
    }
    format!("{}…{}", &trimmed[..6], &trimmed[trimmed.len() - 4..])
}

/// The chains the engine can value, with the addresses to look at.
///
/// The addresses come from the keystore's public metadata, so the engine reads
/// the wallet it is configured with without ever unlocking it.
///
/// Only the primary account of each chain is valued: every extra account is
/// another full set of RPC reads, and the primary account is the one the
/// wallet itself advertises.
pub fn watched_accounts(metadata: &crate::wallet::WalletMetadata) -> Vec<(String, String)> {
    let mut accounts = Vec::new();
    for chain in ["solana", "evm"] {
        let Some(address) = metadata
            .accounts
            .iter()
            .filter(|account| account.chain.eq_ignore_ascii_case(chain))
            .map(|account| account.address.clone())
            .next()
        else {
            continue;
        };
        accounts.push((chain.to_string(), address));
    }
    accounts
}

/// Turns the per-chain readings into one wallet.
pub fn wallet_of(chains: Vec<ChainHoldings>, configured: bool) -> WalletHoldings {
    // `sum` on an empty iterator yields -0.0, which would serialise as a
    // negative zero balance.
    let value_usd = chains
        .iter()
        .fold(0.0, |total, chain| total + chain.value_usd);
    WalletHoldings {
        configured,
        chains,
        value_usd,
        fetched_at_ms: crate::types::now_ms(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_address_stays_readable() {
        assert_eq!(
            short_address("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
            "0xA0b8…eB48"
        );
        assert_eq!(short_address("abc"), "abc");
    }

    #[test]
    fn an_unpriced_token_is_not_valued() {
        let holding = TokenHolding {
            chain: "solana".into(),
            asset: "mint".into(),
            symbol: "WHAT".into(),
            balance: 10.0,
            decimals: 6,
            price_usd: 0.0,
            value_usd: 0.0,
            priced: false,
        };
        assert!(!holding.priced);
        assert_eq!(holding.value_usd, 0.0);
    }

    #[test]
    fn the_wallet_totals_every_chain() {
        let wallet = wallet_of(
            vec![
                ChainHoldings {
                    chain: "solana".into(),
                    address: "addr".into(),
                    native_symbol: "SOL".into(),
                    native_balance: 1.0,
                    native_price_usd: 100.0,
                    native_value_usd: 100.0,
                    tokens: vec![],
                    tokens_skipped: 0,
                    value_usd: 100.0,
                    gas_spent_usd: 0.0,
                    fetched_at_ms: 0,
                },
                ChainHoldings {
                    chain: "ethereum".into(),
                    address: "addr".into(),
                    native_symbol: "ETH".into(),
                    native_balance: 0.5,
                    native_price_usd: 2_000.0,
                    native_value_usd: 1_000.0,
                    tokens: vec![],
                    tokens_skipped: 0,
                    value_usd: 1_000.0,
                    gas_spent_usd: 0.0,
                    fetched_at_ms: 0,
                },
            ],
            true,
        );
        assert_eq!(wallet.value_usd, 1_100.0);
        assert_eq!(wallet.chains.len(), 2);
        assert!(wallet.configured);
    }

    #[test]
    fn an_unconfigured_wallet_is_empty() {
        let wallet = wallet_of(Vec::new(), false);
        assert!(!wallet.configured);
        assert_eq!(wallet.value_usd, 0.0);
        assert!(wallet.value_usd.is_sign_positive(), "not a negative zero");
        assert!(wallet.chains.is_empty());
    }

    fn pair(base: &str, symbol: &str, price: f64, liquidity: f64) -> serde_json::Value {
        serde_json::json!({
            "baseToken": {"address": base, "symbol": symbol},
            "priceUsd": price,
            "liquidity": {"usd": liquidity},
        })
    }

    #[test]
    fn the_deepest_pool_of_the_token_is_quoted() {
        let pairs = vec![
            pair("0xabc", "TKN", 1.0, 5_000.0),
            pair("0xabc", "TKN", 2.0, 900_000.0),
            pair("0xabc", "TKN", 3.0, 100_000.0),
        ];
        let price = best_price(&pairs, "0xABC").expect("a price");
        assert_eq!(price.price_usd, 2.0);
        assert_eq!(price.liquidity_usd, 900_000.0);
        assert_eq!(price.symbol, "TKN");
    }

    #[test]
    fn a_pair_where_the_token_is_the_quote_does_not_price_it() {
        // USDC is the quote here, so this pair prices WETH, not USDC.
        let pairs = vec![serde_json::json!({
            "baseToken": {"address": "0xweth", "symbol": "WETH"},
            "quoteToken": {"address": "0xusdc", "symbol": "USDC"},
            "priceUsd": 3_000.0,
            "liquidity": {"usd": 1_000_000.0},
        })];
        assert!(best_price(&pairs, "0xusdc").is_none());
    }

    #[test]
    fn a_useless_price_is_not_a_price() {
        for price in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let pairs = vec![pair("0xabc", "TKN", price, 1_000.0)];
            assert!(
                best_price(&pairs, "0xabc").is_none(),
                "{price} must not be quoted"
            );
        }
        assert!(best_price(&[], "0xabc").is_none());
    }

    /// The feed quotes the price as a string and the liquidity as a number.
    /// Reading only JSON numbers would leave every holding unpriced.
    #[test]
    fn a_price_sent_as_a_string_is_still_a_price() {
        let pairs = vec![serde_json::json!({
            "baseToken": {"address": "0xabc", "symbol": "TKN"},
            "priceUsd": "1934.54",
            "liquidity": {"usd": 78_825_593.68},
        })];
        let price = best_price(&pairs, "0xabc").expect("a price");
        assert_eq!(price.price_usd, 1934.54);
        assert_eq!(price.liquidity_usd, 78_825_593.68);

        let garbage = vec![serde_json::json!({
            "baseToken": {"address": "0xabc", "symbol": "TKN"},
            "priceUsd": "not a number",
        })];
        assert!(best_price(&garbage, "0xabc").is_none());
    }

    /// Values a real, public address against mainnet and the live price feed.
    ///
    /// Ignored by default because it needs the network. Run it with
    /// `cargo test -p openether-core holdings -- --ignored`.
    ///
    /// The address is a public exchange hot wallet. Nothing is signed and no
    /// key is involved: this is a read of public balances.
    #[tokio::test]
    #[ignore = "needs the network"]
    async fn a_real_wallet_is_valued_from_the_chain_and_the_feed() {
        let onchain = Arc::new(
            OnchainClient::new(
                Some("https://api.mainnet-beta.solana.com".into()),
                vec![(
                    "ethereum".into(),
                    "https://ethereum-rpc.publicnode.com".into(),
                )],
            )
            .expect("client"),
        );
        let service = HoldingsService::new(onchain, None).expect("service");
        let holdings = service
            .evm(
                "ethereum",
                "0x28C6c06298d514Db089934071355E5743bf21d60",
                &[
                    "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".into(),
                    "0xdAC17F958D2ee523a2206206994597C13D831ec7".into(),
                ],
            )
            .await
            .expect("the chain read");

        assert!(holdings.native_balance > 0.0, "{holdings:?}");
        assert!(holdings.native_price_usd > 0.0, "ETH must be priced");
        assert!(holdings.value_usd > 0.0);

        let usdc = holdings
            .tokens
            .iter()
            .find(|token| token.symbol == "USDC")
            .expect("USDC is held");
        assert_eq!(usdc.decimals, 6, "USDC has 6 decimals, not 18");
        assert!(usdc.priced);
        assert!(usdc.price_usd > 0.5 && usdc.price_usd < 2.0);
        assert!((usdc.value_usd - usdc.balance * usdc.price_usd).abs() < 1e-6);
    }

    /// Values a real Solana address, including the SPL tokens the chain
    /// reports. Ignored by default because it needs the network.
    #[tokio::test]
    #[ignore = "needs the network"]
    async fn a_real_solana_wallet_is_valued() {
        let onchain = Arc::new(
            OnchainClient::new(
                Some("https://api.mainnet-beta.solana.com".into()),
                Vec::new(),
            )
            .expect("client"),
        );
        let service = HoldingsService::new(onchain, None).expect("service");
        let holdings = service
            .solana("5tzFkiKscXHK5ZXCGbXZxdw7gTjjD1mBwuoFbhUvuAi9")
            .await
            .expect("the chain read");

        assert_eq!(holdings.native_symbol, "SOL");
        assert!(holdings.native_price_usd > 0.0, "SOL must be priced");
        assert!(!holdings.tokens.is_empty(), "the address holds SPL tokens");
        assert!(holdings.value_usd > 0.0);
        // The read is bounded, and what it left out is reported.
        assert!(holdings.tokens.len() <= MAX_VALUED_TOKENS);
        assert!(holdings.tokens.iter().all(|token| token.balance > 0.0));
    }
}

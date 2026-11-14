//! Quoting a DEX swap and turning it into a transaction the broadcaster signs.
//!
//! A quote comes from an external aggregator, so it is treated as untrusted
//! input. Before a byte is signed the service checks, in order:
//!
//! * the swap is on an EVM network this core can send on (Solana swaps are
//!   refused: an opaque aggregator transaction cannot be validated here);
//! * both tokens are addresses, or the chain's own coin;
//! * the router *and* the contract that moves the tokens are on the contract
//!   allowlist, and the allowlist is not empty;
//! * the quote is for exactly the tokens and amount that were asked for;
//! * the native value attached to the call is the amount being sold, and zero
//!   for a token sale;
//! * the wallet holds the sell token, and — for a token sale — has allowed the
//!   spender to move it.
//!
//! The calldata itself is deliberately opaque: it is not decoded into a
//! friendly "action" and then re-encoded, because a decoder that disagrees with
//! the router is how a permit turns into a transfer. It is signed as a call to
//! an allowlisted contract, and the dollar value at risk is capped by the same
//! armed limit as a transfer.

use crate::broadcast::{parse_evm_address, BroadcastError, Broadcaster};
use crate::holdings::wrapped_native_address;
use crate::types::now_ms;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

/// The label for a chain's own coin in a swap request.
pub const NATIVE_TOKEN: &str = "native";
/// Native coins on the EVM chains this core sends on have 18 decimals.
const EVM_NATIVE_DECIMALS: u32 = 18;
/// The default tolerance a quote is allowed, in basis points.
pub const DEFAULT_SLIPPAGE_BPS: u16 = 50;
/// Slippage above ten percent is not a quote, it is giving the money away.
pub const MAX_SLIPPAGE_BPS: u16 = 1_000;
/// Calldata larger than this is not a router call worth signing.
const MAX_CALLDATA_BYTES: usize = 8_192;

const SELECTOR_DECIMALS: [u8; 4] = [0x31, 0x3c, 0xe5, 0x67];
const SELECTOR_BALANCE_OF: [u8; 4] = [0x70, 0xa0, 0x82, 0x31];
const SELECTOR_ALLOWANCE: [u8; 4] = [0xdd, 0x62, 0xed, 0x3e];
const SELECTOR_APPROVE: [u8; 4] = [0x09, 0x5e, 0xa7, 0xb3];

fn default_slippage() -> u16 {
    DEFAULT_SLIPPAGE_BPS
}

/// What the caller wants to trade.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapRequest {
    /// An EVM network label, e.g. `ethereum` or `base`.
    pub network: String,
    /// `native` or a token contract address.
    pub sell_token: String,
    /// `native` or a token contract address.
    pub buy_token: String,
    /// The amount sold, in the sell token's smallest unit.
    pub sell_amount_raw: u128,
    /// How much worse than the quote the fill may be, in basis points.
    #[serde(default = "default_slippage")]
    pub slippage_bps: u16,
}

impl SwapRequest {
    /// The same request with normalized labels, or a refusal.
    pub fn normalized(&self) -> Result<SwapRequest, SwapError> {
        let network = self.network.trim().to_ascii_lowercase();
        let sell_token = normalize_token(&self.sell_token)?;
        let buy_token = normalize_token(&self.buy_token)?;
        if sell_token == buy_token {
            return Err(SwapError::InvalidRequest(
                "the sell and buy tokens are the same".into(),
            ));
        }
        if self.sell_amount_raw == 0 {
            return Err(SwapError::InvalidRequest(
                "the amount to sell must be greater than zero".into(),
            ));
        }
        if self.slippage_bps == 0 || self.slippage_bps > MAX_SLIPPAGE_BPS {
            return Err(SwapError::InvalidRequest(format!(
                "slippage must be between 1 and {MAX_SLIPPAGE_BPS} basis points"
            )));
        }
        Ok(SwapRequest {
            network,
            sell_token,
            buy_token,
            sell_amount_raw: self.sell_amount_raw,
            slippage_bps: self.slippage_bps,
        })
    }
}

/// Everything that can go wrong before a swap reaches the signer.
#[derive(Debug, thiserror::Error)]
pub enum SwapError {
    #[error("swaps are not configured: set OPENETHER_SWAP_QUOTE_URL to a quote source")]
    NotConfigured,
    #[error("the network '{0}' cannot execute a swap: {1}")]
    NetworkUnsupported(String, String),
    #[error("invalid swap: {0}")]
    InvalidRequest(String),
    #[error("the quote source answered with an error: {0}")]
    QuoteSource(String),
    #[error("the quote source could not be reached: {0}")]
    Transport(String),
    #[error("the quote is missing {0}")]
    MalformedQuote(String),
    #[error("the quote is for {0}, not the {1} that was requested")]
    QuoteMismatch(String, String),
    #[error("the quote attaches {0} native units, but the trade moves {1}")]
    ValueMismatch(u128, u128),
    #[error(
        "the contract allowlist is empty; a swap needs an explicit list of routers it may call"
    )]
    EmptyAllowlist,
    #[error("contract {0} is not on the contract allowlist")]
    ContractNotAllowed(String),
    #[error("the wallet holds {0} raw units of {1}, less than the {2} the swap sells")]
    InsufficientBalance(u128, String, u128),
    #[error("the {0} contract did not answer the read this swap needs")]
    ContractReadFailed(String),
    #[error("the token {0} did not answer decimals() with a sane value")]
    UnknownDecimals(String),
    #[error(transparent)]
    Rpc(#[from] BroadcastError),
}

/// Values a quote needs from the chain and the caller before the source is
/// asked: the signer, the chain id and both tokens' decimals.
#[derive(Debug, Clone)]
pub struct QuoteContext {
    pub from: String,
    pub chain_id: u64,
    pub sell_decimals: u32,
    pub buy_decimals: u32,
    pub slippage_bps: u16,
}

/// A source of swap quotes. It returns the raw JSON body; parsing and every
/// safety check belong to [`SwapService`], so a source cannot widen them.
#[async_trait::async_trait]
pub trait SwapQuoter: Send + Sync {
    /// A host name for the status surface. Never the full URL: a query string
    /// can carry an API key.
    fn source(&self) -> String;
    async fn fetch(
        &self,
        request: &SwapRequest,
        context: &QuoteContext,
    ) -> Result<Value, SwapError>;
}

/// A quoter backed by an HTTP endpoint template.
///
/// The template must name the sell token, the buy token and the amount, so a
/// misconfigured URL cannot silently price something else. Available
/// placeholders: `{chain_id}`, `{network}`, `{sell_token}`, `{buy_token}`,
/// `{sell_decimals}`, `{buy_decimals}`, `{sell_amount}`, `{from}` and
/// `{slippage_bps}`.
#[derive(Debug)]
pub struct HttpQuoter {
    client: reqwest::Client,
    template: String,
}

impl HttpQuoter {
    pub fn new(template: String) -> Result<Self, SwapError> {
        for placeholder in ["{sell_token}", "{buy_token}", "{sell_amount}"] {
            if !template.contains(placeholder) {
                return Err(SwapError::InvalidRequest(format!(
                    "the quote url template must contain {placeholder}"
                )));
            }
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|e| SwapError::Transport(e.to_string()))?;
        Ok(Self { client, template })
    }

    fn url(&self, request: &SwapRequest, context: &QuoteContext) -> String {
        self.template
            .replace("{chain_id}", &context.chain_id.to_string())
            .replace("{network}", &request.network)
            .replace("{sell_token}", &request.sell_token)
            .replace("{buy_token}", &request.buy_token)
            .replace("{sell_decimals}", &context.sell_decimals.to_string())
            .replace("{buy_decimals}", &context.buy_decimals.to_string())
            .replace("{sell_amount}", &request.sell_amount_raw.to_string())
            .replace("{from}", &context.from)
            .replace("{slippage_bps}", &context.slippage_bps.to_string())
    }
}

#[async_trait::async_trait]
impl SwapQuoter for HttpQuoter {
    fn source(&self) -> String {
        let base = self.template.split('?').next().unwrap_or_default();
        reqwest::Url::parse(base)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string))
            .unwrap_or_else(|| "configured quote source".to_string())
    }

    async fn fetch(
        &self,
        request: &SwapRequest,
        context: &QuoteContext,
    ) -> Result<Value, SwapError> {
        let url = self.url(request, context);
        let response = self
            .client
            .get(url)
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|e| SwapError::Transport(e.to_string()))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| SwapError::Transport(e.to_string()))?;
        if !status.is_success() {
            // The source's error text is useful, but a long HTML page is not.
            let detail: String = body.chars().take(200).collect();
            return Err(SwapError::QuoteSource(format!("http {status}: {detail}")));
        }
        serde_json::from_str(&body)
            .map_err(|e| SwapError::QuoteSource(format!("the answer is not json: {e}")))
    }
}

/// How much of the sell token the wallet holds.
#[derive(Debug, Clone, Serialize)]
pub struct BalanceCheck {
    pub token: String,
    pub balance_raw: u128,
    pub required_raw: u128,
    pub sufficient: bool,
}

/// Whether the router may already move the sell token.
#[derive(Debug, Clone, Serialize)]
pub struct AllowanceCheck {
    pub token: String,
    pub spender: String,
    pub current_raw: u128,
    pub required_raw: u128,
    pub sufficient: bool,
}

/// A validated quote, ready to be signed.
#[derive(Debug, Clone, Serialize)]
pub struct SwapQuote {
    pub network: String,
    pub sell_token: String,
    pub buy_token: String,
    pub sell_amount_raw: u128,
    pub sell_decimals: u32,
    pub buy_decimals: u32,
    /// What the source expects the wallet to receive, before slippage.
    pub buy_amount_raw: u128,
    /// Native value attached to the call. Equals the amount sold for a native
    /// sale and is zero for a token sale.
    pub value_raw: u128,
    /// The router the calldata calls.
    pub to: String,
    /// The opaque calldata, hex encoded exactly as the source returned it.
    pub data: String,
    /// Who moves the sell token. `None` when the router itself does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowance_target: Option<String>,
    pub balance: BalanceCheck,
    /// Absent when the swap sells the native coin.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowance: Option<AllowanceCheck>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gas_limit_estimate: Option<u64>,
    pub source: String,
    pub fetched_at_ms: i64,
}

impl SwapQuote {
    /// The calldata as bytes.
    pub fn call_data(&self) -> Result<Vec<u8>, SwapError> {
        hex::decode(self.data.trim_start_matches("0x"))
            .map_err(|_| SwapError::MalformedQuote("hex calldata".into()))
    }
}

/// The untrusted part of a quote, before the chain is consulted.
struct ParsedQuote {
    buy_amount_raw: u128,
    to: String,
    data: String,
    value_raw: u128,
    allowance_target: Option<String>,
    gas_limit: Option<u64>,
}

/// Quotes and checks swaps against one broadcaster.
pub struct SwapService {
    quoter: Arc<dyn SwapQuoter>,
    broadcaster: Arc<Broadcaster>,
}

impl SwapService {
    pub fn new(quoter: Arc<dyn SwapQuoter>, broadcaster: Arc<Broadcaster>) -> Self {
        Self {
            quoter,
            broadcaster,
        }
    }

    pub fn source(&self) -> String {
        self.quoter.source()
    }

    /// Fetches a quote and checks everything that can be checked before a
    /// signature. Nothing here signs or sends.
    pub async fn quote(
        &self,
        request: &SwapRequest,
        from: &str,
        allowlist: &HashSet<String>,
    ) -> Result<SwapQuote, SwapError> {
        // The network check comes first so an unsupported chain reports why it
        // cannot swap instead of failing later on that chain's address shape.
        let network = request.network.trim().to_ascii_lowercase();
        let chain_id = self.broadcaster.evm_chain_id(&network).ok_or_else(|| {
            let reason = if network == "solana" {
                "an opaque aggregator transaction cannot be validated before signing"
            } else {
                "it has no EVM RPC url configured"
            };
            SwapError::NetworkUnsupported(network.clone(), reason.into())
        })?;
        let request = request.normalized()?;
        let from = normalize_address(from)?;

        let sell_decimals = self
            .token_decimals(&request.network, &request.sell_token)
            .await?;
        let buy_decimals = self
            .token_decimals(&request.network, &request.buy_token)
            .await?;

        let context = QuoteContext {
            from: from.clone(),
            chain_id,
            sell_decimals,
            buy_decimals,
            slippage_bps: request.slippage_bps,
        };
        let body = self.quoter.fetch(&request, &context).await?;
        let parsed = parse_quote(&body, &request)?;
        enforce_allowlist(&parsed.to, parsed.allowance_target.as_deref(), allowlist)?;

        let balance_raw = self
            .balance_of(&request.network, &request.sell_token, &from)
            .await?;
        if balance_raw < request.sell_amount_raw {
            return Err(SwapError::InsufficientBalance(
                balance_raw,
                request.sell_token.clone(),
                request.sell_amount_raw,
            ));
        }

        let allowance = if request.sell_token == NATIVE_TOKEN {
            None
        } else {
            let spender = parsed
                .allowance_target
                .clone()
                .unwrap_or_else(|| parsed.to.clone());
            let current_raw = self
                .token_allowance(&request.network, &request.sell_token, &from, &spender)
                .await?;
            Some(AllowanceCheck {
                token: request.sell_token.clone(),
                spender,
                current_raw,
                required_raw: request.sell_amount_raw,
                sufficient: current_raw >= request.sell_amount_raw,
            })
        };

        Ok(SwapQuote {
            network: request.network.clone(),
            sell_token: request.sell_token.clone(),
            buy_token: request.buy_token.clone(),
            sell_amount_raw: request.sell_amount_raw,
            sell_decimals,
            buy_decimals,
            buy_amount_raw: parsed.buy_amount_raw,
            value_raw: parsed.value_raw,
            to: parsed.to,
            data: parsed.data,
            allowance_target: parsed.allowance_target,
            balance: BalanceCheck {
                token: request.sell_token,
                balance_raw,
                required_raw: request.sell_amount_raw,
                sufficient: true,
            },
            allowance,
            gas_limit_estimate: parsed.gas_limit,
            source: self.quoter.source(),
            fetched_at_ms: now_ms(),
        })
    }

    async fn token_decimals(&self, network: &str, token: &str) -> Result<u32, SwapError> {
        if token == NATIVE_TOKEN {
            return Ok(EVM_NATIVE_DECIMALS);
        }
        let bytes = self
            .eth_call(network, token, &SELECTOR_DECIMALS, &[])
            .await?;
        match decode_u128(&bytes) {
            Some(value) if value <= 36 => Ok(value as u32),
            _ => Err(SwapError::UnknownDecimals(token.to_string())),
        }
    }

    async fn balance_of(&self, network: &str, token: &str, owner: &str) -> Result<u128, SwapError> {
        if token == NATIVE_TOKEN {
            let answer = self
                .broadcaster
                .call_rpc(network, "eth_getBalance", json!([owner, "latest"]))
                .await?;
            let text = answer
                .as_str()
                .ok_or_else(|| SwapError::ContractReadFailed("native balance".into()))?;
            return u128::from_str_radix(text.trim_start_matches("0x"), 16)
                .map_err(|_| SwapError::ContractReadFailed("native balance".into()));
        }
        let owner = address_bytes(owner)?;
        let bytes = self
            .eth_call(
                network,
                token,
                &SELECTOR_BALANCE_OF,
                &[address_word(&owner)],
            )
            .await?;
        decode_u128(&bytes)
            .ok_or_else(|| SwapError::ContractReadFailed(format!("{token} balanceOf()")))
    }

    async fn token_allowance(
        &self,
        network: &str,
        token: &str,
        owner: &str,
        spender: &str,
    ) -> Result<u128, SwapError> {
        let owner = address_bytes(owner)?;
        let spender = address_bytes(spender)?;
        let bytes = self
            .eth_call(
                network,
                token,
                &SELECTOR_ALLOWANCE,
                &[address_word(&owner), address_word(&spender)],
            )
            .await?;
        decode_u128(&bytes)
            .ok_or_else(|| SwapError::ContractReadFailed(format!("{token} allowance()")))
    }

    async fn eth_call(
        &self,
        network: &str,
        to: &str,
        selector: &[u8; 4],
        words: &[[u8; 32]],
    ) -> Result<Vec<u8>, SwapError> {
        let mut data = selector.to_vec();
        for word in words {
            data.extend_from_slice(word);
        }
        let answer = self
            .broadcaster
            .call_rpc(
                network,
                "eth_call",
                json!([{
                    "to": to,
                    "data": format!("0x{}", hex::encode(&data)),
                }, "latest"]),
            )
            .await?;
        let text = answer.as_str().unwrap_or_default();
        hex::decode(text.trim_start_matches("0x"))
            .map_err(|_| SwapError::ContractReadFailed(to.into()))
    }
}

/// Reads the fields a quote has to carry, whichever of the common shapes the
/// source used, and refuses a quote that is for something else.
fn parse_quote(body: &Value, request: &SwapRequest) -> Result<ParsedQuote, SwapError> {
    let root = ["priceRoute", "quote", "result"]
        .iter()
        .find_map(|key| body.get(*key).filter(|value| value.is_object()))
        .unwrap_or(body);
    let empty = Value::Null;
    let tx = root
        .get("tx")
        .filter(|value| value.is_object())
        .unwrap_or(&empty);
    let field = |keys: &[&str]| -> Option<&Value> {
        keys.iter().find_map(|key| {
            root.get(*key)
                .filter(|value| !value.is_null())
                .or_else(|| tx.get(*key).filter(|value| !value.is_null()))
        })
    };

    if let Some(echoed) = field(&["srcToken", "sellToken", "fromToken"]).and_then(Value::as_str) {
        if !token_matches(&request.network, &request.sell_token, echoed) {
            return Err(SwapError::QuoteMismatch(
                echoed.to_string(),
                request.sell_token.clone(),
            ));
        }
    }
    if let Some(echoed) = field(&["destToken", "buyToken", "toToken"]).and_then(Value::as_str) {
        if !token_matches(&request.network, &request.buy_token, echoed) {
            return Err(SwapError::QuoteMismatch(
                echoed.to_string(),
                request.buy_token.clone(),
            ));
        }
    }
    if let Some(value) = field(&["srcAmount", "sellAmount", "fromAmount"]) {
        let echoed = parse_raw(value, "srcAmount")?;
        if echoed != request.sell_amount_raw {
            return Err(SwapError::QuoteMismatch(
                echoed.to_string(),
                request.sell_amount_raw.to_string(),
            ));
        }
    }

    let to = normalize_address(
        field(&["to", "router", "contractAddress"])
            .and_then(Value::as_str)
            .ok_or_else(|| SwapError::MalformedQuote("the router address".into()))?,
    )?;

    let data = normalize_calldata(
        field(&["data", "calldata"])
            .and_then(Value::as_str)
            .ok_or_else(|| SwapError::MalformedQuote("the calldata".into()))?,
    )?;

    let value_raw = match field(&["value"]) {
        Some(value) => parse_raw(value, "value")?,
        None => 0,
    };

    let buy_amount_raw = parse_raw(
        field(&["destAmount", "buyAmount", "amountOut", "toAmount"])
            .ok_or_else(|| SwapError::MalformedQuote("the expected output amount".into()))?,
        "destAmount",
    )?;
    if buy_amount_raw == 0 {
        return Err(SwapError::MalformedQuote(
            "a non-zero expected output amount".into(),
        ));
    }

    // The native value attached to the call is money leaving the wallet. It
    // must be exactly the amount the quote priced: the sale proceeds for a
    // native sale, and nothing at all for a token sale.
    let expected_value = if request.sell_token == NATIVE_TOKEN {
        request.sell_amount_raw
    } else {
        0
    };
    if value_raw != expected_value {
        return Err(SwapError::ValueMismatch(value_raw, expected_value));
    }

    let allowance_target = match field(&["tokenTransferProxy", "allowanceTarget", "spender"]) {
        Some(Value::String(text)) => Some(normalize_address(text)?),
        Some(_) => {
            return Err(SwapError::MalformedQuote(
                "an allowance target address".into(),
            ))
        }
        None => None,
    };

    let gas_limit = match field(&["gas", "gasLimit", "estimatedGas"]) {
        Some(value) => u64::try_from(parse_raw(value, "gas")?).ok(),
        None => None,
    };

    Ok(ParsedQuote {
        buy_amount_raw,
        to,
        data,
        value_raw,
        allowance_target,
        gas_limit,
    })
}

/// Refuses a router or a token mover that the operator did not name.
pub(crate) fn enforce_allowlist(
    router: &str,
    spender: Option<&str>,
    allowlist: &HashSet<String>,
) -> Result<(), SwapError> {
    if allowlist.is_empty() {
        return Err(SwapError::EmptyAllowlist);
    }
    let allowed = |address: &str| {
        allowlist
            .iter()
            .any(|entry| entry.trim().eq_ignore_ascii_case(address))
    };
    if !allowed(router) {
        return Err(SwapError::ContractNotAllowed(router.to_string()));
    }
    if let Some(spender) = spender {
        if !allowed(spender) {
            return Err(SwapError::ContractNotAllowed(spender.to_string()));
        }
    }
    Ok(())
}

/// `approve(spender, amount)` on an ERC-20, for the spender a quote named.
pub fn encode_approve(spender: &str, amount: u128) -> Result<Vec<u8>, SwapError> {
    let spender = address_bytes(spender)?;
    let mut out = SELECTOR_APPROVE.to_vec();
    out.extend_from_slice(&address_word(&spender));
    out.extend_from_slice(&u128_word(amount));
    Ok(out)
}

/// `native` or a lowercase 20-byte address.
pub(crate) fn normalize_token(value: &str) -> Result<String, SwapError> {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case(NATIVE_TOKEN) {
        return Ok(NATIVE_TOKEN.to_string());
    }
    normalize_address(trimmed)
}

pub(crate) fn normalize_address(value: &str) -> Result<String, SwapError> {
    let bytes = address_bytes(value)?;
    Ok(format!("0x{}", hex::encode(bytes)))
}

fn address_bytes(value: &str) -> Result<[u8; 20], SwapError> {
    parse_evm_address(value)
        .map_err(|_| SwapError::InvalidRequest(format!("not an evm address: {value}")))
}

fn normalize_calldata(value: &str) -> Result<String, SwapError> {
    let trimmed = value.trim();
    let hex_part = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .ok_or_else(|| SwapError::MalformedQuote("0x-prefixed calldata".into()))?;
    if hex_part.is_empty() {
        return Err(SwapError::MalformedQuote("non-empty calldata".into()));
    }
    if hex_part.len() % 2 != 0 {
        return Err(SwapError::MalformedQuote(
            "calldata with whole bytes".into(),
        ));
    }
    let bytes =
        hex::decode(hex_part).map_err(|_| SwapError::MalformedQuote("hex calldata".into()))?;
    if bytes.len() > MAX_CALLDATA_BYTES {
        return Err(SwapError::MalformedQuote(format!(
            "calldata under {MAX_CALLDATA_BYTES} bytes"
        )));
    }
    Ok(format!("0x{}", hex::encode(bytes)))
}

/// Whether a token echoed by the source is the one that was asked for. A native
/// request may be echoed as `native`, the zero address or the chain's wrapped
/// coin, because aggregators spell it differently.
fn token_matches(network: &str, requested: &str, echoed: &str) -> bool {
    let Ok(echoed) = normalize_token(echoed) else {
        return false;
    };
    if requested != NATIVE_TOKEN {
        return requested.eq_ignore_ascii_case(&echoed);
    }
    if echoed == NATIVE_TOKEN {
        return true;
    }
    let Ok(bytes) = parse_evm_address(&echoed) else {
        return false;
    };
    if bytes.iter().all(|byte| *byte == 0) {
        return true;
    }
    wrapped_native_address(network)
        .map(|wrapped| echoed.eq_ignore_ascii_case(wrapped))
        .unwrap_or(false)
}

/// A raw amount, as a decimal string, a hex quantity or a JSON number.
fn parse_raw(value: &Value, field: &str) -> Result<u128, SwapError> {
    let text = match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        _ => return Err(SwapError::MalformedQuote(format!("a numeric {field}"))),
    };
    let text = text.trim();
    let parsed = if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        u128::from_str_radix(hex, 16)
    } else {
        text.parse::<u128>()
    };
    parsed.map_err(|_| SwapError::MalformedQuote(format!("a numeric {field}")))
}

/// The low 128 bits of an ABI word. A word whose high half is set is not a
/// balance or a decimal count this core can work with.
fn decode_u128(bytes: &[u8]) -> Option<u128> {
    if bytes.len() < 32 {
        return None;
    }
    let word = &bytes[bytes.len() - 32..];
    if word[..16].iter().any(|byte| *byte != 0) {
        return None;
    }
    let mut low = [0u8; 16];
    low.copy_from_slice(&word[16..]);
    Some(u128::from_be_bytes(low))
}

fn address_word(address: &[u8; 20]) -> [u8; 32] {
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(address);
    word
}

fn u128_word(value: u128) -> [u8; 32] {
    let mut word = [0u8; 32];
    word[16..].copy_from_slice(&value.to_be_bytes());
    word
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broadcast::RpcTransport;
    use std::collections::HashMap;
    use std::sync::Mutex;

    const RPC: &str = "http://evm.test";
    const ROUTER: &str = "0x1111111254eeb25477b68fb85ed929f73a960582";
    const PROXY: &str = "0x216b4b4ba9f3e719726886d34a177484278bfcae";
    const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
    const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
    const OWNER: &str = "0x00000000000000000000000000000000000000aa";

    struct FakeQuoter {
        body: Value,
        calls: Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl SwapQuoter for FakeQuoter {
        fn source(&self) -> String {
            "fake.quotes".into()
        }

        async fn fetch(
            &self,
            _request: &SwapRequest,
            _context: &QuoteContext,
        ) -> Result<Value, SwapError> {
            *self.calls.lock().unwrap() += 1;
            Ok(self.body.clone())
        }
    }

    /// Answers the token reads a quote needs from configured maps.
    struct MockReads {
        decimals: HashMap<String, u128>,
        balances: HashMap<String, u128>,
        allowances: HashMap<String, u128>,
        native_balance: u128,
    }

    impl MockReads {
        fn new() -> Self {
            Self {
                decimals: HashMap::from([(USDC.to_string(), 6), (WETH.to_string(), 18)]),
                balances: HashMap::from([(USDC.to_string(), 5_000_000)]),
                allowances: HashMap::new(),
                native_balance: 0,
            }
        }

        fn with_allowance(mut self, token: &str, amount: u128) -> Self {
            self.allowances.insert(token.to_string(), amount);
            self
        }

        fn with_native_balance(mut self, amount: u128) -> Self {
            self.native_balance = amount;
            self
        }
    }

    #[async_trait::async_trait]
    impl RpcTransport for MockReads {
        async fn call(
            &self,
            _url: &str,
            method: &str,
            params: Value,
        ) -> Result<Value, BroadcastError> {
            match method {
                "eth_call" => {
                    let to = params[0]["to"]
                        .as_str()
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    let data = params[0]["data"].as_str().unwrap_or_default();
                    let value = if data.starts_with("0x313ce567") {
                        *self.decimals.get(&to).unwrap_or(&0)
                    } else if data.starts_with("0x70a08231") {
                        *self.balances.get(&to).unwrap_or(&0)
                    } else if data.starts_with("0xdd62ed3e") {
                        *self.allowances.get(&to).unwrap_or(&0)
                    } else {
                        panic!("unexpected eth_call {data}");
                    };
                    Ok(json!(format!("0x{}", hex::encode(u128_word(value)))))
                }
                "eth_getBalance" => Ok(json!(format!(
                    "0x{}",
                    hex::encode(u128_word(self.native_balance))
                ))),
                other => panic!("unexpected rpc method {other}"),
            }
        }
    }

    fn service(body: Value, reads: MockReads) -> SwapService {
        let broadcaster = Broadcaster::new(
            Arc::new(reads),
            vec![("ethereum".into(), RPC.into())],
            None,
            u128::MAX,
        );
        SwapService::new(
            Arc::new(FakeQuoter {
                body,
                calls: Mutex::new(0),
            }),
            Arc::new(broadcaster),
        )
    }

    fn allowlist() -> HashSet<String> {
        HashSet::from([ROUTER.to_string(), PROXY.to_string()])
    }

    fn quote_body(sell: &str, buy: &str, amount: u128) -> Value {
        json!({
            "priceRoute": {
                "srcToken": sell,
                "destToken": buy,
                "srcAmount": amount.to_string(),
                "destAmount": "500000000000000",
                "to": ROUTER,
                "data": "0xdeadbeef",
                "value": "0",
                "tokenTransferProxy": PROXY,
                "gas": 200000
            }
        })
    }

    fn request() -> SwapRequest {
        SwapRequest {
            network: "ethereum".into(),
            sell_token: USDC.into(),
            buy_token: WETH.into(),
            sell_amount_raw: 1_000_000,
            slippage_bps: DEFAULT_SLIPPAGE_BPS,
        }
    }

    #[tokio::test]
    async fn a_quote_is_read_and_the_allowance_is_reported() {
        let swap = service(quote_body(USDC, WETH, 1_000_000), MockReads::new());
        let quote = swap.quote(&request(), OWNER, &allowlist()).await.unwrap();
        assert_eq!(quote.sell_decimals, 6);
        assert_eq!(quote.buy_decimals, 18);
        assert_eq!(quote.buy_amount_raw, 500_000_000_000_000);
        assert_eq!(quote.to, ROUTER);
        assert_eq!(quote.data, "0xdeadbeef");
        assert_eq!(quote.value_raw, 0);
        assert_eq!(quote.allowance_target.as_deref(), Some(PROXY));
        assert!(quote.balance.sufficient);
        let allowance = quote.allowance.unwrap();
        assert_eq!(allowance.spender, PROXY);
        assert_eq!(allowance.current_raw, 0);
        assert!(!allowance.sufficient);
        assert_eq!(quote.source, "fake.quotes");
    }

    #[tokio::test]
    async fn an_existing_allowance_is_enough() {
        let reads = MockReads::new().with_allowance(USDC, 2_000_000);
        let swap = service(quote_body(USDC, WETH, 1_000_000), reads);
        let quote = swap.quote(&request(), OWNER, &allowlist()).await.unwrap();
        assert!(quote.allowance.unwrap().sufficient);
    }

    #[tokio::test]
    async fn a_quote_for_another_amount_is_refused() {
        let swap = service(quote_body(USDC, WETH, 2_000_000), MockReads::new());
        let error = swap
            .quote(&request(), OWNER, &allowlist())
            .await
            .unwrap_err();
        assert!(matches!(error, SwapError::QuoteMismatch(_, _)));
    }

    #[tokio::test]
    async fn a_quote_for_another_token_is_refused() {
        let swap = service(quote_body(WETH, WETH, 1_000_000), MockReads::new());
        let error = swap
            .quote(&request(), OWNER, &allowlist())
            .await
            .unwrap_err();
        assert!(matches!(error, SwapError::QuoteMismatch(_, _)));
    }

    #[tokio::test]
    async fn a_router_off_the_allowlist_is_refused() {
        let swap = service(quote_body(USDC, WETH, 1_000_000), MockReads::new());
        let only_proxy = HashSet::from([PROXY.to_string()]);
        let error = swap
            .quote(&request(), OWNER, &only_proxy)
            .await
            .unwrap_err();
        assert!(matches!(error, SwapError::ContractNotAllowed(ref c) if c == ROUTER));
    }

    #[tokio::test]
    async fn a_spender_off_the_allowlist_is_refused() {
        let swap = service(quote_body(USDC, WETH, 1_000_000), MockReads::new());
        let only_router = HashSet::from([ROUTER.to_string()]);
        let error = swap
            .quote(&request(), OWNER, &only_router)
            .await
            .unwrap_err();
        assert!(matches!(error, SwapError::ContractNotAllowed(ref c) if c == PROXY));
    }

    #[tokio::test]
    async fn an_empty_allowlist_refuses_every_swap() {
        let swap = service(quote_body(USDC, WETH, 1_000_000), MockReads::new());
        let error = swap
            .quote(&request(), OWNER, &HashSet::new())
            .await
            .unwrap_err();
        assert!(matches!(error, SwapError::EmptyAllowlist));
    }

    #[tokio::test]
    async fn a_short_balance_is_refused_before_signing() {
        let mut reads = MockReads::new();
        reads.balances.insert(USDC.to_string(), 500_000);
        let swap = service(quote_body(USDC, WETH, 1_000_000), reads);
        let error = swap
            .quote(&request(), OWNER, &allowlist())
            .await
            .unwrap_err();
        assert!(matches!(error, SwapError::InsufficientBalance(..)));
    }

    #[tokio::test]
    async fn a_native_sale_must_attach_the_quoted_amount() {
        let mut request = request();
        request.sell_token = NATIVE_TOKEN.into();
        request.buy_token = USDC.into();
        request.sell_amount_raw = 1_000_000_000_000_000;
        let mut body = quote_body(WETH, USDC, 1_000_000_000_000_000);
        // A quote that attaches nothing while asking to sell the native coin
        // would leave the router to pull funds the wallet never agreed to.
        body["priceRoute"]["value"] = json!("0");
        let swap = service(
            body,
            MockReads::new().with_native_balance(2_000_000_000_000_000),
        );
        let error = swap.quote(&request, OWNER, &allowlist()).await.unwrap_err();
        assert!(matches!(error, SwapError::ValueMismatch(0, _)));
    }

    #[tokio::test]
    async fn a_native_sale_with_the_right_value_is_accepted() {
        let mut request = request();
        request.sell_token = NATIVE_TOKEN.into();
        request.buy_token = USDC.into();
        request.sell_amount_raw = 1_000_000_000_000_000;
        // The aggregator echoes the wrapped coin for a native sale.
        let mut body = quote_body(WETH, USDC, 1_000_000_000_000_000);
        body["priceRoute"]["value"] = json!("1000000000000000");
        let swap = service(
            body,
            MockReads::new().with_native_balance(2_000_000_000_000_000),
        );
        let quote = swap.quote(&request, OWNER, &allowlist()).await.unwrap();
        assert_eq!(quote.value_raw, 1_000_000_000_000_000);
        assert_eq!(quote.sell_decimals, 18);
        assert!(quote.allowance.is_none());
    }

    #[tokio::test]
    async fn a_token_sale_must_not_attach_native_value() {
        let mut body = quote_body(USDC, WETH, 1_000_000);
        body["priceRoute"]["value"] = json!("1");
        let swap = service(body, MockReads::new());
        let error = swap
            .quote(&request(), OWNER, &allowlist())
            .await
            .unwrap_err();
        assert!(matches!(error, SwapError::ValueMismatch(1, 0)));
    }

    #[tokio::test]
    async fn a_quote_without_calldata_is_refused() {
        let mut body = quote_body(USDC, WETH, 1_000_000);
        body["priceRoute"]["data"] = Value::Null;
        let swap = service(body, MockReads::new());
        let error = swap
            .quote(&request(), OWNER, &allowlist())
            .await
            .unwrap_err();
        assert!(matches!(error, SwapError::MalformedQuote(_)));
    }

    #[tokio::test]
    async fn calldata_that_is_not_hex_is_refused() {
        let mut body = quote_body(USDC, WETH, 1_000_000);
        body["priceRoute"]["data"] = json!("0xzz");
        let swap = service(body, MockReads::new());
        let error = swap
            .quote(&request(), OWNER, &allowlist())
            .await
            .unwrap_err();
        assert!(matches!(error, SwapError::MalformedQuote(_)));
    }

    #[tokio::test]
    async fn a_quote_without_an_output_amount_is_refused() {
        let mut body = quote_body(USDC, WETH, 1_000_000);
        body["priceRoute"]["destAmount"] = json!("0");
        let swap = service(body, MockReads::new());
        let error = swap
            .quote(&request(), OWNER, &allowlist())
            .await
            .unwrap_err();
        assert!(matches!(error, SwapError::MalformedQuote(_)));
    }

    #[tokio::test]
    async fn a_flat_body_with_hex_amounts_is_read() {
        let body = json!({
            "to": ROUTER,
            "calldata": "0x1234",
            "value": "0x0",
            "amountOut": "0x1e240",
            "srcAmount": "0xf4240",
            "srcToken": USDC,
            "destToken": WETH,
            "allowanceTarget": PROXY
        });
        let swap = service(body, MockReads::new());
        let quote = swap.quote(&request(), OWNER, &allowlist()).await.unwrap();
        assert_eq!(quote.buy_amount_raw, 123_456);
        assert_eq!(quote.data, "0x1234");
    }

    #[tokio::test]
    async fn solana_swaps_are_refused_before_any_quote() {
        let mut request = request();
        request.network = "solana".into();
        // A real Solana mint is not an EVM address; the network gate still has
        // to answer first, otherwise the refusal reads as a malformed request.
        request.sell_token = "So11111111111111111111111111111111111111112".into();
        request.buy_token = USDC.into();
        let swap = service(quote_body(USDC, WETH, 1_000_000), MockReads::new());
        let error = swap.quote(&request, OWNER, &allowlist()).await.unwrap_err();
        assert!(matches!(error, SwapError::NetworkUnsupported(ref n, _) if n == "solana"));
    }

    #[test]
    fn approval_calldata_is_the_standard_erc20_call() {
        let data = encode_approve(PROXY, 1_234).unwrap();
        assert_eq!(&data[..4], SELECTOR_APPROVE);
        assert_eq!(data.len(), 4 + 32 + 32);
        let spender = &data[4..36];
        assert!(spender[..12].iter().all(|byte| *byte == 0));
        assert_eq!(&spender[12..], &hex::decode(&PROXY[2..]).unwrap()[..]);
        let amount = &data[36..68];
        assert!(amount[..16].iter().all(|byte| *byte == 0));
        assert_eq!(&amount[16..], &1_234u128.to_be_bytes()[..]);
    }

    #[test]
    fn the_quote_source_is_shown_without_its_query() {
        let quoter = HttpQuoter::new(
            "https://api.example.com/prices?network={chain_id}&src={sell_token}&dst={buy_token}&amount={sell_amount}&key=secret"
                .into(),
        )
        .unwrap();
        assert_eq!(quoter.source(), "api.example.com");
    }

    #[test]
    fn a_template_that_does_not_name_the_trade_is_refused() {
        let error = HttpQuoter::new("https://api.example.com/prices".into()).unwrap_err();
        assert!(matches!(error, SwapError::InvalidRequest(_)));
    }

    #[test]
    fn a_request_with_the_same_token_on_both_sides_is_refused() {
        let mut request = request();
        request.buy_token = USDC.into();
        assert!(matches!(
            request.normalized(),
            Err(SwapError::InvalidRequest(_))
        ));
    }

    #[test]
    fn slippage_beyond_ten_percent_is_refused() {
        let mut request = request();
        request.slippage_bps = MAX_SLIPPAGE_BPS + 1;
        assert!(matches!(
            request.normalized(),
            Err(SwapError::InvalidRequest(_))
        ));
    }
}

use crate::types::Chain;
use serde::{Deserialize, Serialize};

/// Read-only multi-chain access.
///
/// The engine never holds a private key for on-chain interactions: balances and
/// metadata are read through public RPC endpoints, and anything that would
/// require a signature is refused here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenBalance {
    pub chain: String,
    pub asset: String,
    pub balance: f64,
    pub decimals: u32,
    pub symbol: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnchainAccount {
    pub chain: String,
    pub address: String,
    pub native_balance: f64,
    pub native_symbol: String,
    pub tokens: Vec<TokenBalance>,
    pub fetched_at_ms: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum OnchainError {
    #[error("chain not configured: {0}")]
    ChainNotConfigured(String),
    #[error("rpc error: {0}")]
    Rpc(String),
    #[error("invalid address: {0}")]
    InvalidAddress(String),
    #[error("invalid response: {0}")]
    InvalidResponse(String),
}

pub type OnchainResult<T> = Result<T, OnchainError>;

/// ERC-20 `balanceOf(address)` selector. Hard-coded because it is a constant of
/// the ERC-20 ABI.
const ERC20_BALANCE_OF_SELECTOR: &str = "70a08231";

/// ERC-20 `decimals()` selector.
const ERC20_DECIMALS_SELECTOR: &str = "313ce567";

/// ERC-20 `symbol()` selector.
const ERC20_SYMBOL_SELECTOR: &str = "95d89b41";

pub struct OnchainClient {
    client: reqwest::Client,
    solana_rpc: Option<String>,
    evm_rpcs: Vec<(String, String)>,
}

impl OnchainClient {
    pub fn new(
        solana_rpc: Option<String>,
        evm_rpcs: Vec<(String, String)>,
    ) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()?;
        Ok(Self {
            client,
            solana_rpc,
            evm_rpcs,
        })
    }

    pub fn chains(&self) -> Vec<String> {
        let mut chains = vec!["solana".to_string()];
        chains.extend(self.evm_rpcs.iter().map(|(name, _)| name.clone()));
        chains
    }

    pub async fn solana_account(&self, owner: &str) -> OnchainResult<OnchainAccount> {
        let rpc = self
            .solana_rpc
            .as_ref()
            .ok_or_else(|| OnchainError::ChainNotConfigured("solana".into()))?;
        if owner.trim().is_empty() {
            return Err(OnchainError::InvalidAddress(owner.to_string()));
        }

        let balance = self
            .rpc_call(
                rpc,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "getBalance",
                    "params": [owner, {"commitment": "confirmed"}]
                }),
            )
            .await?;
        let lamports = balance
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_u64())
            .ok_or_else(|| OnchainError::InvalidResponse("getBalance".into()))?;
        let native_balance = lamports as f64 / 1e9;

        let accounts = self
            .rpc_call(
                rpc,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "getTokenAccountsByOwner",
                    "params": [
                        owner,
                        {"programId": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"},
                        {"encoding": "jsonParsed"}
                    ]
                }),
            )
            .await?;

        let mut tokens = Vec::new();
        if let Some(list) = accounts
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_array())
        {
            for item in list {
                let info = item
                    .get("account")
                    .and_then(|a| a.get("data"))
                    .and_then(|d| d.get("parsed"))
                    .and_then(|p| p.get("info"));
                let Some(info) = info else { continue };
                let mint = info
                    .get("mint")
                    .and_then(|m| m.as_str())
                    .unwrap_or_default();
                let amount = info
                    .get("tokenAmount")
                    .and_then(|t| t.get("uiAmount"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let decimals = info
                    .get("tokenAmount")
                    .and_then(|t| t.get("decimals"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                tokens.push(TokenBalance {
                    chain: "solana".into(),
                    asset: mint.to_string(),
                    balance: amount,
                    decimals,
                    symbol: None,
                });
            }
        }

        Ok(OnchainAccount {
            chain: "solana".into(),
            address: owner.to_string(),
            native_balance,
            native_symbol: "SOL".into(),
            tokens,
            fetched_at_ms: crate::types::now_ms(),
        })
    }

    pub async fn evm_balance(&self, chain: &str, address: &str) -> OnchainResult<OnchainAccount> {
        let rpc = self.rpc_for(chain)?;
        if address.trim().is_empty() {
            return Err(OnchainError::InvalidAddress(address.to_string()));
        }

        let hex_balance = self
            .rpc_call(
                &rpc,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "eth_getBalance",
                    "params": [address, "latest"]
                }),
            )
            .await?;
        let raw = hex_balance
            .get("result")
            .and_then(|r| r.as_str())
            .ok_or_else(|| OnchainError::InvalidResponse("eth_getBalance".into()))?;
        let wei = u128::from_str_radix(raw.trim_start_matches("0x"), 16)
            .map_err(|e| OnchainError::InvalidResponse(e.to_string()))?;
        let native_balance = wei as f64 / 1e18;
        // Base's native gas token is ETH as well, so every EVM chain reports ETH.
        let native_symbol = "ETH";

        Ok(OnchainAccount {
            chain: chain.to_string(),
            address: address.to_string(),
            native_balance,
            native_symbol: native_symbol.into(),
            tokens: Vec::new(),
            fetched_at_ms: crate::types::now_ms(),
        })
    }

    /// ERC-20 balance through `eth_call` on `balanceOf`.
    ///
    /// The decimals come from the token itself. Assuming 18 would report a
    /// USDC balance a trillion times too large.
    pub async fn evm_token_balance(
        &self,
        chain: &str,
        token: &str,
        holder: &str,
    ) -> OnchainResult<TokenBalance> {
        let rpc = self.rpc_for(chain)?;
        let holder = holder.trim_start_matches("0x");
        if holder.len() != 40 || !holder.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(OnchainError::InvalidAddress(holder.to_string()));
        }
        let padded = format!("{:0>64}", holder);
        let call_data = format!("0x{}{}", ERC20_BALANCE_OF_SELECTOR, padded);

        let raw = self
            .evm_call(&rpc, token, &call_data)
            .await?
            .ok_or_else(|| OnchainError::InvalidResponse("eth_call".into()))?;
        let value = u128::from_str_radix(raw.trim_start_matches("0x"), 16)
            .map_err(|e| OnchainError::InvalidResponse(e.to_string()))?;

        let decimals = self.evm_decimals(&rpc, token).await.unwrap_or(18);
        let symbol = self.evm_symbol(&rpc, token).await;

        Ok(TokenBalance {
            chain: chain.to_string(),
            asset: token.to_string(),
            balance: value as f64 / 10_f64.powi(decimals as i32),
            decimals,
            symbol,
        })
    }

    /// The decimals of an ERC-20 token.
    ///
    /// A token that does not implement `decimals()` is rare but legal, so a
    /// failed call falls back to 18 rather than failing the whole read.
    pub async fn evm_decimals(&self, rpc: &str, token: &str) -> Option<u32> {
        let raw = self
            .evm_call(rpc, token, &format!("0x{ERC20_DECIMALS_SELECTOR}"))
            .await
            .ok()??;
        let value = u128::from_str_radix(raw.trim_start_matches("0x"), 16).ok()?;
        u32::try_from(value).ok()
    }

    /// The symbol of an ERC-20 token.
    pub async fn evm_symbol(&self, rpc: &str, token: &str) -> Option<String> {
        let raw = self
            .evm_call(rpc, token, &format!("0x{ERC20_SYMBOL_SELECTOR}"))
            .await
            .ok()??;
        decode_abi_string(&raw)
    }

    /// A read-only `eth_call`.
    ///
    /// Returns `None` when the node answered with no data, which is what a
    /// contract that does not implement the method does.
    async fn evm_call(&self, rpc: &str, to: &str, data: &str) -> OnchainResult<Option<String>> {
        let response = self
            .rpc_call(
                rpc,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "eth_call",
                    "params": [{"to": to, "data": data}, "latest"]
                }),
            )
            .await?;
        Ok(response
            .get("result")
            .and_then(|r| r.as_str())
            .map(|s| s.to_string()))
    }

    fn rpc_for(&self, chain: &str) -> OnchainResult<String> {
        self.evm_rpcs
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(chain))
            .map(|(_, url)| url.clone())
            .ok_or_else(|| OnchainError::ChainNotConfigured(chain.to_string()))
    }

    async fn rpc_call(
        &self,
        url: &str,
        payload: serde_json::Value,
    ) -> OnchainResult<serde_json::Value> {
        let response = self
            .client
            .post(url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| OnchainError::Rpc(e.to_string()))?;
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| OnchainError::InvalidResponse(e.to_string()))?;
        if let Some(error) = body.get("error") {
            return Err(OnchainError::Rpc(error.to_string()));
        }
        Ok(body)
    }
}

/// Decodes the string an ERC-20 `symbol()` call returns.
///
/// The ABI encodes a dynamic string as an offset word, a length word and the
/// bytes; some older tokens return a fixed `bytes32` instead, so both shapes
/// are handled. Every word is 32 bytes, so the offset lives in the first 64
/// hex characters — reading half of it yields zero and silently turns the
/// symbol into a run of NUL bytes.
fn decode_abi_string(raw: &str) -> Option<String> {
    let bytes = hex::decode(raw.trim().trim_start_matches("0x")).ok()?;
    if bytes.len() < 32 {
        return None;
    }
    let word = |index: usize| -> Option<usize> {
        let chunk = bytes.get(index * 32..(index + 1) * 32)?;
        let mut low = [0u8; 8];
        low.copy_from_slice(&chunk[24..]);
        Some(u64::from_be_bytes(low) as usize)
    };

    if word(0)? == 32 {
        let length = word(1)?;
        let data = bytes.get(64..64usize.checked_add(length)?)?;
        return String::from_utf8(data.to_vec()).ok();
    }
    // A `bytes32`: the whole word is the text, right padded with NULs.
    String::from_utf8(bytes[..32].to_vec())
        .ok()
        .map(|text| text.trim_end_matches('\0').trim().to_string())
        .filter(|text| !text.is_empty())
}

/// Normalizes the chain argument coming from an MCP call.
pub fn parse_chain(value: &str) -> Option<Chain> {
    match value.trim().to_ascii_lowercase().as_str() {
        "solana" | "sol" => Some(Chain::Solana),
        "ethereum" | "eth" | "base" | "evm" => Some(Chain::Evm),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_names_are_normalized() {
        assert!(matches!(parse_chain("SOL"), Some(Chain::Solana)));
        assert!(matches!(parse_chain("base"), Some(Chain::Evm)));
        assert!(parse_chain("bitcoin").is_none());
    }

    #[test]
    fn evm_address_validation_rejects_garbage() {
        let client = OnchainClient::new(None, vec![("ethereum".into(), "http://localhost".into())])
            .expect("client");
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let err = runtime.block_on(client.evm_token_balance("ethereum", "0xtoken", "0x123"));
        assert!(matches!(err, Err(OnchainError::InvalidAddress(_))));
    }

    /// The exact words mainnet USDC returns for `symbol()`.
    #[test]
    fn a_dynamic_abi_string_is_decoded() {
        let raw = "0x0000000000000000000000000000000000000000000000000000000000000020\
                   0000000000000000000000000000000000000000000000000000000000000004\
                   5553444300000000000000000000000000000000000000000000000000000000";
        assert_eq!(decode_abi_string(raw).as_deref(), Some("USDC"));
    }

    /// Older tokens such as MKR return a fixed `bytes32` instead.
    #[test]
    fn a_bytes32_abi_string_is_decoded() {
        let raw = "0x4d4b520000000000000000000000000000000000000000000000000000000000";
        assert_eq!(decode_abi_string(raw).as_deref(), Some("MKR"));
    }

    #[test]
    fn a_malformed_abi_string_is_refused() {
        assert!(decode_abi_string("0x").is_none());
        assert!(decode_abi_string("0xzz").is_none());
        // An offset that points past the end of the answer.
        let raw = "0x0000000000000000000000000000000000000000000000000000000000000020\
                   000000000000000000000000000000000000000000000000000000000000ffff";
        assert!(decode_abi_string(raw).is_none());
    }
}

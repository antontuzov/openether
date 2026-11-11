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
        let rpc = self
            .evm_rpcs
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(chain))
            .map(|(_, url)| url.clone())
            .ok_or_else(|| OnchainError::ChainNotConfigured(chain.to_string()))?;
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
    pub async fn evm_token_balance(
        &self,
        chain: &str,
        token: &str,
        holder: &str,
    ) -> OnchainResult<TokenBalance> {
        let rpc = self
            .evm_rpcs
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(chain))
            .map(|(_, url)| url.clone())
            .ok_or_else(|| OnchainError::ChainNotConfigured(chain.to_string()))?;
        let holder = holder.trim_start_matches("0x");
        if holder.len() != 40 || !holder.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(OnchainError::InvalidAddress(holder.to_string()));
        }
        let padded = format!("{:0>64}", holder);
        let call_data = format!("0x{}{}", ERC20_BALANCE_OF_SELECTOR, padded);

        let response = self
            .rpc_call(
                &rpc,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "eth_call",
                    "params": [{"to": token, "data": call_data}, "latest"]
                }),
            )
            .await?;
        let raw = response
            .get("result")
            .and_then(|r| r.as_str())
            .ok_or_else(|| OnchainError::InvalidResponse("eth_call".into()))?;
        let value = u128::from_str_radix(raw.trim_start_matches("0x"), 16)
            .map_err(|e| OnchainError::InvalidResponse(e.to_string()))?;

        Ok(TokenBalance {
            chain: chain.to_string(),
            asset: token.to_string(),
            balance: value as f64 / 1e18,
            decimals: 18,
            symbol: None,
        })
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
}

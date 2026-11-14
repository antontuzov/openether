//! Broadcasting a signed transaction to an EVM chain or to Solana.
//!
//! The wallet module has no networking code: it signs and hands back a
//! signature. This module is the only place that talks to a chain, and it
//! assembles the transaction *around* that signature so the bytes signed are
//! the bytes broadcast.
//!
//! # What a transfer has to get right
//!
//! * **Nonce** — an EVM account may only have one transaction per nonce. Two
//!   transfers prepared at once must not get the same one, and a transfer that
//!   fails to send must not leave a gap. Nonces are handed out from a lock and
//!   resynchronised with the node's pending count on every request.
//! * **Fees** — EIP-1559: a priority fee and a maximum fee that covers twice
//!   the current base fee. If the network's price is above the ceiling this
//!   process may pay, the transfer is refused instead of paying it.
//! * **Finality** — a transaction is not done when it is included. It is done
//!   when enough blocks sit on top of it. Until then a reorg can move it.
//! * **Reorgs** — a receipt that disappears, or reappears in a different
//!   block, takes the broadcast back to `reorged`; the reconciler keeps
//!   watching. On Solana only `finalized` counts, because `confirmed` can be
//!   rolled back.
//! * **Replacement** — a nonce used by another transaction without our receipt
//!   appearing means ours was replaced. It will never land, so it is dropped.
//!
//! # The three gates
//!
//! This module has no arming logic of its own. The caller proves the wallet is
//! armed for on-chain transfers, the wallet proves its own `dry_run == false`,
//! and a signature is verified against the address it claims before the
//! transaction is sent. A broadcaster on its own cannot move anything.

use crate::types::now_ms;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha3::{Digest, Keccak256};
use std::collections::HashMap;
use std::sync::Mutex;

/// How many blocks must sit on top of an EVM transaction before it is final.
pub const EVM_FINALITY_DEPTH: u64 = 12;
/// Solana's base fee is 5 000 lamports per signature.
pub const SOLANA_BASE_FEE_LAMPORTS: u64 = 5_000;
/// Extra gas granted on top of the node's estimate, in basis points.
pub const GAS_LIMIT_MARGIN_BPS: u64 = 2_000;
/// The floor for an EVM transfer, so a bad estimate cannot underfund it.
pub const MIN_TRANSFER_GAS: u64 = 21_000;
/// A plain transfer never gets close to this; it bounds a runaway estimate.
pub const MAX_TRANSFER_GAS: u64 = 200_000;
/// The Solana system program.
pub const SYSTEM_PROGRAM_ID: &str = "11111111111111111111111111111111";
/// `SystemInstruction::Transfer`.
const SYSTEM_TRANSFER_INDEX: u32 = 2;

/// Where a broadcast is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BroadcastState {
    /// Signed and about to be sent.
    Signed,
    /// The node accepted it and it has not reached finality.
    Submitted,
    /// It was included, then the block that held it was replaced.
    Reorged,
    /// Final. Enough blocks confirm it.
    Confirmed,
    /// It will never land: the nonce was replaced, or the Solana blockhash
    /// expired.
    Dropped,
    /// It landed and failed: an EVM revert, or a Solana instruction error.
    Failed,
}

impl BroadcastState {
    pub fn as_str(self) -> &'static str {
        match self {
            BroadcastState::Signed => "signed",
            BroadcastState::Submitted => "submitted",
            BroadcastState::Reorged => "reorged",
            BroadcastState::Confirmed => "confirmed",
            BroadcastState::Dropped => "dropped",
            BroadcastState::Failed => "failed",
        }
    }

    /// `true` when nothing more will change.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            BroadcastState::Confirmed | BroadcastState::Dropped | BroadcastState::Failed
        )
    }
}

/// One transfer the core signed, and what the chain said about it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Broadcast {
    pub id: String,
    /// The network: `ethereum`, `base` or `solana`.
    pub chain: String,
    pub from: String,
    pub to: String,
    /// EVM account nonce. Zero on Solana.
    #[serde(default)]
    pub nonce: u64,
    pub amount_raw: u64,
    /// The most the transaction may cost in fees.
    pub fee_limit_raw: u64,
    /// What it did cost, once a receipt is known.
    #[serde(default)]
    pub fee_paid_raw: u64,
    /// EVM transaction hash, or the Solana signature.
    pub tx_hash: String,
    /// The serialised transaction: hex on EVM, base64 on Solana.
    pub raw_tx: String,
    /// EVM block the receipt was seen in. Zero until then.
    #[serde(default)]
    pub block_number: u64,
    /// Hash of that block, so a reorg can be spotted.
    #[serde(default)]
    pub block_hash: String,
    /// For a Solana transfer: the block height after which its blockhash is
    /// no longer valid, so a missing transaction can be declared dropped.
    #[serde(default)]
    pub last_valid_block_height: u64,
    #[serde(default)]
    pub confirmations: u64,
    pub state: BroadcastState,
    /// Human-readable explanation of the current state, for the operator.
    pub note: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// A request to move coins out of the wallet.
#[derive(Debug, Clone)]
pub struct TransferRequest {
    /// The network label the operator configured, e.g. `ethereum` or `solana`.
    pub network: String,
    pub to: String,
    pub amount_raw: u64,
    /// A per-gas ceiling that only tightens the broadcaster's own.
    pub max_fee_per_gas_wei: Option<u128>,
}

/// Everything that can go wrong while preparing, sending or watching.
#[derive(Debug, thiserror::Error)]
pub enum BroadcastError {
    #[error("network not configured: {0}; set the matching RPC url")]
    NetworkNotConfigured(String),
    #[error("rpc error: {0}")]
    Rpc(String),
    #[error("invalid transfer: {0}")]
    InvalidRequest(String),
    #[error("the fee ceiling of {0} per gas is below the network's {1}")]
    FeeCeilingTooLow(String, String),
    #[error("the wallet signed as {0}, but the transfer is from {1}")]
    SignerMismatch(String, String),
    #[error("the node reported {0} for a transaction that hashes to {1}")]
    HashMismatch(String, String),
    #[error("the wallet refused to sign: {0}")]
    Signer(String),
}

/// Async JSON-RPC. Implemented over HTTP for real use and over canned answers
/// in tests.
#[async_trait::async_trait]
pub trait RpcTransport: Send + Sync {
    async fn call(&self, url: &str, method: &str, params: Value) -> Result<Value, BroadcastError>;
}

/// A JSON-RPC client for a public or private node.
pub struct HttpRpc {
    client: reqwest::Client,
    next_id: std::sync::atomic::AtomicU64,
}

impl HttpRpc {
    pub fn new() -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()?;
        Ok(Self {
            client,
            next_id: std::sync::atomic::AtomicU64::new(1),
        })
    }
}

#[async_trait::async_trait]
impl RpcTransport for HttpRpc {
    async fn call(&self, url: &str, method: &str, params: Value) -> Result<Value, BroadcastError> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let response = self
            .client
            .post(url)
            .json(&body)
            .send()
            .await
            .map_err(|e| BroadcastError::Rpc(format!("{method}: {e}")))?;
        let value: Value = response
            .json()
            .await
            .map_err(|e| BroadcastError::Rpc(format!("{method}: {e}")))?;
        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            return Err(BroadcastError::Rpc(format!("{method}: {message}")));
        }
        value
            .get("result")
            .cloned()
            .ok_or_else(|| BroadcastError::Rpc(format!("{method}: no result")))
    }
}

/// The signing half of a wallet, as the broadcaster needs it.
///
/// The key stays behind this trait. The broadcaster asks for an address and a
/// signature, and verifies that the signature recovers to the address it was
/// given before anything is sent.
pub trait TxSigner: Send + Sync {
    fn evm_address(&self) -> Result<String, BroadcastError>;
    /// `r || s || y_parity` over the EIP-1559 signing hash.
    fn sign_evm_digest(&self, digest: &[u8; 32]) -> Result<Vec<u8>, BroadcastError>;
    fn solana_address(&self) -> Result<String, BroadcastError>;
    fn sign_solana_message(&self, message: &[u8]) -> Result<[u8; 64], BroadcastError>;
}

/// The transaction builder plus the trackers that watch it.
pub struct Broadcaster {
    transport: std::sync::Arc<dyn RpcTransport>,
    evm_rpcs: HashMap<String, String>,
    evm_chain_ids: HashMap<String, u64>,
    solana_rpc: Option<String>,
    /// The most this process will pay per gas unit, in wei.
    max_fee_per_gas_wei: u128,
    /// The next nonce per `network:address`, so two transfers at once cannot
    /// collide.
    nonces: Mutex<HashMap<String, u64>>,
}

impl Broadcaster {
    pub fn new(
        transport: std::sync::Arc<dyn RpcTransport>,
        evm_rpcs: Vec<(String, String)>,
        solana_rpc: Option<String>,
        max_fee_per_gas_wei: u128,
    ) -> Self {
        let evm_chain_ids = evm_rpcs
            .iter()
            .map(|(name, _)| {
                let id = match name.as_str() {
                    "ethereum" => 1,
                    "base" => 8_453,
                    // An unknown EVM chain is refused rather than guessed: a
                    // wrong chain id produces a signature the chain rejects.
                    _ => 0,
                };
                (name.clone(), id)
            })
            .filter(|(_, id)| *id != 0)
            .collect();
        Self {
            transport,
            evm_rpcs: evm_rpcs.into_iter().collect(),
            evm_chain_ids,
            solana_rpc,
            max_fee_per_gas_wei,
            nonces: Mutex::new(HashMap::new()),
        }
    }

    /// `true` when at least one network has an RPC url.
    pub fn is_configured(&self) -> bool {
        self.solana_rpc.is_some() || !self.evm_rpcs.is_empty()
    }

    /// The networks this broadcaster can reach.
    pub fn networks(&self) -> Vec<String> {
        let mut networks: Vec<String> = self.evm_rpcs.keys().cloned().collect();
        if self.solana_rpc.is_some() {
            networks.push("solana".to_string());
        }
        networks.sort();
        networks
    }

    /// Prepares, signs, verifies and sends one transfer.
    pub async fn transfer(
        &self,
        request: &TransferRequest,
        signer: &dyn TxSigner,
    ) -> Result<Broadcast, BroadcastError> {
        if request.amount_raw == 0 {
            return Err(BroadcastError::InvalidRequest(
                "the amount must be greater than zero".into(),
            ));
        }
        if request.to.trim().is_empty() {
            return Err(BroadcastError::InvalidRequest(
                "the destination must not be empty".into(),
            ));
        }
        if request.network == "solana" {
            self.transfer_solana(request, signer).await
        } else {
            self.transfer_evm(request, signer).await
        }
    }

    /// Re-reads one broadcast and advances its state.
    pub async fn refresh(&self, broadcast: &mut Broadcast) -> Result<(), BroadcastError> {
        if broadcast.state.is_terminal() {
            return Ok(());
        }
        if broadcast.chain == "solana" {
            self.refresh_solana(broadcast).await
        } else {
            self.refresh_evm(broadcast).await
        }
    }

    fn rpc_for(&self, network: &str) -> Result<&str, BroadcastError> {
        if network == "solana" {
            return self
                .solana_rpc
                .as_deref()
                .ok_or_else(|| BroadcastError::NetworkNotConfigured(network.to_string()));
        }
        self.evm_rpcs
            .get(network)
            .map(String::as_str)
            .ok_or_else(|| BroadcastError::NetworkNotConfigured(network.to_string()))
    }

    // --- EVM -------------------------------------------------------------

    async fn transfer_evm(
        &self,
        request: &TransferRequest,
        signer: &dyn TxSigner,
    ) -> Result<Broadcast, BroadcastError> {
        let rpc = self.rpc_for(&request.network)?.to_string();
        let chain_id = *self
            .evm_chain_ids
            .get(&request.network)
            .ok_or_else(|| BroadcastError::NetworkNotConfigured(request.network.clone()))?;
        let from = signer.evm_address()?;
        let to = parse_evm_address(&request.to)?;

        let nonce = self.next_nonce(&rpc, &request.network, &from).await?;

        // Gas: the node's estimate with a margin, bounded both ways.
        let estimate = self
            .call_evm(
                &rpc,
                "eth_estimateGas",
                json!([{
                    "from": from,
                    "to": request.to,
                    "value": format!("0x{:x}", request.amount_raw),
                }]),
            )
            .await?;
        let estimate = parse_quantity(&estimate, "eth_estimateGas")?;
        let gas_limit = gas_limit_from(estimate);

        let base_fee = self
            .call_evm(&rpc, "eth_getBlockByNumber", json!(["latest", false]))
            .await
            .and_then(|block| {
                block
                    .get("baseFeePerGas")
                    .ok_or_else(|| BroadcastError::Rpc("the block has no baseFeePerGas".into()))
                    .and_then(|value| parse_quantity(value, "baseFeePerGas"))
            })? as u128;
        let tip = self.evm_tip(&rpc).await? as u128;
        let ceiling = self.max_fee_per_gas_wei.min(request.max_fee_per_gas_wei());
        let fees = choose_fees(base_fee, tip, Some(ceiling))?;

        let tx = EvmTransaction {
            chain_id,
            nonce,
            max_priority_fee_per_gas: fees.max_priority,
            max_fee_per_gas: fees.max_fee,
            gas_limit,
            to,
            value: request.amount_raw as u128,
            data: Vec::new(),
        };
        let digest = tx.signing_hash();
        let signature = signer.sign_evm_digest(&digest)?;
        let signature: [u8; 65] = signature.try_into().map_err(|_| {
            BroadcastError::InvalidRequest("an evm signature must be 65 bytes".into())
        })?;
        verify_evm_signature(&digest, &signature, &from)?;
        let raw = tx.encode_signed(&signature);
        let expected_hash = format!("0x{}", hex::encode(tx.hash(&signature)));
        let reported = self
            .call_evm(
                &rpc,
                "eth_sendRawTransaction",
                json!([format!("0x{}", hex::encode(&raw))]),
            )
            .await?
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| BroadcastError::Rpc("eth_sendRawTransaction returned no hash".into()))?;
        if !reported.eq_ignore_ascii_case(&expected_hash) {
            self.release_nonce(&request.network, &from, nonce);
            return Err(BroadcastError::HashMismatch(reported, expected_hash));
        }

        let now = now_ms();
        Ok(Broadcast {
            id: uuid::Uuid::new_v4().to_string(),
            chain: request.network.clone(),
            from,
            to: request.to.clone(),
            nonce,
            amount_raw: request.amount_raw,
            fee_limit_raw: fees
                .max_fee
                .saturating_mul(gas_limit as u128)
                .min(u64::MAX as u128) as u64,
            fee_paid_raw: 0,
            tx_hash: expected_hash,
            raw_tx: format!("0x{}", hex::encode(&raw)),
            block_number: 0,
            block_hash: String::new(),
            last_valid_block_height: 0,
            confirmations: 0,
            state: BroadcastState::Submitted,
            note: format!(
                "sent at nonce {nonce}; {} blocks make it final",
                EVM_FINALITY_DEPTH
            ),
            created_at_ms: now,
            updated_at_ms: now,
        })
    }

    async fn evm_tip(&self, rpc: &str) -> Result<u64, BroadcastError> {
        if let Ok(value) = self
            .call_evm(rpc, "eth_maxPriorityFeePerGas", json!([]))
            .await
        {
            return parse_quantity(&value, "eth_maxPriorityFeePerGas");
        }
        // A node without the EIP-1559 tip endpoint still quotes a gas price.
        let price = self
            .call_evm(rpc, "eth_gasPrice", json!([]))
            .await
            .and_then(|value| parse_quantity(&value, "eth_gasPrice"))?;
        Ok(price.min(1_000_000_000))
    }

    async fn next_nonce(
        &self,
        rpc: &str,
        network: &str,
        from: &str,
    ) -> Result<u64, BroadcastError> {
        let pending = self
            .call_evm(rpc, "eth_getTransactionCount", json!([from, "pending"]))
            .await
            .and_then(|value| parse_quantity(&value, "eth_getTransactionCount"))?;
        let key = format!("{network}:{from}");
        let mut nonces = self.nonces.lock().unwrap_or_else(|e| e.into_inner());
        // The node is the floor: if it has caught up past our cache, trust it.
        let next = nonces.entry(key).or_insert(pending);
        if *next < pending {
            *next = pending;
        }
        let nonce = *next;
        *next += 1;
        Ok(nonce)
    }

    fn release_nonce(&self, network: &str, from: &str, nonce: u64) {
        let key = format!("{network}:{from}");
        let mut nonces = self.nonces.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(next) = nonces.get_mut(&key) {
            // Only give the nonce back if nothing was handed out after it.
            if *next == nonce + 1 {
                *next = nonce;
            }
        }
    }

    async fn refresh_evm(&self, broadcast: &mut Broadcast) -> Result<(), BroadcastError> {
        let rpc = self.rpc_for(&broadcast.chain)?.to_string();
        broadcast.updated_at_ms = now_ms();
        let receipt = self
            .call_evm(
                &rpc,
                "eth_getTransactionReceipt",
                json!([broadcast.tx_hash]),
            )
            .await?;

        if receipt.is_null() {
            if broadcast.block_number > 0 {
                // We saw it in a block and now we do not: that block is gone.
                broadcast.state = BroadcastState::Reorged;
                broadcast.block_number = 0;
                broadcast.block_hash.clear();
                broadcast.confirmations = 0;
                broadcast.note =
                    "the receipt disappeared: the block that held it was replaced".into();
                return Ok(());
            }
            let latest = self
                .call_evm(
                    &rpc,
                    "eth_getTransactionCount",
                    json!([broadcast.from, "latest"]),
                )
                .await
                .and_then(|value| parse_quantity(&value, "eth_getTransactionCount"))?;
            if latest > broadcast.nonce {
                broadcast.state = BroadcastState::Dropped;
                broadcast.note = "another transaction used the nonce; this one was replaced".into();
            }
            return Ok(());
        }

        let block_number = receipt
            .get("blockNumber")
            .ok_or_else(|| BroadcastError::Rpc("the receipt has no blockNumber".into()))
            .and_then(|value| parse_quantity(value, "blockNumber"))?;
        let block_hash = receipt
            .get("blockHash")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // The transaction was in one block and is now in another: the chain
        // reorged and this transaction was re-included.
        let moved = broadcast.block_number > 0 && broadcast.block_hash != block_hash;
        let gas_used = receipt
            .get("gasUsed")
            .ok_or_else(|| BroadcastError::Rpc("the receipt has no gasUsed".into()))
            .and_then(|value| parse_quantity(value, "gasUsed"))?;
        let price = receipt
            .get("effectiveGasPrice")
            .ok_or_else(|| BroadcastError::Rpc("the receipt has no effectiveGasPrice".into()))
            .and_then(|value| parse_quantity(value, "effectiveGasPrice"))?;
        broadcast.block_number = block_number;
        broadcast.block_hash = block_hash;
        broadcast.fee_paid_raw = (gas_used as u128)
            .saturating_mul(price as u128)
            .min(u64::MAX as u128) as u64;

        let head = self
            .call_evm(&rpc, "eth_blockNumber", json!([]))
            .await
            .and_then(|value| parse_quantity(&value, "eth_blockNumber"))?;
        broadcast.confirmations = head.saturating_sub(block_number).saturating_add(1);

        let succeeded = receipt
            .get("status")
            .and_then(Value::as_str)
            .map(|status| status != "0x0")
            .unwrap_or(true);
        if !succeeded {
            broadcast.state = BroadcastState::Failed;
            broadcast.note = format!(
                "reverted in block {block_number}; the fee of {} was still paid",
                broadcast.fee_paid_raw
            );
        } else if moved {
            broadcast.state = BroadcastState::Reorged;
            broadcast.note = format!(
                "the transaction moved to block {block_number}: the chain reorged and re-included it"
            );
        } else if broadcast.confirmations >= EVM_FINALITY_DEPTH {
            broadcast.state = BroadcastState::Confirmed;
            broadcast.note = format!(
                "final: {} confirmations, fee {}",
                broadcast.confirmations, broadcast.fee_paid_raw
            );
        } else {
            broadcast.state = BroadcastState::Submitted;
            broadcast.note = format!(
                "{} of {} confirmations",
                broadcast.confirmations, EVM_FINALITY_DEPTH
            );
        }
        Ok(())
    }

    async fn call_evm(
        &self,
        rpc: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, BroadcastError> {
        self.transport.call(rpc, method, params).await
    }

    // --- Solana ----------------------------------------------------------

    async fn transfer_solana(
        &self,
        request: &TransferRequest,
        signer: &dyn TxSigner,
    ) -> Result<Broadcast, BroadcastError> {
        let rpc = self.rpc_for("solana")?.to_string();
        let from = signer.solana_address()?;
        let from_key = decode_solana_address(&from)?;
        let to_key = decode_solana_address(&request.to)?;

        let latest = self
            .transport
            .call(
                &rpc,
                "getLatestBlockhash",
                json!([{"commitment": "confirmed"}]),
            )
            .await?;
        let value = latest
            .get("value")
            .ok_or_else(|| BroadcastError::Rpc("getLatestBlockhash returned no value".into()))?;
        let blockhash_str = value
            .get("blockhash")
            .and_then(Value::as_str)
            .ok_or_else(|| BroadcastError::Rpc("the blockhash is missing".into()))?;
        let last_valid_block_height = value
            .get("lastValidBlockHeight")
            .and_then(Value::as_u64)
            .ok_or_else(|| BroadcastError::Rpc("lastValidBlockHeight is missing".into()))?;
        let blockhash = decode_solana_address(blockhash_str)?;

        let message = compile_transfer_message(&from_key, &to_key, &blockhash, request.amount_raw);

        // The fee is deterministic for a one-signature transfer, but the node
        // knows the current schedule, so ask it and fall back to the base fee.
        let fee = self
            .transport
            .call(
                &rpc,
                "getFeeForMessage",
                json!([
                    base64_encode(&message),
                    {"commitment": "confirmed"}
                ]),
            )
            .await
            .ok()
            .and_then(|answer| answer.get("value").and_then(Value::as_u64))
            .unwrap_or(SOLANA_BASE_FEE_LAMPORTS);

        let signature = signer.sign_solana_message(&message)?;
        verify_solana_signature(&message, &signature, &from_key)?;

        let mut raw = Vec::with_capacity(1 + 64 + message.len());
        raw.push(1); // one signature
        raw.extend_from_slice(&signature);
        raw.extend_from_slice(&message);

        let expected_signature = bs58::encode(signature).into_string();
        let reported = self
            .transport
            .call(
                &rpc,
                "sendTransaction",
                json!([
                    base64_encode(&raw),
                    {"encoding": "base64", "preflightCommitment": "confirmed"}
                ]),
            )
            .await?
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| BroadcastError::Rpc("sendTransaction returned no signature".into()))?;
        if reported != expected_signature {
            return Err(BroadcastError::HashMismatch(reported, expected_signature));
        }

        let now = now_ms();
        Ok(Broadcast {
            id: uuid::Uuid::new_v4().to_string(),
            chain: "solana".into(),
            from,
            to: request.to.clone(),
            nonce: 0,
            amount_raw: request.amount_raw,
            fee_limit_raw: fee,
            fee_paid_raw: 0,
            tx_hash: expected_signature,
            raw_tx: base64_encode(&raw),
            block_number: 0,
            block_hash: String::new(),
            last_valid_block_height,
            confirmations: 0,
            state: BroadcastState::Submitted,
            note: "sent; only a finalized slot makes it final".into(),
            created_at_ms: now,
            updated_at_ms: now,
        })
    }

    async fn refresh_solana(&self, broadcast: &mut Broadcast) -> Result<(), BroadcastError> {
        let rpc = self.rpc_for("solana")?.to_string();
        broadcast.updated_at_ms = now_ms();
        let statuses = self
            .transport
            .call(
                &rpc,
                "getSignatureStatuses",
                json!([[broadcast.tx_hash], {"searchTransactionHistory": true}]),
            )
            .await?;
        let status = statuses
            .get("value")
            .and_then(Value::as_array)
            .and_then(|values| values.first())
            .cloned()
            .unwrap_or(Value::Null);

        if status.is_null() {
            // A transaction whose blockhash expired can never land.
            if broadcast.last_valid_block_height > 0 {
                let height = self
                    .transport
                    .call(&rpc, "getBlockHeight", json!([{"commitment": "confirmed"}]))
                    .await
                    .and_then(|value| {
                        value.as_u64().ok_or_else(|| {
                            BroadcastError::Rpc("getBlockHeight is not a number".into())
                        })
                    })?;
                if height > broadcast.last_valid_block_height {
                    broadcast.state = BroadcastState::Dropped;
                    broadcast.note = format!(
                        "the blockhash expired at block {} (now {height}) and the transaction never landed",
                        broadcast.last_valid_block_height
                    );
                }
            }
            return Ok(());
        }

        if let Some(error) = status.get("err").filter(|error| !error.is_null()) {
            broadcast.state = BroadcastState::Failed;
            broadcast.fee_paid_raw = broadcast.fee_limit_raw;
            broadcast.note = format!("the transaction failed on chain: {error}");
            return Ok(());
        }

        // `confirmed` can still be rolled back; only `finalized` is final.
        match status.get("confirmationStatus").and_then(Value::as_str) {
            Some("finalized") => {
                broadcast.state = BroadcastState::Confirmed;
                broadcast.confirmations = status
                    .get("confirmations")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                broadcast.fee_paid_raw = broadcast.fee_limit_raw;
                broadcast.note = format!("finalized; fee {}", broadcast.fee_paid_raw);
            }
            Some("confirmed") => {
                broadcast.state = BroadcastState::Submitted;
                broadcast.confirmations = 1;
                broadcast.note = "confirmed, waiting for a finalized slot".into();
            }
            _ => {
                broadcast.state = BroadcastState::Submitted;
                broadcast.note = "processed, not yet confirmed".into();
            }
        }
        Ok(())
    }
}

impl TransferRequest {
    fn max_fee_per_gas_wei(&self) -> u128 {
        self.max_fee_per_gas_wei.unwrap_or(u128::MAX)
    }
}

/// The fee pair for an EIP-1559 transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvmFees {
    pub max_priority: u128,
    pub max_fee: u128,
}

/// Picks the fee pair and refuses to exceed the ceiling.
///
/// The maximum fee covers twice the current base fee, so the transaction still
/// lands if the base fee rises. If that number is above what this process is
/// allowed to pay, the transfer stops here: paying it would be a surprise.
pub fn choose_fees(
    base_fee: u128,
    tip: u128,
    ceiling: Option<u128>,
) -> Result<EvmFees, BroadcastError> {
    let max_fee = base_fee.saturating_mul(2).saturating_add(tip);
    if let Some(ceiling) = ceiling {
        if max_fee > ceiling {
            return Err(BroadcastError::FeeCeilingTooLow(
                ceiling.to_string(),
                max_fee.to_string(),
            ));
        }
    }
    Ok(EvmFees {
        max_priority: tip,
        max_fee,
    })
}

/// Adds the safety margin and clamps to the transfer bounds.
pub fn gas_limit_from(estimate: u64) -> u64 {
    let with_margin =
        estimate.saturating_add(estimate.saturating_mul(GAS_LIMIT_MARGIN_BPS) / 10_000);
    with_margin.clamp(MIN_TRANSFER_GAS, MAX_TRANSFER_GAS)
}

/// An EIP-1559 transaction, ready to sign.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvmTransaction {
    pub chain_id: u64,
    pub nonce: u64,
    pub max_priority_fee_per_gas: u128,
    pub max_fee_per_gas: u128,
    pub gas_limit: u64,
    pub to: [u8; 20],
    pub value: u128,
    pub data: Vec<u8>,
}

impl EvmTransaction {
    /// The field list, in the order EIP-1559 defines it.
    fn unsigned_fields(&self) -> Vec<Vec<u8>> {
        vec![
            rlp_uint(self.chain_id as u128),
            rlp_uint(self.nonce as u128),
            rlp_uint(self.max_priority_fee_per_gas),
            rlp_uint(self.max_fee_per_gas),
            rlp_uint(self.gas_limit as u128),
            rlp_bytes(&self.to),
            rlp_uint(self.value),
            rlp_bytes(&self.data),
            rlp_list(&[]),
        ]
    }

    /// `keccak256(0x02 || rlp(unsigned fields))`.
    pub fn signing_hash(&self) -> [u8; 32] {
        let mut encoded = vec![0x02];
        encoded.extend_from_slice(&rlp_list(&self.unsigned_fields()));
        Keccak256::digest(&encoded).into()
    }

    /// `0x02 || rlp(every field including the signature)`.
    pub fn encode_signed(&self, signature: &[u8; 65]) -> Vec<u8> {
        let mut r = [0u8; 32];
        r.copy_from_slice(&signature[..32]);
        let mut s = [0u8; 32];
        s.copy_from_slice(&signature[32..64]);
        let mut fields = self.unsigned_fields();
        fields.push(rlp_uint(signature[64] as u128));
        fields.push(rlp_bytes(trim_leading_zeroes(&r)));
        fields.push(rlp_bytes(trim_leading_zeroes(&s)));
        let mut encoded = vec![0x02];
        encoded.extend_from_slice(&rlp_list(&fields));
        encoded
    }

    /// The transaction hash: keccak of the serialised signed transaction.
    pub fn hash(&self, signature: &[u8; 65]) -> [u8; 32] {
        Keccak256::digest(self.encode_signed(signature)).into()
    }
}

/// Minimal big-endian RLP for a non-negative integer. Zero is the empty
/// string, per the spec.
pub fn rlp_uint(value: u128) -> Vec<u8> {
    if value == 0 {
        return rlp_bytes(&[]);
    }
    let bytes = value.to_be_bytes();
    let first = bytes.iter().position(|byte| *byte != 0).unwrap_or(15);
    rlp_bytes(&bytes[first..])
}

/// RLP for a byte string.
pub fn rlp_bytes(data: &[u8]) -> Vec<u8> {
    if data.len() == 1 && data[0] < 0x80 {
        return vec![data[0]];
    }
    let mut out = rlp_length_prefix(data.len(), 0x80);
    out.extend_from_slice(data);
    out
}

/// RLP for a list of already-encoded items.
pub fn rlp_list(items: &[Vec<u8>]) -> Vec<u8> {
    let payload: Vec<u8> = items.iter().flatten().copied().collect();
    let mut out = rlp_length_prefix(payload.len(), 0xc0);
    out.extend_from_slice(&payload);
    out
}

fn rlp_length_prefix(len: usize, offset: u8) -> Vec<u8> {
    if len < 56 {
        return vec![offset + len as u8];
    }
    let len_bytes = len.to_be_bytes();
    let first = len_bytes
        .iter()
        .position(|byte| *byte != 0)
        .unwrap_or(len_bytes.len() - 1);
    let length = &len_bytes[first..];
    let mut out = vec![offset + 55 + length.len() as u8];
    out.extend_from_slice(length);
    out
}

fn trim_leading_zeroes(value: &[u8; 32]) -> &[u8] {
    let first = value.iter().position(|byte| *byte != 0).unwrap_or(32);
    &value[first..]
}

/// `[header, account keys, blockhash, instructions]` for a system transfer.
pub fn compile_transfer_message(
    from: &[u8; 32],
    to: &[u8; 32],
    blockhash: &[u8; 32],
    lamports: u64,
) -> Vec<u8> {
    // The system program id is the all-zero public key.
    let system = [0u8; 32];
    let mut message = Vec::with_capacity(3 + 1 + 96 + 32 + 1 + 1 + 1 + 2 + 1 + 12);
    message.extend_from_slice(&[1, 0, 1]); // one signer, no readonly signed, one readonly unsigned
    message.push(3); // account keys
    message.extend_from_slice(from);
    message.extend_from_slice(to);
    message.extend_from_slice(&system);
    message.extend_from_slice(blockhash);
    message.push(1); // one instruction
    message.push(2); // program: the system program, index 2
    message.push(2); // accounts: from and to
    message.push(0);
    message.push(1);
    message.push(12); // data length
    message.extend_from_slice(&SYSTEM_TRANSFER_INDEX.to_le_bytes());
    message.extend_from_slice(&lamports.to_le_bytes());
    message
}

fn parse_evm_address(value: &str) -> Result<[u8; 20], BroadcastError> {
    let trimmed = value.trim();
    let hex_part = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    let bytes = hex::decode(hex_part)
        .map_err(|_| BroadcastError::InvalidRequest(format!("not a hex address: {value}")))?;
    bytes
        .try_into()
        .map_err(|_| BroadcastError::InvalidRequest(format!("an evm address is 20 bytes: {value}")))
}

fn decode_solana_address(value: &str) -> Result<[u8; 32], BroadcastError> {
    let bytes = bs58::decode(value.trim())
        .into_vec()
        .map_err(|_| BroadcastError::InvalidRequest(format!("not a base58 address: {value}")))?;
    bytes.try_into().map_err(|_| {
        BroadcastError::InvalidRequest(format!("a solana address is 32 bytes: {value}"))
    })
}

fn parse_quantity(value: &Value, method: &str) -> Result<u64, BroadcastError> {
    let text = value
        .as_str()
        .ok_or_else(|| BroadcastError::Rpc(format!("{method} did not answer with a quantity")))?;
    u64::from_str_radix(text.trim_start_matches("0x"), 16)
        .map_err(|_| BroadcastError::Rpc(format!("{method} answered {text}, not a quantity")))
}

fn verify_evm_signature(
    digest: &[u8; 32],
    signature: &[u8; 65],
    expected: &str,
) -> Result<(), BroadcastError> {
    use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};

    let mut r = [0u8; 32];
    r.copy_from_slice(&signature[..32]);
    let mut s = [0u8; 32];
    s.copy_from_slice(&signature[32..64]);
    let typed = Signature::from_scalars(r, s)
        .map_err(|_| BroadcastError::Signer("the evm signature is malformed".into()))?;
    let recovery = RecoveryId::from_byte(signature[64])
        .ok_or_else(|| BroadcastError::Signer("the evm recovery id is not 0 or 1".into()))?;
    let key = VerifyingKey::recover_from_prehash(digest, &typed, recovery)
        .map_err(|_| BroadcastError::Signer("the evm signature does not recover".into()))?;
    let point = key.to_encoded_point(false);
    let address = evm_address_from_public(point.as_bytes());
    if !address.eq_ignore_ascii_case(expected) {
        return Err(BroadcastError::SignerMismatch(
            address,
            expected.to_string(),
        ));
    }
    Ok(())
}

fn evm_address_from_public(uncompressed: &[u8]) -> String {
    let hash = Keccak256::digest(&uncompressed[1..]);
    format!("0x{}", hex::encode(&hash[12..]))
}

fn verify_solana_signature(
    message: &[u8],
    signature: &[u8; 64],
    public_key: &[u8; 32],
) -> Result<(), BroadcastError> {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    let key = VerifyingKey::from_bytes(public_key)
        .map_err(|_| BroadcastError::Signer("the solana public key is invalid".into()))?;
    let signature = Signature::from_bytes(signature);
    key.verify(message, &signature)
        .map_err(|_| BroadcastError::Signer("the solana signature does not verify".into()))
}

fn base64_encode(data: &[u8]) -> String {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;
    STANDARD.encode(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use k256::ecdsa::SigningKey;
    use std::collections::VecDeque;
    use std::sync::Arc;

    const EVM_RPC: &str = "http://evm.test";
    const SOL_RPC: &str = "http://sol.test";

    /// Canned JSON-RPC answers, one queue per method.
    struct MockRpc {
        answers: Mutex<HashMap<String, VecDeque<Value>>>,
    }

    impl MockRpc {
        fn new() -> Self {
            Self {
                answers: Mutex::new(HashMap::new()),
            }
        }

        fn queue(&self, method: &str, value: Value) {
            self.answers
                .lock()
                .unwrap()
                .entry(method.to_string())
                .or_default()
                .push_back(value);
        }
    }

    #[async_trait::async_trait]
    impl RpcTransport for MockRpc {
        async fn call(
            &self,
            _url: &str,
            method: &str,
            params: Value,
        ) -> Result<Value, BroadcastError> {
            // A real node echoes the hash of the bytes it was handed, and the
            // first signature of a Solana transaction. Both can be computed
            // from the request, so the mock does the same.
            if method == "eth_sendRawTransaction" {
                let raw = params[0].as_str().unwrap_or_default();
                let bytes = hex::decode(raw.trim_start_matches("0x"))
                    .map_err(|e| BroadcastError::Rpc(e.to_string()))?;
                let hash: [u8; 32] = Keccak256::digest(&bytes).into();
                return Ok(json!(format!("0x{}", hex::encode(hash))));
            }
            if method == "sendTransaction" {
                use base64::engine::general_purpose::STANDARD;
                use base64::Engine as _;
                let raw = STANDARD
                    .decode(params[0].as_str().unwrap_or_default())
                    .map_err(|e| BroadcastError::Rpc(e.to_string()))?;
                let signature = raw.get(1..65).unwrap_or_default();
                return Ok(json!(bs58::encode(signature).into_string()));
            }
            let mut answers = self.answers.lock().unwrap();
            let queue = answers
                .get_mut(method)
                .unwrap_or_else(|| panic!("no answer queued for {method}"));
            Ok(queue.pop_front().unwrap_or(Value::Null))
        }
    }

    /// A signer with real keys, so the signatures in the tests recover.
    struct TestSigner {
        evm: SigningKey,
        solana: ed25519_dalek::SigningKey,
    }

    impl TestSigner {
        fn new() -> Self {
            Self {
                evm: SigningKey::from_slice(&[3u8; 32]).unwrap(),
                solana: ed25519_dalek::SigningKey::from_bytes(&[5u8; 32]),
            }
        }

        fn evm_address(&self) -> String {
            let point = self.evm.verifying_key().to_encoded_point(false);
            evm_address_from_public(point.as_bytes())
        }
    }

    impl TxSigner for TestSigner {
        fn evm_address(&self) -> Result<String, BroadcastError> {
            Ok(TestSigner::evm_address(self))
        }

        fn sign_evm_digest(&self, digest: &[u8; 32]) -> Result<Vec<u8>, BroadcastError> {
            let (signature, recovery) = self
                .evm
                .sign_prehash_recoverable(digest)
                .map_err(|_| BroadcastError::Signer("could not sign".into()))?;
            let mut out = [0u8; 65];
            out[..32].copy_from_slice(&signature.r().to_bytes());
            out[32..64].copy_from_slice(&signature.s().to_bytes());
            out[64] = recovery.to_byte();
            Ok(out.to_vec())
        }

        fn solana_address(&self) -> Result<String, BroadcastError> {
            Ok(bs58::encode(self.solana.verifying_key().to_bytes()).into_string())
        }

        fn sign_solana_message(&self, message: &[u8]) -> Result<[u8; 64], BroadcastError> {
            use ed25519_dalek::Signer;
            Ok(self.solana.sign(message).to_bytes())
        }
    }

    fn test_broadcaster(mock: Arc<MockRpc>) -> Broadcaster {
        Broadcaster::new(
            mock,
            vec![("ethereum".into(), EVM_RPC.into())],
            Some(SOL_RPC.into()),
            u128::MAX,
        )
    }

    fn request() -> TransferRequest {
        TransferRequest {
            network: "ethereum".into(),
            to: "0x000000000000000000000000000000000000dEaD".into(),
            amount_raw: 1_000_000_000_000_000,
            max_fee_per_gas_wei: None,
        }
    }

    #[test]
    fn rlp_encodes_the_way_the_spec_says() {
        assert_eq!(rlp_bytes(b"dog"), hex::decode("83646f67").unwrap());
        assert_eq!(rlp_bytes(&[]), vec![0x80]);
        assert_eq!(rlp_uint(0), vec![0x80]);
        assert_eq!(rlp_uint(15), vec![0x0f]);
        assert_eq!(rlp_uint(1024), hex::decode("820400").unwrap());
        assert_eq!(rlp_list(&[]), vec![0xc0]);
        assert_eq!(
            rlp_list(&[rlp_bytes(b"cat"), rlp_bytes(b"dog")]),
            hex::decode("c88363617483646f67").unwrap()
        );
    }

    #[test]
    fn an_eip1559_transaction_hashes_and_recovers() {
        let signer = TestSigner::new();
        let tx = EvmTransaction {
            chain_id: 1,
            nonce: 9,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 20_000_000_000,
            gas_limit: 21_000,
            to: [0x11; 20],
            value: 1_000,
            data: Vec::new(),
        };
        let digest = tx.signing_hash();
        assert_eq!(digest.len(), 32);

        let signature: [u8; 65] = signer.sign_evm_digest(&digest).unwrap().try_into().unwrap();
        verify_evm_signature(&digest, &signature, &signer.evm_address()).unwrap();

        let raw = tx.encode_signed(&signature);
        assert_eq!(raw[0], 0x02);
        // The hash is taken over the serialised signed transaction.
        let expected: [u8; 32] = Keccak256::digest(&raw).into();
        assert_eq!(tx.hash(&signature), expected);

        // A signature from another key does not recover to this address.
        let other = SigningKey::from_slice(&[9u8; 32]).unwrap();
        let (other_sig, other_recovery) = other.sign_prehash_recoverable(&digest).unwrap();
        let mut wrong = [0u8; 65];
        wrong[..32].copy_from_slice(&other_sig.r().to_bytes());
        wrong[32..64].copy_from_slice(&other_sig.s().to_bytes());
        wrong[64] = other_recovery.to_byte();
        assert!(matches!(
            verify_evm_signature(&digest, &wrong, &signer.evm_address()),
            Err(BroadcastError::SignerMismatch(_, _))
        ));
    }

    #[test]
    fn the_fee_ceiling_refuses_a_spike() {
        // Twice a 100 gwei base plus a 1 gwei tip is 201 gwei.
        let fees = choose_fees(100_000_000_000, 1_000_000_000, Some(250_000_000_000)).unwrap();
        assert_eq!(fees.max_fee, 201_000_000_000);
        assert!(matches!(
            choose_fees(100_000_000_000, 1_000_000_000, Some(200_000_000_000)),
            Err(BroadcastError::FeeCeilingTooLow(_, _))
        ));
    }

    #[test]
    fn the_gas_limit_has_a_floor_and_a_ceiling() {
        assert_eq!(gas_limit_from(21_000), 25_200);
        assert_eq!(gas_limit_from(1), MIN_TRANSFER_GAS);
        assert_eq!(gas_limit_from(10_000_000), MAX_TRANSFER_GAS);
    }

    #[tokio::test]
    async fn an_evm_transfer_is_estimated_signed_and_sent() {
        let mock = Arc::new(MockRpc::new());
        mock.queue("eth_getTransactionCount", json!("0x5"));
        mock.queue("eth_estimateGas", json!("0x5208"));
        mock.queue(
            "eth_getBlockByNumber",
            json!({"baseFeePerGas": "0x3b9aca00"}),
        );
        mock.queue("eth_maxPriorityFeePerGas", json!("0x3b9aca00"));
        let broadcaster = test_broadcaster(mock.clone());
        let signer = TestSigner::new();

        let broadcast = broadcaster.transfer(&request(), &signer).await.unwrap();
        assert_eq!(broadcast.state, BroadcastState::Submitted);
        assert_eq!(broadcast.chain, "ethereum");
        assert_eq!(broadcast.nonce, 5);
        assert_eq!(broadcast.from, signer.evm_address());
        assert_eq!(broadcast.to, request().to);
        // 21 000 gas plus the 20 % margin, at twice the base fee plus the tip.
        assert_eq!(broadcast.fee_limit_raw, 3_000_000_000u64 * 25_200);
        assert_eq!(broadcast.note, "sent at nonce 5; 12 blocks make it final");

        // The hash the node echoed is the hash of the bytes actually sent.
        let raw = hex::decode(broadcast.raw_tx.trim_start_matches("0x")).unwrap();
        let expected: [u8; 32] = Keccak256::digest(&raw).into();
        assert_eq!(broadcast.tx_hash, format!("0x{}", hex::encode(expected)));
        assert_eq!(raw[0], 0x02);
    }

    #[tokio::test]
    async fn an_evm_transfer_above_the_fee_ceiling_is_refused_before_signing() {
        let mock = Arc::new(MockRpc::new());
        mock.queue("eth_getTransactionCount", json!("0x5"));
        mock.queue("eth_estimateGas", json!("0x5208"));
        // A base fee of 100 gwei needs a 201 gwei maximum.
        mock.queue(
            "eth_getBlockByNumber",
            json!({"baseFeePerGas": "0x174876e800"}),
        );
        mock.queue("eth_maxPriorityFeePerGas", json!("0x3b9aca00"));
        let broadcaster = Broadcaster::new(
            mock,
            vec![("ethereum".into(), EVM_RPC.into())],
            None,
            150_000_000_000,
        );
        let error = broadcaster
            .transfer(&request(), &TestSigner::new())
            .await
            .unwrap_err();
        assert!(matches!(error, BroadcastError::FeeCeilingTooLow(_, _)));
    }

    #[tokio::test]
    async fn a_solana_transfer_is_built_signed_and_sent() {
        let mock = Arc::new(MockRpc::new());
        mock.queue(
            "getLatestBlockhash",
            json!({"value": {
                "blockhash": "11111111111111111111111111111111",
                "lastValidBlockHeight": 100
            }}),
        );
        mock.queue("getFeeForMessage", json!({"value": 5_000}));
        let broadcaster = test_broadcaster(mock.clone());
        let signer = TestSigner::new();

        let mut request = request();
        request.network = "solana".into();
        request.to = bs58::encode([9u8; 32]).into_string();
        request.amount_raw = 1_000_000;

        let broadcast = broadcaster.transfer(&request, &signer).await.unwrap();
        assert_eq!(broadcast.state, BroadcastState::Submitted);
        assert_eq!(broadcast.from, TxSigner::solana_address(&signer).unwrap());
        assert_eq!(broadcast.fee_limit_raw, 5_000);
        assert_eq!(broadcast.last_valid_block_height, 100);

        use base64::engine::general_purpose::STANDARD;
        use base64::Engine as _;
        let raw = STANDARD.decode(&broadcast.raw_tx).unwrap();
        assert_eq!(raw[0], 1); // one signature
        assert_eq!(raw[1..65].len(), 64);
        let message = &raw[65..];
        assert_eq!(&message[..3], &[1, 0, 1]);
        // The signature the node echoed is the transaction id.
        assert_eq!(broadcast.tx_hash, bs58::encode(&raw[1..65]).into_string());
    }

    #[tokio::test]
    async fn two_transfers_get_consecutive_nonces() {
        let mock = Arc::new(MockRpc::new());
        mock.queue("eth_getTransactionCount", json!("0x5"));
        mock.queue("eth_getTransactionCount", json!("0x5"));
        let broadcaster = test_broadcaster(mock);

        let signer = TestSigner::new();
        let from = signer.evm_address();
        let first = broadcaster
            .next_nonce(EVM_RPC, "ethereum", &from)
            .await
            .unwrap();
        let second = broadcaster
            .next_nonce(EVM_RPC, "ethereum", &from)
            .await
            .unwrap();
        assert_eq!((first, second), (5, 6));

        // If the node has caught up past the cache, the node wins.
        let mock = Arc::new(MockRpc::new());
        mock.queue("eth_getTransactionCount", json!("0x5"));
        mock.queue("eth_getTransactionCount", json!("0x9"));
        let broadcaster = test_broadcaster(mock);
        let first = broadcaster
            .next_nonce(EVM_RPC, "ethereum", &from)
            .await
            .unwrap();
        let second = broadcaster
            .next_nonce(EVM_RPC, "ethereum", &from)
            .await
            .unwrap();
        assert_eq!((first, second), (5, 9));
    }

    #[tokio::test]
    async fn a_failed_send_gives_the_nonce_back() {
        let mock = Arc::new(MockRpc::new());
        mock.queue("eth_getTransactionCount", json!("0x5"));
        mock.queue("eth_getTransactionCount", json!("0x5"));
        mock.queue("eth_getTransactionCount", json!("0x5"));
        let broadcaster = test_broadcaster(mock);
        let signer = TestSigner::new();
        let from = signer.evm_address();

        let nonce = broadcaster
            .next_nonce(EVM_RPC, "ethereum", &from)
            .await
            .unwrap();
        assert_eq!(nonce, 5);
        // The send failed, so the nonce was never used.
        broadcaster.release_nonce("ethereum", &from, nonce);
        let again = broadcaster
            .next_nonce(EVM_RPC, "ethereum", &from)
            .await
            .unwrap();
        assert_eq!(again, 5);

        // A later nonce is in flight, so releasing an older one must not rewind
        // the counter and hand out a duplicate.
        broadcaster.release_nonce("ethereum", &from, 6);
        let next = broadcaster
            .next_nonce(EVM_RPC, "ethereum", &from)
            .await
            .unwrap();
        assert_eq!(next, 6);
    }

    fn submitted_evm() -> Broadcast {
        Broadcast {
            id: "b-1".into(),
            chain: "ethereum".into(),
            from: "0x0000000000000000000000000000000000000001".into(),
            to: "0x0000000000000000000000000000000000000002".into(),
            nonce: 5,
            amount_raw: 1_000,
            fee_limit_raw: 10_000,
            fee_paid_raw: 0,
            tx_hash: "0xabc".into(),
            raw_tx: "0x02".into(),
            block_number: 0,
            block_hash: String::new(),
            last_valid_block_height: 0,
            confirmations: 0,
            state: BroadcastState::Submitted,
            note: String::new(),
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    #[tokio::test]
    async fn a_receipt_that_disappears_is_a_reorg() {
        let mock = Arc::new(MockRpc::new());
        let mut broadcast = submitted_evm();
        broadcast.block_number = 100;
        broadcast.block_hash = "0xaaa".into();
        broadcast.confirmations = 1;

        // The receipt is gone: the block that held it was replaced.
        mock.queue("eth_getTransactionReceipt", Value::Null);
        test_broadcaster(mock.clone())
            .refresh(&mut broadcast)
            .await
            .unwrap();
        assert_eq!(broadcast.state, BroadcastState::Reorged);
        assert_eq!(broadcast.block_number, 0);
        assert!(broadcast.note.contains("replaced"));
    }

    #[tokio::test]
    async fn a_receipt_that_moves_to_another_block_is_a_reorg() {
        let mock = Arc::new(MockRpc::new());
        let mut broadcast = submitted_evm();
        broadcast.block_number = 100;
        broadcast.block_hash = "0xaaa".into();

        mock.queue(
            "eth_getTransactionReceipt",
            json!({
                "blockNumber": "0x65",
                "blockHash": "0xbbb",
                "gasUsed": "0x5208",
                "effectiveGasPrice": "0x3b9aca00",
                "status": "0x1"
            }),
        );
        mock.queue("eth_blockNumber", json!("0x65"));
        test_broadcaster(mock.clone())
            .refresh(&mut broadcast)
            .await
            .unwrap();

        assert_eq!(broadcast.state, BroadcastState::Reorged);
        assert_eq!(broadcast.block_number, 101);
        assert_eq!(broadcast.block_hash, "0xbbb");
        assert_eq!(broadcast.confirmations, 1);
    }

    #[tokio::test]
    async fn confirmations_count_up_to_finality() {
        let mock = Arc::new(MockRpc::new());
        let mut broadcast = submitted_evm();
        mock.queue(
            "eth_getTransactionReceipt",
            json!({
                "blockNumber": "0x64",
                "blockHash": "0xaaa",
                "gasUsed": "0x5208",
                "effectiveGasPrice": "0x3b9aca00",
                "status": "0x1"
            }),
        );
        // Head is block 111, so block 100 has 12 confirmations.
        mock.queue("eth_blockNumber", json!("0x6f"));
        test_broadcaster(mock.clone())
            .refresh(&mut broadcast)
            .await
            .unwrap();
        assert_eq!(broadcast.confirmations, 12);
        assert_eq!(broadcast.state, BroadcastState::Confirmed);
        assert_eq!(broadcast.fee_paid_raw, 21_000u64 * 1_000_000_000);
        assert!(broadcast.state.is_terminal());
    }

    #[tokio::test]
    async fn a_consumed_nonce_means_the_transfer_was_replaced() {
        let mock = Arc::new(MockRpc::new());
        let mut broadcast = submitted_evm();
        mock.queue("eth_getTransactionReceipt", Value::Null);
        mock.queue("eth_getTransactionCount", json!("0x6"));
        test_broadcaster(mock.clone())
            .refresh(&mut broadcast)
            .await
            .unwrap();
        assert_eq!(broadcast.state, BroadcastState::Dropped);
        assert!(broadcast.note.contains("replaced"));
    }

    #[tokio::test]
    async fn a_reverted_transfer_still_paid_its_fee() {
        let mock = Arc::new(MockRpc::new());
        let mut broadcast = submitted_evm();
        mock.queue(
            "eth_getTransactionReceipt",
            json!({
                "blockNumber": "0x64",
                "blockHash": "0xaaa",
                "gasUsed": "0x5208",
                "effectiveGasPrice": "0x77359400",
                "status": "0x0"
            }),
        );
        mock.queue("eth_blockNumber", json!("0x6f"));
        test_broadcaster(mock.clone())
            .refresh(&mut broadcast)
            .await
            .unwrap();
        assert_eq!(broadcast.state, BroadcastState::Failed);
        assert_eq!(broadcast.fee_paid_raw, 21_000u64 * 2_000_000_000);
        assert!(broadcast.note.contains("fee"));
    }

    #[test]
    fn a_solana_transfer_message_has_the_system_transfer_shape() {
        let from = [1u8; 32];
        let to = [2u8; 32];
        let blockhash = [3u8; 32];
        let message = compile_transfer_message(&from, &to, &blockhash, 1_000_000);

        assert_eq!(&message[..3], &[1, 0, 1]);
        assert_eq!(message[3], 3);
        assert_eq!(&message[4..36], &from);
        assert_eq!(&message[36..68], &to);
        assert_eq!(&message[100..132], &blockhash);
        // One instruction, the system program, two accounts, 12 data bytes.
        let instruction = &message[132..];
        assert_eq!(&instruction[..5], &[1, 2, 2, 0, 1]);
        assert_eq!(instruction[5], 12);
        assert_eq!(&instruction[6..10], &SYSTEM_TRANSFER_INDEX.to_le_bytes());
        assert_eq!(&instruction[10..18], &1_000_000u64.to_le_bytes());
    }

    fn submitted_solana() -> Broadcast {
        Broadcast {
            id: "s-1".into(),
            chain: "solana".into(),
            from: "11111111111111111111111111111111".into(),
            to: "So11111111111111111111111111111111111111112".into(),
            nonce: 0,
            amount_raw: 1_000,
            fee_limit_raw: 5_000,
            fee_paid_raw: 0,
            tx_hash: "5VERv8NMvzbJMEkV8xnrLkEaWRtSz9CosKDYjCJjBRnbJLgp8uirBgmQpjKhoR4tjF3ZpRzrFmBV6UjKdiSZkQUW".into(),
            raw_tx: String::new(),
            block_number: 0,
            block_hash: String::new(),
            last_valid_block_height: 100,
            confirmations: 0,
            state: BroadcastState::Submitted,
            note: String::new(),
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    #[tokio::test]
    async fn solana_only_counts_a_finalized_slot_as_final() {
        let mock = Arc::new(MockRpc::new());
        let broadcaster = test_broadcaster(mock.clone());

        let mut confirmed = submitted_solana();
        mock.queue(
            "getSignatureStatuses",
            json!({"value": [{"confirmations": 1, "confirmationStatus": "confirmed"}]}),
        );
        broadcaster.refresh(&mut confirmed).await.unwrap();
        assert_eq!(confirmed.state, BroadcastState::Submitted);

        let mut finalized = submitted_solana();
        mock.queue(
            "getSignatureStatuses",
            json!({"value": [{"confirmations": 32, "confirmationStatus": "finalized"}]}),
        );
        broadcaster.refresh(&mut finalized).await.unwrap();
        assert_eq!(finalized.state, BroadcastState::Confirmed);
        assert_eq!(finalized.fee_paid_raw, 5_000);
    }

    #[tokio::test]
    async fn a_solana_transfer_becomes_dropped_when_its_blockhash_expires() {
        let mock = Arc::new(MockRpc::new());
        let broadcaster = test_broadcaster(mock.clone());
        let mut broadcast = submitted_solana();
        mock.queue("getSignatureStatuses", json!({"value": [null]}));
        mock.queue("getBlockHeight", json!(101));
        broadcaster.refresh(&mut broadcast).await.unwrap();
        assert_eq!(broadcast.state, BroadcastState::Dropped);
        assert!(broadcast.note.contains("expired"));
    }

    #[tokio::test]
    async fn a_solana_instruction_error_is_a_failure_that_paid_the_fee() {
        let mock = Arc::new(MockRpc::new());
        let broadcaster = test_broadcaster(mock.clone());
        let mut broadcast = submitted_solana();
        mock.queue(
            "getSignatureStatuses",
            json!({"value": [{"err": {"InstructionError": [0, "InsufficientFunds"]}}]}),
        );
        broadcaster.refresh(&mut broadcast).await.unwrap();
        assert_eq!(broadcast.state, BroadcastState::Failed);
        assert_eq!(broadcast.fee_paid_raw, 5_000);
    }
}

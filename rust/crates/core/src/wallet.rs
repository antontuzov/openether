//! Encrypted local keystore and dry-run signing.
//!
//! The core never holds a plaintext key for longer than it has to:
//!
//! * a private key lives on disk only as AES-256-GCM ciphertext,
//! * the passphrase is turned into a 256 bit key with Argon2id,
//! * the unlocked key is wrapped in [`zeroize::Zeroizing`] so it is wiped from
//!   memory as soon as it is dropped,
//! * an *unlock lease* bounds how long an unlocked key may stay resident, and
//! * the only signing path in this module is a dry run. **There is no
//!   broadcast function and no networking code in this file at all** — the
//!   module has no HTTP client, no RPC client and no socket. A signed
//!   transaction is a local artefact that the operator can inspect and hand
//!   to a signer themselves.
//!
//! # Keystore file format
//!
//! A keystore is a single JSON document. Version 2 looks like this:
//!
//! ```json
//! {
//!   "version": 2,
//!   "kdf": "argon2id",
//!   "kdf_params": { "m_cost_kib": 19456, "t_cost": 2, "p_cost": 1 },
//!   "salt": "<base64 of 16 random bytes>",
//!   "nonce": "<base64 of 12 random bytes>",
//!   "cipher": "aes-256-gcm",
//!   "ciphertext": "<base64 of the encrypted secret>",
//!   "key_check": "<base64 of a 32 byte HMAC-SHA256 tag>",
//!   "wallet_type": "hd",
//!   "chain": "evm",
//!   "address": "0x…",
//!   "public_key": "0x…",
//!   "accounts": [
//!     { "chain": "evm", "index": 0, "path": "m/44'/60'/0'/0/0",
//!       "address": "0x…", "public_key": "0x…" }
//!   ],
//!   "created_at_ms": 1730000000000
//! }
//! ```
//!
//! * `kdf` is always `argon2id` (Argon2id, version 0x13) with the parameters in
//!   `kdf_params`. The parameters are stored so that a keystore written by an
//!   older build can still be unlocked.
//! * `salt` is 16 random bytes; `nonce` is the 12 byte GCM nonce.
//! * `ciphertext` is the AES-256-GCM encryption of the sealed plaintext, which
//!   is `"OEKS2" || u16_be(len) || payload`.
//! * `wallet_type` is `single` for a keystore that holds one raw private key
//!   and `hd` for a keystore that holds a BIP-39 mnemonic and derives every
//!   account from it.
//! * `chain`, `address` and `public_key` describe the *primary* account, so
//!   that code written before the multi-account format keeps working.
//! * `accounts` lists every account the keystore holds. It is public data.
//! * The AEAD associated data is a canonical serialisation of every public
//!   header field except `ciphertext` and `key_check`, so the ciphertext is
//!   bound to the address, chain, account list and KDF parameters it was
//!   created with. Tampering with any of them breaks the decryption.
//! * `key_check` is `HMAC-SHA256(derived_key, "openether/keystore/key-check/v1")`
//!   and exists only to tell a wrong passphrase apart from a corrupted file.
//!   It is verified in constant time with `hmac::Mac::verify_slice`. It adds
//!   no attack surface beyond the GCM tag, which is offline verifiable anyway.
//! * The file is written with mode `0600` and contains no secret material in
//!   clear text: `address`, `public_key`, `chain` and `accounts` are public by
//!   design.
//!
//! Version 1 keystores — one raw private key, one chain, no `wallet_type` and
//! no `accounts` — are still readable. Their associated data and their sealed
//! plaintext are byte-for-byte what version 1 produced, so a file written by an
//! older build keeps working. New files are always written as version 2.
//!
//! # Address derivation
//!
//! Addresses are derived for real, not copied from a field the caller
//! supplied:
//!
//! * **EVM** — secp256k1 public key (`k256`), `keccak256(x || y)`, last 20
//!   bytes, rendered as an EIP-55 checksummed `0x…` address.
//! * **Solana** — Ed25519 public key (`ed25519-dalek`), base58 encoded
//!   (`bs58`).
//!
//! An HD keystore derives them from a BIP-39 mnemonic: BIP-44 for EVM
//! (`m/44'/60'/0'/0/i`) and SLIP-0010 for Solana (`m/44'/501'/i'/0'`).
//!
//! # Dry-run signing
//!
//! [`TxEnvelope`] is a canonical, chain-agnostic description of a transfer.
//! It is serialised deterministically and signed:
//!
//! * EVM: `keccak256(canonical_bytes)` is signed with ECDSA (secp256k1); the
//!   signature is returned as `r || s || v` (65 bytes, `v ∈ {27, 28}`) and the
//!   transaction hash is the keccak digest.
//! * Solana: the canonical bytes themselves are signed with Ed25519; the
//!   transaction hash is `base58(sha256(canonical_bytes))`.
//!
//! The canonical bytes are *not* RLP and *not* a Solana transaction message.
//! The signature therefore proves that the operator signed exactly the
//! envelope that is returned, but it is not directly broadcastable — this
//! build has no broadcast path at all.

use crate::config::WalletConfig;
use crate::hd::{DerivationPath, HdWallet};
use crate::types::{now_ms, Chain};
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use bip39::Mnemonic;
use ed25519_dalek::{Signer, Verifier};
use hmac::{Hmac, Mac};
use rand::rngs::OsRng;
use rand::TryRngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sha3::Keccak256;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

type HmacSha256 = Hmac<Sha256>;

/// The current keystore format version.
pub const KEYSTORE_VERSION: u16 = 2;
/// The first keystore format: one raw private key, one chain.
pub const KEYSTORE_VERSION_V1: u16 = 1;
/// Name of the key derivation function recorded in the keystore.
pub const KDF_NAME: &str = "argon2id";
/// Name of the AEAD recorded in the keystore.
pub const CIPHER_NAME: &str = "aes-256-gcm";
/// A keystore that holds a single private key.
pub const WALLET_TYPE_SINGLE: &str = "single";
/// A keystore that holds a seed and derives every account from it.
pub const WALLET_TYPE_HD: &str = "hd";

const MAGIC: &[u8; 5] = b"OEKS1";
const MAGIC_HD: &[u8; 5] = b"OEKS2";
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;
const KEY_CHECK_LEN: usize = 32;
const KEY_CHECK_CONTEXT: &[u8] = b"openether/keystore/key-check/v1";
/// The largest number of accounts a single keystore may hold.
pub const MAX_ACCOUNTS: u32 = 100;

/// Everything that can go wrong in the wallet module.
///
/// No variant ever embeds key material: the messages are static or describe
/// the shape of the request, never the secret.
#[derive(Debug, thiserror::Error)]
pub enum WalletError {
    #[error("wallet is not configured: no keystore path is set")]
    NotConfigured,
    #[error("wallet is locked: unlock the keystore before signing")]
    NotUnlocked,
    #[error("the unlock lease expired; unlock the keystore again")]
    LeaseExpired,
    #[error("invalid passphrase")]
    InvalidPassphrase,
    #[error("the keystore could not be decrypted: it is corrupt or was tampered with")]
    DecryptFailed,
    #[error("live broadcast is not supported in this build; only dry-run signing is available")]
    LiveBroadcastUnsupported,
    #[error("chain mismatch: the keystore holds a {keystore} key but the envelope is {envelope}")]
    ChainMismatch {
        keystore: &'static str,
        envelope: &'static str,
    },
    #[error("invalid key material")]
    InvalidKey,
    #[error("invalid transaction envelope: {0}")]
    InvalidEnvelope(String),
    #[error("the signature does not verify against the public key")]
    SignatureInvalid,
    #[error("no such account: {0}")]
    AccountNotFound(String),
    #[error("unsupported keystore format: {0}")]
    UnsupportedFormat(String),
    #[error("keystore io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Argon2id cost parameters. They are stored inside the keystore so that a
/// keystore stays unlockable even if the defaults change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdfParams {
    /// Memory cost in kibibytes.
    pub m_cost_kib: u32,
    /// Number of passes over the memory.
    pub t_cost: u32,
    /// Number of lanes.
    pub p_cost: u32,
}

impl Default for KdfParams {
    fn default() -> Self {
        Self {
            m_cost_kib: 19_456,
            t_cost: 2,
            p_cost: 1,
        }
    }
}

impl KdfParams {
    fn argon2(&self) -> Result<Argon2<'static>, WalletError> {
        // A keystore file is untrusted input: refuse absurd cost parameters so
        // that a tampered file cannot turn an unlock into a denial of service.
        if self.p_cost == 0
            || self.p_cost > 16
            || self.t_cost == 0
            || self.t_cost > 16
            || self.m_cost_kib > 1_048_576
        {
            return Err(WalletError::UnsupportedFormat(
                "argon2 parameters are out of range".into(),
            ));
        }
        let params = Params::new(self.m_cost_kib, self.t_cost, self.p_cost, Some(KEY_LEN))
            .map_err(|_| WalletError::UnsupportedFormat("invalid argon2 parameters".into()))?;
        Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
    }
}

/// One account of a keystore.
///
/// Everything in here is public: a chain, an index, a derivation path and the
/// address and public key derived from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountEntry {
    /// `evm` or `solana`.
    pub chain: String,
    /// The position of the account within its chain.
    pub index: u32,
    /// The derivation path. Empty for a single-key keystore.
    pub path: String,
    /// The derived address.
    pub address: String,
    /// The derived public key.
    pub public_key: String,
}

/// Picks the account of a keystore that signs a transaction.
///
/// Both fields are optional: without them the primary account signs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccountSelector {
    /// The chain of the account. `None` means "the chain of the envelope".
    #[serde(default)]
    pub chain: Option<String>,
    /// The index of the account within its chain. `None` means the first one.
    #[serde(default)]
    pub index: Option<u32>,
}

/// The on-disk keystore document.
///
/// The derived `Debug` implementation redacts the ciphertext and the key
/// check so that a stray `{:?}` can never spill encrypted material into a log.
pub struct KeystoreFile {
    pub version: u16,
    pub kdf: String,
    pub kdf_params: KdfParams,
    pub salt: String,
    pub nonce: String,
    pub cipher: String,
    pub ciphertext: String,
    pub key_check: String,
    pub chain: String,
    pub address: String,
    pub public_key: String,
    pub created_at_ms: i64,
    /// `single` or `hd`.
    pub wallet_type: String,
    /// Every account the keystore holds.
    pub accounts: Vec<AccountEntry>,
}

impl fmt::Debug for KeystoreFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeystoreFile")
            .field("version", &self.version)
            .field("kdf", &self.kdf)
            .field("kdf_params", &self.kdf_params)
            .field("salt", &"<redacted>")
            .field("nonce", &"<redacted>")
            .field("cipher", &self.cipher)
            .field("ciphertext", &"<redacted>")
            .field("key_check", &"<redacted>")
            .field("chain", &self.chain)
            .field("address", &self.address)
            .field("public_key", &self.public_key)
            .field("created_at_ms", &self.created_at_ms)
            .field("wallet_type", &self.wallet_type)
            .field("accounts", &self.accounts)
            .finish()
    }
}

impl Serialize for KeystoreFile {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("KeystoreFile", 15)?;
        s.serialize_field("version", &self.version)?;
        s.serialize_field("kdf", &self.kdf)?;
        s.serialize_field("kdf_params", &self.kdf_params)?;
        s.serialize_field("salt", &self.salt)?;
        s.serialize_field("nonce", &self.nonce)?;
        s.serialize_field("cipher", &self.cipher)?;
        s.serialize_field("ciphertext", &self.ciphertext)?;
        s.serialize_field("key_check", &self.key_check)?;
        s.serialize_field("chain", &self.chain)?;
        s.serialize_field("address", &self.address)?;
        s.serialize_field("public_key", &self.public_key)?;
        s.serialize_field("created_at_ms", &self.created_at_ms)?;
        s.serialize_field("wallet_type", &self.wallet_type)?;
        s.serialize_field("accounts", &self.accounts)?;
        s.end()
    }
}

impl<'de> Deserialize<'de> for KeystoreFile {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            version: u16,
            kdf: String,
            kdf_params: KdfParams,
            salt: String,
            nonce: String,
            cipher: String,
            ciphertext: String,
            key_check: String,
            chain: String,
            address: String,
            public_key: String,
            created_at_ms: i64,
            /// Absent in version 1 files.
            #[serde(default)]
            wallet_type: String,
            /// Absent in version 1 files.
            #[serde(default)]
            accounts: Vec<AccountEntry>,
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(Self {
            version: raw.version,
            kdf: raw.kdf,
            kdf_params: raw.kdf_params,
            salt: raw.salt,
            nonce: raw.nonce,
            cipher: raw.cipher,
            ciphertext: raw.ciphertext,
            key_check: raw.key_check,
            chain: raw.chain,
            address: raw.address,
            public_key: raw.public_key,
            created_at_ms: raw.created_at_ms,
            wallet_type: raw.wallet_type,
            accounts: raw.accounts,
        })
    }
}

/// Secret key bytes. Wiped from memory when dropped.
#[derive(Zeroize, ZeroizeOnDrop)]
struct SecretMaterial {
    bytes: Zeroizing<Vec<u8>>,
}

impl fmt::Debug for SecretMaterial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretMaterial(<redacted>)")
    }
}

/// The plaintext of a keystore, once it is decrypted.
enum KeystoreMaterial {
    /// One raw private key.
    Single(SecretMaterial),
    /// A BIP-39 mnemonic, from which every account is derived.
    Hd(Zeroizing<String>),
}

impl fmt::Debug for KeystoreMaterial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeystoreMaterial::Single(_) => f.write_str("KeystoreMaterial::Single(<redacted>)"),
            KeystoreMaterial::Hd(_) => f.write_str("KeystoreMaterial::Hd(<redacted>)"),
        }
    }
}

/// The unlocked material a lease holds.
enum LeaseMaterial {
    Single(SecretMaterial),
    Hd(HdWallet),
}

/// Public, non-sensitive description of the wallet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletMetadata {
    /// `true` when a keystore is configured *and* could be loaded.
    pub configured: bool,
    /// `evm` / `solana` / `null`.
    pub chain: Option<String>,
    /// The derived address, or `null` when no keystore is loaded.
    pub address: Option<String>,
    /// Always `true`: this build has no plaintext keystore format.
    pub encrypted: bool,
    /// `true` when only dry-run signing is permitted.
    pub dry_run: bool,
    /// The derived public key (public data).
    pub public_key: Option<String>,
    /// `single` / `hd` / `null`.
    pub wallet_type: Option<String>,
    /// Every account the keystore holds.
    pub accounts: Vec<AccountEntry>,
}

/// A canonical, chain agnostic description of a transfer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TxEnvelope {
    /// Target chain. Must match the chain of the keystore.
    pub chain: Chain,
    /// Monotonic nonce (or sequence number) chosen by the caller.
    #[serde(default)]
    pub nonce: u64,
    /// Timestamp of the envelope.
    #[serde(default)]
    pub timestamp_ms: i64,
    /// Destination address.
    pub to: String,
    /// Human readable amount. Informational only: `amount_raw` is what gets
    /// signed, because floats are not canonical.
    #[serde(default)]
    pub amount: f64,
    /// Amount in the smallest unit of the chain. This is the signed value.
    #[serde(default)]
    pub amount_raw: u64,
    /// Fee in the smallest unit of the chain.
    #[serde(default)]
    pub fee: u64,
    /// Free-form memo.
    #[serde(default)]
    pub memo: String,
}

impl TxEnvelope {
    fn validate(&self) -> Result<(), WalletError> {
        if self.to.trim().is_empty() {
            return Err(WalletError::InvalidEnvelope(
                "the destination address must not be empty".into(),
            ));
        }
        if self.to.len() > u16::MAX as usize {
            return Err(WalletError::InvalidEnvelope(
                "the destination address is too long".into(),
            ));
        }
        if self.memo.len() > u16::MAX as usize {
            return Err(WalletError::InvalidEnvelope("the memo is too long".into()));
        }
        if !self.amount.is_finite() || self.amount < 0.0 {
            return Err(WalletError::InvalidEnvelope(
                "the amount must be a positive finite number".into(),
            ));
        }
        Ok(())
    }

    /// Deterministic byte serialisation of the signed fields.
    ///
    /// `amount` is deliberately excluded: only `amount_raw` is signed so that
    /// the same logical transfer always produces the same digest.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(96 + self.to.len() + self.memo.len());
        out.extend_from_slice(b"openether/tx/v1");
        out.push(chain_tag(self.chain));
        out.extend_from_slice(&self.nonce.to_be_bytes());
        out.extend_from_slice(&self.timestamp_ms.to_be_bytes());
        let to = self.to.as_bytes();
        out.extend_from_slice(&(to.len() as u16).to_be_bytes());
        out.extend_from_slice(to);
        out.extend_from_slice(&self.amount_raw.to_be_bytes());
        out.extend_from_slice(&self.fee.to_be_bytes());
        let memo = self.memo.as_bytes();
        out.extend_from_slice(&(memo.len() as u16).to_be_bytes());
        out.extend_from_slice(memo);
        out
    }
}

/// The result of a dry-run signature.
///
/// `broadcast` is always `false` and `dry_run` is always `true`: this build
/// has no code path that could send a transaction anywhere.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DryRunSignature {
    /// Always `true`.
    pub dry_run: bool,
    /// Always `false`.
    pub broadcast: bool,
    /// Chain the envelope was signed for.
    pub chain: Chain,
    /// Address of the signer.
    pub address: String,
    /// Public key of the signer.
    pub public_key: String,
    /// Digest of the canonical envelope.
    pub tx_hash: String,
    /// The signature. Signatures are public, keys are not.
    pub signature: String,
    /// The envelope that was signed.
    pub envelope: TxEnvelope,
    /// The account that signed.
    pub account: AccountEntry,
    /// When the signature was produced.
    pub signed_at_ms: i64,
    /// Human readable reminder that nothing was sent.
    pub note: String,
}

/// An unlocked key with a hard expiry.
struct KeyLease {
    /// The unlocked material.
    material: LeaseMaterial,
    expires_at_ms: i64,
}

/// The lease slot: the unlocked key (if any) and whether the previous lease
/// ran out. The flag lets the API answer "the lease expired" instead of the
/// vaguer "the wallet is locked" once the key has already been wiped.
#[derive(Default)]
struct LeaseState {
    lease: Option<KeyLease>,
    expired: bool,
}

/// Wallet service: owns the (encrypted) keystore and the unlock lease.
///
/// The service holds **no** secret material unless a lease is active. The
/// lease is bounded by `WalletConfig::unlock_timeout_ms` and is reaped both
/// lazily (on every access) and by a background task, so an agent cannot keep
/// a key resident by repeatedly asking for it.
pub struct WalletService {
    config: WalletConfig,
    keystore: Option<Arc<KeystoreFile>>,
    lease: Mutex<LeaseState>,
}

impl fmt::Debug for WalletService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WalletService")
            .field("configured", &self.keystore.is_some())
            .field("dry_run", &self.config.dry_run)
            .field("unlock_timeout_ms", &self.config.unlock_timeout_ms)
            .field(
                "lease_active",
                &self
                    .lease
                    .lock()
                    .map(|l| l.lease.is_some())
                    .unwrap_or(false),
            )
            .finish()
    }
}

impl WalletService {
    /// Builds the service and loads the keystore metadata (never the secret).
    pub fn new(config: WalletConfig) -> Self {
        let keystore = match config.keystore_path.as_ref() {
            Some(path) => match load_keystore(path) {
                Ok(file) => {
                    // A single-key keystore holds exactly one chain, so a
                    // mismatch is worth a warning. An HD keystore holds
                    // accounts on every chain, so there is nothing to compare.
                    if file.wallet_type == WALLET_TYPE_SINGLE {
                        if let Some(chain) = config.chain {
                            if chain_label(chain) != file.chain {
                                tracing::warn!(
                                    configured_chain = chain_label(chain),
                                    keystore_chain = %file.chain,
                                    "the configured wallet chain does not match the keystore; the keystore wins"
                                );
                            }
                        }
                    }
                    Some(Arc::new(file))
                }
                Err(e) => {
                    tracing::error!(error = %e, "the wallet keystore could not be loaded");
                    None
                }
            },
            None => None,
        };

        Self {
            config,
            keystore,
            lease: Mutex::new(LeaseState::default()),
        }
    }

    /// `true` when a keystore is configured and could be read.
    pub fn is_configured(&self) -> bool {
        self.keystore.is_some()
    }

    /// Public metadata. Never contains a key, a passphrase or ciphertext.
    pub fn metadata(&self) -> WalletMetadata {
        WalletMetadata {
            configured: self.keystore.is_some(),
            chain: self.keystore.as_ref().map(|k| k.chain.clone()),
            address: self.keystore.as_ref().map(|k| k.address.clone()),
            encrypted: true,
            dry_run: self.config.dry_run,
            public_key: self.keystore.as_ref().map(|k| k.public_key.clone()),
            wallet_type: self.keystore.as_ref().map(|k| k.wallet_type.clone()),
            accounts: self
                .keystore
                .as_ref()
                .map(|k| k.accounts.clone())
                .unwrap_or_default(),
        }
    }

    /// Unlocks the keystore and starts an unlock lease.
    ///
    /// The lease expires after `unlock_timeout_ms`; the key is then wiped from
    /// memory. Repeated calls do not extend an existing lease.
    pub fn unlock(&self, passphrase: &str) -> Result<(), WalletError> {
        let keystore = self.keystore.as_ref().ok_or(WalletError::NotConfigured)?;
        let material = open_keystore(keystore, passphrase)?;
        let material = lease_material(material)?;
        let expires_at_ms = now_ms() + self.config.unlock_timeout_ms.max(1);
        let mut state = self.lock_lease();
        state.lease = Some(KeyLease {
            material,
            expires_at_ms,
        });
        state.expired = false;
        drop(state);
        tracing::info!(
            chain = %keystore.chain,
            wallet_type = %keystore.wallet_type,
            timeout_ms = self.config.unlock_timeout_ms,
            "wallet keystore unlocked"
        );
        Ok(())
    }

    /// Locks the wallet immediately, wiping the key from memory.
    pub fn lock(&self) {
        let mut state = self.lock_lease();
        if state.lease.take().is_some() {
            state.expired = false;
            tracing::info!("wallet keystore locked");
        }
    }

    /// Expiry of the active lease, if any.
    pub fn lease_expires_at_ms(&self) -> Option<i64> {
        let state = self.lock_lease();
        state.lease.as_ref().map(|l| l.expires_at_ms)
    }

    /// Drops the lease when it has expired. Returns `true` when a key was
    /// dropped.
    pub fn reap_expired(&self) -> bool {
        let now = now_ms();
        let mut state = self.lock_lease();
        let expired = state
            .lease
            .as_ref()
            .map(|l| now >= l.expires_at_ms)
            .unwrap_or(false);
        if expired {
            // Dropping the lease wipes the key bytes.
            state.lease = None;
            state.expired = true;
            tracing::info!("wallet unlock lease expired; the key was wiped");
        }
        expired
    }

    /// Spawns the background reaper that wipes the key when the lease
    /// expires, even if no request arrives in the meantime.
    pub fn spawn_key_reaper(self: &Arc<Self>) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let this = Arc::clone(self);
        let tick = self.config.unlock_timeout_ms.clamp(50, 1_000) as u64;
        handle.spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(tick));
            loop {
                interval.tick().await;
                this.reap_expired();
            }
        });
    }

    /// Signs an envelope with the currently unlocked key.
    pub fn sign_dry_run(
        &self,
        envelope: &TxEnvelope,
        selector: &AccountSelector,
    ) -> Result<DryRunSignature, WalletError> {
        if !self.config.dry_run {
            return Err(WalletError::LiveBroadcastUnsupported);
        }
        let now = now_ms();
        let mut state = self.lock_lease();
        let expired = state
            .lease
            .as_ref()
            .map(|lease| now >= lease.expires_at_ms)
            .unwrap_or(false);
        if expired {
            // Dropping the lease wipes the key bytes.
            state.lease = None;
            state.expired = true;
            return Err(WalletError::LeaseExpired);
        }
        let Some(lease) = state.lease.as_ref() else {
            return Err(if state.expired {
                WalletError::LeaseExpired
            } else {
                WalletError::NotUnlocked
            });
        };
        let keystore = self.keystore.as_ref().ok_or(WalletError::NotConfigured)?;
        sign_with_material(keystore, &lease.material, selector, envelope)
    }

    /// Signs an envelope with a passphrase, without keeping the key around.
    ///
    /// This is the path used by the HTTP endpoint: the key exists for the
    /// duration of the call and is wiped immediately afterwards, so a request
    /// can never leave the core holding an unlocked key.
    pub fn sign_dry_run_with_passphrase(
        &self,
        passphrase: &str,
        envelope: &TxEnvelope,
        selector: &AccountSelector,
    ) -> Result<DryRunSignature, WalletError> {
        if !self.config.dry_run {
            return Err(WalletError::LiveBroadcastUnsupported);
        }
        let keystore = self.keystore.as_ref().ok_or(WalletError::NotConfigured)?;
        let material = open_keystore(keystore, passphrase)?;
        let material = lease_material(material)?;
        sign_with_material(keystore, &material, selector, envelope)
    }

    fn lock_lease(&self) -> std::sync::MutexGuard<'_, LeaseState> {
        self.lease.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Turns the decrypted plaintext into the material a lease holds.
fn lease_material(material: KeystoreMaterial) -> Result<LeaseMaterial, WalletError> {
    Ok(match material {
        KeystoreMaterial::Single(secret) => LeaseMaterial::Single(secret),
        KeystoreMaterial::Hd(mnemonic) => LeaseMaterial::Hd(
            HdWallet::from_mnemonic(&mnemonic, "")
                .map_err(|e| WalletError::UnsupportedFormat(format!("invalid mnemonic: {e}")))?,
        ),
    })
}

/// Creates an encrypted keystore file for `secret` and returns its metadata.
///
/// The secret is only ever written as ciphertext. The file is created with
/// mode `0600`.
pub fn create_keystore(
    path: &Path,
    chain: Chain,
    secret: &[u8],
    passphrase: &str,
    params: KdfParams,
) -> Result<WalletMetadata, WalletError> {
    if passphrase.is_empty() {
        return Err(WalletError::InvalidPassphrase);
    }
    let (public_key, address) = derive_public(chain, secret)?;

    let mut salt = [0u8; SALT_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    OsRng
        .try_fill_bytes(&mut salt)
        .map_err(|_| WalletError::UnsupportedFormat("no entropy available".into()))?;
    OsRng
        .try_fill_bytes(&mut nonce)
        .map_err(|_| WalletError::UnsupportedFormat("no entropy available".into()))?;

    let derived = derive_key(passphrase, &salt, params)?;
    let key_check = key_check_tag(&derived);

    let accounts = vec![AccountEntry {
        chain: chain_label(chain).to_string(),
        index: 0,
        path: String::new(),
        address: address.clone(),
        public_key: public_key.clone(),
    }];

    let mut file = KeystoreFile {
        version: KEYSTORE_VERSION,
        kdf: KDF_NAME.to_string(),
        kdf_params: params,
        salt: BASE64.encode(salt),
        nonce: BASE64.encode(nonce),
        cipher: CIPHER_NAME.to_string(),
        ciphertext: String::new(),
        key_check: BASE64.encode(key_check),
        chain: chain_label(chain).to_string(),
        address: address.clone(),
        public_key: public_key.clone(),
        created_at_ms: now_ms(),
        wallet_type: WALLET_TYPE_SINGLE.to_string(),
        accounts,
    };

    let plaintext = seal_plaintext(secret);
    encrypt_keystore(&mut file, &plaintext, &derived);

    write_keystore(path, &file)?;
    tracing::info!(
        chain = chain_label(chain),
        path = %path.display(),
        "wallet keystore created"
    );

    Ok(WalletMetadata {
        configured: true,
        chain: Some(chain_label(chain).to_string()),
        address: Some(address),
        encrypted: true,
        dry_run: true,
        public_key: Some(public_key),
        wallet_type: Some(WALLET_TYPE_SINGLE.to_string()),
        accounts: file.accounts,
    })
}

/// Creates an encrypted keystore that derives every account from a BIP-39
/// mnemonic.
///
/// The mnemonic is the root of every account. It is *not* written to the file
/// in clear text: the file holds the encrypted mnemonic, and the operator is
/// expected to write the words down separately.
pub fn create_hd_keystore(
    path: &Path,
    mnemonic: &str,
    passphrase: &str,
    primary: Chain,
    accounts_per_chain: u32,
    params: KdfParams,
) -> Result<WalletMetadata, WalletError> {
    if passphrase.is_empty() {
        return Err(WalletError::InvalidPassphrase);
    }
    if accounts_per_chain == 0 || accounts_per_chain > MAX_ACCOUNTS {
        return Err(WalletError::UnsupportedFormat(format!(
            "the account count must be between 1 and {MAX_ACCOUNTS}"
        )));
    }
    let words = Mnemonic::parse_normalized(mnemonic)
        .map_err(|e| WalletError::UnsupportedFormat(format!("invalid mnemonic: {e}")))?
        .to_string();
    let wallet = HdWallet::from_mnemonic(&words, "")
        .map_err(|e| WalletError::UnsupportedFormat(format!("invalid mnemonic: {e}")))?;

    let mut accounts = Vec::with_capacity(accounts_per_chain as usize * 2);
    for chain in [Chain::Evm, Chain::Solana] {
        for index in 0..accounts_per_chain {
            let path = path_for(chain, index);
            let key = derive_hd_key(&wallet, chain, index)?;
            let (public_key, address) = derive_public(chain, &key[..])?;
            accounts.push(AccountEntry {
                chain: chain_label(chain).to_string(),
                index,
                path: path.to_string(),
                address,
                public_key,
            });
        }
    }

    let primary_account = accounts
        .iter()
        .find(|a| a.chain == chain_label(primary) && a.index == 0)
        .ok_or_else(|| {
            WalletError::UnsupportedFormat("the primary account could not be derived".into())
        })?
        .clone();

    let mut salt = [0u8; SALT_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    OsRng
        .try_fill_bytes(&mut salt)
        .map_err(|_| WalletError::UnsupportedFormat("no entropy available".into()))?;
    OsRng
        .try_fill_bytes(&mut nonce)
        .map_err(|_| WalletError::UnsupportedFormat("no entropy available".into()))?;

    let derived = derive_key(passphrase, &salt, params)?;
    let key_check = key_check_tag(&derived);

    let mut file = KeystoreFile {
        version: KEYSTORE_VERSION,
        kdf: KDF_NAME.to_string(),
        kdf_params: params,
        salt: BASE64.encode(salt),
        nonce: BASE64.encode(nonce),
        cipher: CIPHER_NAME.to_string(),
        ciphertext: String::new(),
        key_check: BASE64.encode(key_check),
        chain: chain_label(primary).to_string(),
        address: primary_account.address.clone(),
        public_key: primary_account.public_key.clone(),
        created_at_ms: now_ms(),
        wallet_type: WALLET_TYPE_HD.to_string(),
        accounts,
    };

    let plaintext = seal_plaintext(words.as_bytes());
    encrypt_keystore(&mut file, &plaintext, &derived);

    write_keystore(path, &file)?;
    tracing::info!(
        primary = chain_label(primary),
        accounts = file.accounts.len(),
        path = %path.display(),
        "hd wallet keystore created"
    );

    Ok(WalletMetadata {
        configured: true,
        chain: Some(chain_label(primary).to_string()),
        address: Some(primary_account.address),
        encrypted: true,
        dry_run: true,
        public_key: Some(primary_account.public_key),
        wallet_type: Some(WALLET_TYPE_HD.to_string()),
        accounts: file.accounts,
    })
}

/// Creates an HD keystore from fresh entropy and returns the mnemonic.
///
/// The mnemonic is handed back exactly once, here, and never written to disk
/// in clear text. If the operator loses it, the wallet is gone.
pub fn generate_hd_keystore(
    path: &Path,
    passphrase: &str,
    primary: Chain,
    accounts_per_chain: u32,
    params: KdfParams,
) -> Result<(String, WalletMetadata), WalletError> {
    let mut entropy = [0u8; 16];
    OsRng
        .try_fill_bytes(&mut entropy)
        .map_err(|_| WalletError::UnsupportedFormat("no entropy available".into()))?;
    let mnemonic = Mnemonic::from_entropy(&entropy)
        .map_err(|e| WalletError::UnsupportedFormat(format!("cannot build a mnemonic: {e}")))?
        .to_string();
    let metadata = create_hd_keystore(
        path,
        &mnemonic,
        passphrase,
        primary,
        accounts_per_chain,
        params,
    )?;
    Ok((mnemonic, metadata))
}

/// Reads the public metadata of a keystore without unlocking it.
pub fn read_keystore_metadata(path: &Path) -> Result<WalletMetadata, WalletError> {
    let file = load_keystore(path)?;
    Ok(WalletMetadata {
        configured: true,
        chain: Some(file.chain.clone()),
        address: Some(file.address.clone()),
        encrypted: true,
        dry_run: true,
        public_key: Some(file.public_key.clone()),
        wallet_type: Some(file.wallet_type.clone()),
        accounts: file.accounts,
    })
}

/// Encrypts the plaintext into `file` and stores the ciphertext in it.
fn encrypt_keystore(file: &mut KeystoreFile, plaintext: &[u8], derived: &[u8]) {
    let aad = header_aad(file);
    let cipher =
        Aes256Gcm::new_from_slice(derived).expect("a 32 byte key is always a valid aes key");
    let nonce = BASE64
        .decode(file.nonce.as_bytes())
        .expect("the nonce was just written as base64");
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .expect("aes-gcm encryption of a small buffer cannot fail");
    file.ciphertext = BASE64.encode(ciphertext);
}

/// The derivation path of an account.
fn path_for(chain: Chain, index: u32) -> DerivationPath {
    match chain {
        Chain::Evm => DerivationPath::evm(index),
        Chain::Solana => DerivationPath::solana(index),
    }
}

/// Derives the private key of an account of an HD wallet.
fn derive_hd_key(
    wallet: &HdWallet,
    chain: Chain,
    index: u32,
) -> Result<Zeroizing<[u8; 32]>, WalletError> {
    let path = path_for(chain, index);
    let key = match chain {
        Chain::Evm => wallet.derive_evm(&path),
        Chain::Solana => wallet.derive_solana(&path),
    };
    key.map_err(|e| WalletError::UnsupportedFormat(format!("key derivation failed: {e}")))
}

/// Picks the account that signs and the key material to sign with.
fn sign_with_material(
    file: &KeystoreFile,
    material: &LeaseMaterial,
    selector: &AccountSelector,
    envelope: &TxEnvelope,
) -> Result<DryRunSignature, WalletError> {
    let (chain, index) = resolve_account(file, selector, envelope)?;
    match material {
        LeaseMaterial::Single(secret) => {
            sign_with_secret(chain, &secret.bytes[..], envelope, index, String::new())
        }
        LeaseMaterial::Hd(wallet) => {
            let key = derive_hd_key(wallet, chain, index)?;
            let (_, address) = derive_public(chain, &key[..])?;
            check_against_header(file, chain, index, &address)?;
            sign_with_secret(
                chain,
                &key[..],
                envelope,
                index,
                path_for(chain, index).to_string(),
            )
        }
    }
}

/// Resolves the account a request refers to.
///
/// A single-key keystore holds one account; an HD keystore holds one account
/// per index per chain.
fn resolve_account(
    file: &KeystoreFile,
    selector: &AccountSelector,
    envelope: &TxEnvelope,
) -> Result<(Chain, u32), WalletError> {
    let (chain, index) = match file.wallet_type.as_str() {
        WALLET_TYPE_HD => {
            let chain = match &selector.chain {
                Some(label) => parse_chain_label(label).ok_or_else(|| {
                    WalletError::UnsupportedFormat(format!("unknown chain: {label}"))
                })?,
                None => envelope.chain,
            };
            (chain, selector.index.unwrap_or(0))
        }
        _ => {
            let chain = parse_chain_label(&file.chain).ok_or_else(|| {
                WalletError::UnsupportedFormat(format!("unknown chain: {}", file.chain))
            })?;
            let index = selector.index.unwrap_or(0);
            if index != 0 {
                return Err(WalletError::AccountNotFound(format!(
                    "the keystore holds a single {} account",
                    chain_label(chain)
                )));
            }
            (chain, index)
        }
    };

    if envelope.chain != chain {
        return Err(WalletError::ChainMismatch {
            keystore: chain_label(chain),
            envelope: chain_label(envelope.chain),
        });
    }
    Ok((chain, index))
}

/// Checks a derived address against the account list recorded in the header.
///
/// The header is bound to the ciphertext by the AEAD, so a mismatch means the
/// file describes a different key than the one it actually holds.
fn check_against_header(
    file: &KeystoreFile,
    chain: Chain,
    index: u32,
    address: &str,
) -> Result<(), WalletError> {
    let Some(entry) = file
        .accounts
        .iter()
        .find(|a| a.chain == chain_label(chain) && a.index == index)
    else {
        return Ok(());
    };
    if entry.address != address {
        return Err(WalletError::AccountNotFound(format!(
            "the {} account {index} in the keystore does not match the derived key",
            chain_label(chain)
        )));
    }
    Ok(())
}

/// Signs `envelope` with `secret` and returns the dry-run result.
fn sign_with_secret(
    chain: Chain,
    secret: &[u8],
    envelope: &TxEnvelope,
    index: u32,
    path: String,
) -> Result<DryRunSignature, WalletError> {
    envelope.validate()?;
    if envelope.chain != chain {
        return Err(WalletError::ChainMismatch {
            keystore: chain_label(chain),
            envelope: chain_label(envelope.chain),
        });
    }
    let message = envelope.canonical_bytes();

    let (public_key, address, tx_hash, signature) = match chain {
        Chain::Evm => {
            let signing = evm_signing_key(secret)?;
            let digest = Keccak256::digest(&message);
            let (sig, recovery_id) = signing
                .sign_recoverable(&digest)
                .map_err(|_| WalletError::InvalidKey)?;
            let mut sig_bytes = [0u8; 65];
            sig_bytes[..32].copy_from_slice(&sig.r().to_bytes());
            sig_bytes[32..64].copy_from_slice(&sig.s().to_bytes());
            sig_bytes[64] = 27 + recovery_id.to_byte();
            let point = signing.verifying_key().to_encoded_point(false);
            (
                format!("0x{}", hex::encode(point.as_bytes())),
                evm_address(point.as_bytes()),
                format!("0x{}", hex::encode(digest)),
                format!("0x{}", hex::encode(sig_bytes)),
            )
        }
        Chain::Solana => {
            let signing = solana_signing_key(secret)?;
            let signature = signing.sign(&message);
            let public_bytes = signing.verifying_key().to_bytes();
            (
                hex::encode(public_bytes),
                bs58::encode(public_bytes).into_string(),
                bs58::encode(Sha256::digest(&message)).into_string(),
                bs58::encode(signature.to_bytes()).into_string(),
            )
        }
    };

    Ok(DryRunSignature {
        dry_run: true,
        broadcast: false,
        chain,
        address: address.clone(),
        public_key: public_key.clone(),
        tx_hash,
        signature,
        envelope: envelope.clone(),
        account: AccountEntry {
            chain: chain_label(chain).to_string(),
            index,
            path,
            address,
            public_key,
        },
        signed_at_ms: now_ms(),
        note: "dry run only: this transaction was signed locally and never broadcast".to_string(),
    })
}

/// Verifies a dry-run signature against the public key it carries.
///
/// Used by the tests to prove that the signing path is real and not a stub.
pub fn verify_dry_run(result: &DryRunSignature) -> Result<(), WalletError> {
    result.envelope.validate()?;
    let message = result.envelope.canonical_bytes();
    match result.envelope.chain {
        Chain::Evm => {
            let public_key = decode_hex(&result.public_key)?;
            let verifying = k256::ecdsa::VerifyingKey::from_sec1_bytes(&public_key)
                .map_err(|_| WalletError::InvalidKey)?;
            let signature = decode_hex(&result.signature)?;
            if signature.len() != 65 {
                return Err(WalletError::SignatureInvalid);
            }
            let mut r = [0u8; 32];
            r.copy_from_slice(&signature[..32]);
            let mut s = [0u8; 32];
            s.copy_from_slice(&signature[32..64]);
            let sig = k256::ecdsa::Signature::from_scalars(r, s)
                .map_err(|_| WalletError::SignatureInvalid)?;
            let digest = Keccak256::digest(&message);
            verifying
                .verify(&digest, &sig)
                .map_err(|_| WalletError::SignatureInvalid)
        }
        Chain::Solana => {
            let public_key = decode_hex(&result.public_key)?;
            let public_bytes: [u8; 32] =
                public_key.try_into().map_err(|_| WalletError::InvalidKey)?;
            let verifying = ed25519_dalek::VerifyingKey::from_bytes(&public_bytes)
                .map_err(|_| WalletError::InvalidKey)?;
            let signature = bs58::decode(&result.signature)
                .into_vec()
                .map_err(|_| WalletError::SignatureInvalid)?;
            let signature = ed25519_dalek::Signature::from_slice(&signature)
                .map_err(|_| WalletError::SignatureInvalid)?;
            verifying
                .verify(&message, &signature)
                .map_err(|_| WalletError::SignatureInvalid)
        }
    }
}

fn decode_hex(value: &str) -> Result<Vec<u8>, WalletError> {
    hex::decode(value.trim_start_matches("0x")).map_err(|_| WalletError::InvalidKey)
}

fn derive_public(chain: Chain, secret: &[u8]) -> Result<(String, String), WalletError> {
    match chain {
        Chain::Evm => {
            let signing = evm_signing_key(secret)?;
            let point = signing.verifying_key().to_encoded_point(false);
            let address = evm_address(point.as_bytes());
            Ok((format!("0x{}", hex::encode(point.as_bytes())), address))
        }
        Chain::Solana => {
            let signing = solana_signing_key(secret)?;
            let public_bytes = signing.verifying_key().to_bytes();
            Ok((
                hex::encode(public_bytes),
                bs58::encode(public_bytes).into_string(),
            ))
        }
    }
}

fn evm_signing_key(secret: &[u8]) -> Result<k256::ecdsa::SigningKey, WalletError> {
    let key = k256::SecretKey::from_slice(secret).map_err(|_| WalletError::InvalidKey)?;
    Ok(k256::ecdsa::SigningKey::from(&key))
}

fn solana_signing_key(secret: &[u8]) -> Result<ed25519_dalek::SigningKey, WalletError> {
    let seed: [u8; 32] = match secret.len() {
        32 => secret.try_into().map_err(|_| WalletError::InvalidKey)?,
        // Solana keypairs are stored as seed || public key.
        64 => secret[..32]
            .try_into()
            .map_err(|_| WalletError::InvalidKey)?,
        _ => return Err(WalletError::InvalidKey),
    };
    Ok(ed25519_dalek::SigningKey::from_bytes(&seed))
}

/// `keccak256(x || y)[12..]` rendered as an EIP-55 checksummed address.
fn evm_address(uncompressed_pubkey: &[u8]) -> String {
    let body = if uncompressed_pubkey.len() == 65 {
        &uncompressed_pubkey[1..]
    } else {
        uncompressed_pubkey
    };
    let hash = Keccak256::digest(body);
    let lower = hex::encode(&hash[12..]);
    eip55_checksum(&lower)
}

/// EIP-55 mixed case checksum for a lower case, `0x`-less hex address.
fn eip55_checksum(lower_hex: &str) -> String {
    let hash = Keccak256::digest(lower_hex.as_bytes());
    let mut out = String::with_capacity(2 + lower_hex.len());
    out.push_str("0x");
    for (index, ch) in lower_hex.chars().enumerate() {
        let byte = hash[index / 2];
        let nibble = if index % 2 == 0 {
            byte >> 4
        } else {
            byte & 0x0f
        };
        if ch.is_ascii_alphabetic() && nibble >= 8 {
            out.push(ch.to_ascii_uppercase());
        } else {
            out.push(ch);
        }
    }
    out
}

fn chain_label(chain: Chain) -> &'static str {
    match chain {
        Chain::Evm => "evm",
        Chain::Solana => "solana",
    }
}

fn parse_chain_label(value: &str) -> Option<Chain> {
    crate::onchain::parse_chain(value)
}

fn chain_tag(chain: Chain) -> u8 {
    match chain {
        Chain::Evm => 0,
        Chain::Solana => 1,
    }
}

/// `HMAC-SHA256(derived_key, context)`, used to tell a wrong passphrase apart
/// from a corrupted file.
fn key_check_tag(derived_key: &[u8]) -> Vec<u8> {
    let mut mac =
        <HmacSha256 as Mac>::new_from_slice(derived_key).expect("hmac accepts any key length");
    mac.update(KEY_CHECK_CONTEXT);
    mac.finalize().into_bytes().to_vec()
}

/// Argon2id key derivation with the stored parameters.
fn derive_key(
    passphrase: &str,
    salt: &[u8],
    params: KdfParams,
) -> Result<Zeroizing<Vec<u8>>, WalletError> {
    let argon2 = params.argon2()?;
    let mut key = Zeroizing::new(vec![0u8; KEY_LEN]);
    argon2
        .hash_password_into(passphrase.as_bytes(), salt, key.as_mut_slice())
        .map_err(|_| WalletError::UnsupportedFormat("key derivation failed".into()))?;
    Ok(key)
}

/// The plaintext that is encrypted into the keystore.
///
/// The buffer holds the secret, so it is zeroized as soon as it is dropped.
/// Version 2 keystores use `"OEKS2" || u16_be(len) || payload`; the payload
/// is the raw key for a single-key wallet and the mnemonic for an HD wallet.
fn seal_plaintext(payload: &[u8]) -> Zeroizing<Vec<u8>> {
    let mut plaintext = Vec::with_capacity(MAGIC_HD.len() + 2 + payload.len());
    plaintext.extend_from_slice(MAGIC_HD);
    plaintext.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    plaintext.extend_from_slice(payload);
    Zeroizing::new(plaintext)
}

/// Canonical serialisation of the public header, used as AEAD associated data.
///
/// Version 1 keystores keep the v1 context and field order byte for byte, so
/// a file written by an older build still decrypts. Version 2 adds its own
/// context and binds the wallet type and the account list, so neither the
/// wallet type nor the advertised accounts can be swapped after the fact.
fn header_aad(file: &KeystoreFile) -> Vec<u8> {
    let mut aad = Vec::with_capacity(256);
    if file.version >= KEYSTORE_VERSION {
        aad.extend_from_slice(b"openether/keystore/aad/v2");
    } else {
        aad.extend_from_slice(b"openether/keystore/aad/v1");
    }
    aad.extend_from_slice(&file.version.to_be_bytes());
    aad.extend_from_slice(file.kdf.as_bytes());
    aad.extend_from_slice(&file.kdf_params.m_cost_kib.to_be_bytes());
    aad.extend_from_slice(&file.kdf_params.t_cost.to_be_bytes());
    aad.extend_from_slice(&file.kdf_params.p_cost.to_be_bytes());
    aad.extend_from_slice(file.salt.as_bytes());
    aad.extend_from_slice(file.nonce.as_bytes());
    aad.extend_from_slice(file.cipher.as_bytes());
    aad.extend_from_slice(file.chain.as_bytes());
    aad.extend_from_slice(file.address.as_bytes());
    aad.extend_from_slice(file.public_key.as_bytes());
    aad.extend_from_slice(&file.created_at_ms.to_be_bytes());
    if file.version >= KEYSTORE_VERSION {
        aad.extend_from_slice(file.wallet_type.as_bytes());
        for account in &file.accounts {
            aad.extend_from_slice(account.chain.as_bytes());
            aad.push(b'|');
            aad.extend_from_slice(&account.index.to_be_bytes());
            aad.push(b'|');
            aad.extend_from_slice(account.path.as_bytes());
            aad.push(b'|');
            aad.extend_from_slice(account.address.as_bytes());
            aad.push(b'|');
            aad.extend_from_slice(account.public_key.as_bytes());
            aad.push(b';');
        }
    }
    aad
}

fn load_keystore(path: &Path) -> Result<KeystoreFile, WalletError> {
    let raw = std::fs::read(path)?;
    let mut file: KeystoreFile = serde_json::from_slice(&raw)
        .map_err(|e| WalletError::UnsupportedFormat(format!("invalid keystore json: {e}")))?;
    if file.version != KEYSTORE_VERSION && file.version != KEYSTORE_VERSION_V1 {
        return Err(WalletError::UnsupportedFormat(format!(
            "keystore version {} is not supported",
            file.version
        )));
    }
    if file.kdf != KDF_NAME {
        return Err(WalletError::UnsupportedFormat(format!(
            "keystore kdf {} is not supported",
            file.kdf
        )));
    }
    if file.cipher != CIPHER_NAME {
        return Err(WalletError::UnsupportedFormat(format!(
            "keystore cipher {} is not supported",
            file.cipher
        )));
    }
    // A version 1 file predates the wallet type; it always holds one key.
    if file.wallet_type.is_empty() {
        file.wallet_type = WALLET_TYPE_SINGLE.to_string();
    }
    if file.wallet_type != WALLET_TYPE_SINGLE && file.wallet_type != WALLET_TYPE_HD {
        return Err(WalletError::UnsupportedFormat(format!(
            "keystore wallet type {} is not supported",
            file.wallet_type
        )));
    }
    Ok(file)
}

/// Decrypts a keystore and returns the secret material.
///
/// The passphrase is only used to derive the key; it is never stored.
fn open_keystore(file: &KeystoreFile, passphrase: &str) -> Result<KeystoreMaterial, WalletError> {
    let salt = BASE64
        .decode(file.salt.as_bytes())
        .map_err(|_| WalletError::UnsupportedFormat("invalid salt encoding".into()))?;
    let nonce = BASE64
        .decode(file.nonce.as_bytes())
        .map_err(|_| WalletError::UnsupportedFormat("invalid nonce encoding".into()))?;
    let ciphertext = BASE64
        .decode(file.ciphertext.as_bytes())
        .map_err(|_| WalletError::UnsupportedFormat("invalid ciphertext encoding".into()))?;
    let expected_check = BASE64
        .decode(file.key_check.as_bytes())
        .map_err(|_| WalletError::UnsupportedFormat("invalid key check encoding".into()))?;
    if nonce.len() != NONCE_LEN {
        return Err(WalletError::UnsupportedFormat(
            "invalid nonce length".into(),
        ));
    }
    if expected_check.len() != KEY_CHECK_LEN {
        return Err(WalletError::UnsupportedFormat(
            "invalid key check length".into(),
        ));
    }

    let derived = derive_key(passphrase, &salt, file.kdf_params)?;

    // Constant time comparison: a wrong passphrase never yields a usable key.
    let mut mac =
        <HmacSha256 as Mac>::new_from_slice(&derived[..]).expect("hmac accepts any key length");
    mac.update(KEY_CHECK_CONTEXT);
    mac.verify_slice(&expected_check)
        .map_err(|_| WalletError::InvalidPassphrase)?;

    let cipher = Aes256Gcm::new_from_slice(&derived[..])
        .map_err(|_| WalletError::UnsupportedFormat("invalid aes key".into()))?;
    let aad = header_aad(file);
    // The decrypted buffer holds the secret, so it is zeroized on drop.
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| WalletError::DecryptFailed)?,
    );

    if file.version == KEYSTORE_VERSION_V1 {
        parse_plaintext_v1(&plaintext, &file.chain)
    } else {
        parse_plaintext_v2(&plaintext, file)
    }
}

/// Extracts the secret from a version 1 plaintext and checks its integrity.
///
/// The layout is `"OEKS1" || chain_tag || u16_be(len) || secret`.
fn parse_plaintext_v1(
    plaintext: &[u8],
    expected_chain: &str,
) -> Result<KeystoreMaterial, WalletError> {
    if plaintext.len() < MAGIC.len() + 3 || &plaintext[..MAGIC.len()] != MAGIC {
        return Err(WalletError::DecryptFailed);
    }
    let chain_tag = plaintext[MAGIC.len()];
    let len_bytes = [plaintext[MAGIC.len() + 1], plaintext[MAGIC.len() + 2]];
    let secret_len = u16::from_be_bytes(len_bytes) as usize;
    let start = MAGIC.len() + 3;
    let end = start + secret_len;
    if plaintext.len() != end {
        return Err(WalletError::DecryptFailed);
    }
    let chain = match chain_tag {
        0 => "evm",
        1 => "solana",
        _ => return Err(WalletError::DecryptFailed),
    };
    if chain != expected_chain {
        return Err(WalletError::DecryptFailed);
    }
    Ok(KeystoreMaterial::Single(SecretMaterial {
        bytes: Zeroizing::new(plaintext[start..end].to_vec()),
    }))
}

/// Extracts the secret from a version 2 plaintext and checks its integrity.
///
/// The layout is `"OEKS2" || u16_be(len) || payload`. The payload is the raw
/// key of a single-key wallet and the mnemonic of an HD wallet.
fn parse_plaintext_v2(
    plaintext: &[u8],
    file: &KeystoreFile,
) -> Result<KeystoreMaterial, WalletError> {
    if plaintext.len() < MAGIC_HD.len() + 2 || &plaintext[..MAGIC_HD.len()] != MAGIC_HD {
        return Err(WalletError::DecryptFailed);
    }
    let len_bytes = [plaintext[MAGIC_HD.len()], plaintext[MAGIC_HD.len() + 1]];
    let payload_len = u16::from_be_bytes(len_bytes) as usize;
    let start = MAGIC_HD.len() + 2;
    let end = start + payload_len;
    if plaintext.len() != end {
        return Err(WalletError::DecryptFailed);
    }
    let payload = &plaintext[start..end];
    match file.wallet_type.as_str() {
        WALLET_TYPE_SINGLE => Ok(KeystoreMaterial::Single(SecretMaterial {
            bytes: Zeroizing::new(payload.to_vec()),
        })),
        WALLET_TYPE_HD => {
            let words = std::str::from_utf8(payload)
                .map_err(|_| WalletError::DecryptFailed)?
                .trim()
                .to_string();
            Mnemonic::parse_normalized(&words).map_err(|_| WalletError::DecryptFailed)?;
            Ok(KeystoreMaterial::Hd(Zeroizing::new(words)))
        }
        _ => Err(WalletError::UnsupportedFormat(format!(
            "keystore wallet type {} is not supported",
            file.wallet_type
        ))),
    }
}

/// Writes the keystore with restrictive permissions.
fn write_keystore(path: &Path, file: &KeystoreFile) -> Result<(), WalletError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let bytes = serde_json::to_vec_pretty(file)
        .map_err(|e| WalletError::UnsupportedFormat(format!("cannot serialise keystore: {e}")))?;

    let tmp = path.with_extension("tmp");
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut handle = options.open(&tmp)?;
        std::io::Write::write_all(&mut handle, &bytes)?;
        handle.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WalletConfig;
    use std::path::PathBuf;

    /// Obviously fake, runtime generated test keys. Nothing in this module is
    /// a real key and nothing is committed to disk outside of the temporary
    /// directory.
    fn fake_evm_secret() -> Vec<u8> {
        let mut secret = vec![0u8; 32];
        OsRng.try_fill_bytes(&mut secret).unwrap();
        secret
    }

    fn fake_solana_secret() -> Vec<u8> {
        let mut secret = vec![0u8; 32];
        OsRng.try_fill_bytes(&mut secret).unwrap();
        secret
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "openether-wallet-test-{tag}-{}-{}",
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn test_params() -> KdfParams {
        // Cheaper than the default so the test suite stays fast.
        KdfParams {
            m_cost_kib: 8 * 1024,
            t_cost: 1,
            p_cost: 1,
        }
    }

    fn envelope(chain: Chain) -> TxEnvelope {
        TxEnvelope {
            chain,
            nonce: 7,
            timestamp_ms: 1_700_000_000_000,
            to: match chain {
                Chain::Evm => "0x000000000000000000000000000000000000dEaD".to_string(),
                Chain::Solana => "11111111111111111111111111111111".to_string(),
            },
            amount: 1.5,
            amount_raw: 1_500_000,
            fee: 21_000,
            memo: "dry run".to_string(),
        }
    }

    fn wallet_config(path: &Path, chain: Chain) -> WalletConfig {
        WalletConfig {
            keystore_path: Some(path.to_path_buf()),
            chain: Some(chain),
            dry_run: true,
            unlock_timeout_ms: 60_000,
            accounts: 5,
        }
    }

    #[test]
    fn keystore_file_never_contains_the_plaintext_secret() {
        let dir = temp_dir("plaintext");
        let path = dir.join("wallet.json");
        let secret = fake_evm_secret();
        create_keystore(&path, Chain::Evm, &secret, "test-passphrase", test_params()).unwrap();

        let raw = std::fs::read(&path).unwrap();
        assert!(!raw.windows(secret.len()).any(|w| w == secret.as_slice()));
        let hex_secret = hex::encode(&secret);
        assert!(
            !String::from_utf8_lossy(&raw).contains(&hex_secret),
            "the hex encoded secret must not appear in the keystore"
        );
        let b64_secret = BASE64.encode(&secret);
        assert!(
            !String::from_utf8_lossy(&raw).contains(&b64_secret),
            "the base64 encoded secret must not appear in the keystore"
        );
        // The file is still valid json with the expected shape.
        let value: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(value["version"], 2);
        assert_eq!(value["wallet_type"], "single");
        assert_eq!(value["kdf"], "argon2id");
        assert_eq!(value["cipher"], "aes-256-gcm");
        assert!(value["ciphertext"].as_str().unwrap().len() > 16);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unlock_returns_the_secret_for_the_right_passphrase_only() {
        let dir = temp_dir("unlock");
        let path = dir.join("wallet.json");
        let secret = fake_evm_secret();
        create_keystore(
            &path,
            Chain::Evm,
            &secret,
            "correct-passphrase",
            test_params(),
        )
        .unwrap();

        let file = load_keystore(&path).unwrap();
        let opened = open_keystore(&file, "correct-passphrase").unwrap();
        match opened {
            KeystoreMaterial::Single(material) => {
                assert_eq!(&material.bytes[..], &secret[..])
            }
            KeystoreMaterial::Hd(_) => panic!("a single key keystore must not open as hd"),
        }

        let wrong = open_keystore(&file, "wrong-passphrase").unwrap_err();
        assert!(matches!(wrong, WalletError::InvalidPassphrase));
        // A wrong passphrase must not yield any usable key material.
        assert!(open_keystore(&file, "wrong-passphrase").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tampered_ciphertext_fails_to_decrypt() {
        let dir = temp_dir("tamper");
        let path = dir.join("wallet.json");
        let secret = fake_evm_secret();
        create_keystore(&path, Chain::Evm, &secret, "test-passphrase", test_params()).unwrap();

        let mut file = load_keystore(&path).unwrap();
        let mut ciphertext = BASE64.decode(file.ciphertext.as_bytes()).unwrap();
        let last = ciphertext.len() - 1;
        ciphertext[last] ^= 0xff;
        file.ciphertext = BASE64.encode(&ciphertext);

        let error = open_keystore(&file, "test-passphrase").unwrap_err();
        assert!(matches!(error, WalletError::DecryptFailed));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dry_run_signature_round_trips_and_verifies() {
        let dir = temp_dir("sign");
        let path = dir.join("wallet.json");
        let secret = fake_evm_secret();
        let metadata =
            create_keystore(&path, Chain::Evm, &secret, "test-passphrase", test_params()).unwrap();

        let service = WalletService::new(wallet_config(&path, Chain::Evm));
        let result = service
            .sign_dry_run_with_passphrase(
                "test-passphrase",
                &envelope(Chain::Evm),
                &AccountSelector::default(),
            )
            .unwrap();

        assert!(result.dry_run);
        assert!(!result.broadcast);
        assert_eq!(result.chain, Chain::Evm);
        assert_eq!(result.address, metadata.address.clone().unwrap());
        assert!(result.tx_hash.starts_with("0x"));
        assert!(result.signature.starts_with("0x"));
        verify_dry_run(&result).expect("the signature must verify");

        // A different envelope must not verify against the same signature.
        let mut tampered = result.clone();
        tampered.envelope.amount_raw += 1;
        assert!(verify_dry_run(&tampered).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn solana_dry_run_signature_round_trips_and_verifies() {
        let dir = temp_dir("sign-solana");
        let path = dir.join("wallet.json");
        let secret = fake_solana_secret();
        create_keystore(
            &path,
            Chain::Solana,
            &secret,
            "test-passphrase",
            test_params(),
        )
        .unwrap();

        let service = WalletService::new(wallet_config(&path, Chain::Solana));
        let result = service
            .sign_dry_run_with_passphrase(
                "test-passphrase",
                &envelope(Chain::Solana),
                &AccountSelector::default(),
            )
            .unwrap();
        assert!(result.dry_run);
        assert!(!result.broadcast);
        verify_dry_run(&result).expect("the signature must verify");

        // The address is the base58 encoding of the public key.
        let decoded = bs58::decode(&result.address).into_vec().unwrap();
        assert_eq!(decoded.len(), 32);
        assert_eq!(hex::encode(decoded), result.public_key);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn metadata_contains_no_secret_material() {
        let dir = temp_dir("metadata");
        let path = dir.join("wallet.json");
        let secret = fake_evm_secret();
        create_keystore(&path, Chain::Evm, &secret, "test-passphrase", test_params()).unwrap();

        let service = WalletService::new(wallet_config(&path, Chain::Evm));
        let metadata = service.metadata();
        assert!(metadata.configured);
        assert_eq!(metadata.chain.as_deref(), Some("evm"));
        assert!(metadata.encrypted);
        assert!(metadata.dry_run);

        let json = serde_json::to_string(&metadata).unwrap();
        assert!(!json.contains(&hex::encode(&secret)));
        assert!(!json.contains(&BASE64.encode(&secret)));
        assert!(!json.to_lowercase().contains("secret"));
        assert!(!json.to_lowercase().contains("passphrase"));
        assert!(!json.to_lowercase().contains("ciphertext"));
        assert!(!json.to_lowercase().contains("private"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unlock_lease_expires_and_wipes_the_key() {
        let dir = temp_dir("lease");
        let path = dir.join("wallet.json");
        let secret = fake_evm_secret();
        create_keystore(&path, Chain::Evm, &secret, "test-passphrase", test_params()).unwrap();

        let mut config = wallet_config(&path, Chain::Evm);
        config.unlock_timeout_ms = 40;
        let service = WalletService::new(config);

        service.unlock("test-passphrase").unwrap();
        assert!(service.lease_expires_at_ms().is_some());
        assert!(service
            .sign_dry_run(&envelope(Chain::Evm), &AccountSelector::default())
            .is_ok());

        std::thread::sleep(std::time::Duration::from_millis(80));
        assert!(service.reap_expired());
        assert!(service.lease_expires_at_ms().is_none());
        let error = service
            .sign_dry_run(&envelope(Chain::Evm), &AccountSelector::default())
            .unwrap_err();
        assert!(matches!(error, WalletError::LeaseExpired));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn the_background_reaper_wipes_an_unused_key() {
        let dir = temp_dir("reaper");
        let path = dir.join("wallet.json");
        let secret = fake_evm_secret();
        create_keystore(&path, Chain::Evm, &secret, "test-passphrase", test_params()).unwrap();

        let mut config = wallet_config(&path, Chain::Evm);
        config.unlock_timeout_ms = 50;
        let service = Arc::new(WalletService::new(config));
        service.spawn_key_reaper();
        service.unlock("test-passphrase").unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(service.lease_expires_at_ms().is_none());
        let error = service
            .sign_dry_run(&envelope(Chain::Evm), &AccountSelector::default())
            .unwrap_err();
        assert!(matches!(error, WalletError::LeaseExpired));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn signing_is_refused_when_dry_run_is_disabled() {
        let dir = temp_dir("dry-run-off");
        let path = dir.join("wallet.json");
        let secret = fake_evm_secret();
        create_keystore(&path, Chain::Evm, &secret, "test-passphrase", test_params()).unwrap();

        let mut config = wallet_config(&path, Chain::Evm);
        config.dry_run = false;
        let service = WalletService::new(config);
        let error = service
            .sign_dry_run_with_passphrase(
                "test-passphrase",
                &envelope(Chain::Evm),
                &AccountSelector::default(),
            )
            .unwrap_err();
        assert!(matches!(error, WalletError::LiveBroadcastUnsupported));
        assert!(!service.metadata().dry_run);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_chain_mismatch_is_refused() {
        let dir = temp_dir("chain");
        let path = dir.join("wallet.json");
        let secret = fake_evm_secret();
        create_keystore(&path, Chain::Evm, &secret, "test-passphrase", test_params()).unwrap();

        let service = WalletService::new(wallet_config(&path, Chain::Evm));
        let error = service
            .sign_dry_run_with_passphrase(
                "test-passphrase",
                &envelope(Chain::Solana),
                &AccountSelector::default(),
            )
            .unwrap_err();
        assert!(matches!(error, WalletError::ChainMismatch { .. }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unconfigured_wallet_refuses_to_sign() {
        let config = WalletConfig {
            keystore_path: None,
            chain: None,
            dry_run: true,
            unlock_timeout_ms: 1_000,
            accounts: 5,
        };
        let service = WalletService::new(config);
        assert!(!service.is_configured());
        let metadata = service.metadata();
        assert!(!metadata.configured);
        assert!(metadata.chain.is_none());
        assert!(metadata.address.is_none());
        let error = service
            .sign_dry_run_with_passphrase(
                "whatever",
                &envelope(Chain::Evm),
                &AccountSelector::default(),
            )
            .unwrap_err();
        assert!(matches!(error, WalletError::NotConfigured));
    }

    #[test]
    fn the_metadata_is_readable_without_unlocking() {
        let dir = temp_dir("no-unlock");
        let path = dir.join("wallet.json");
        let secret = fake_evm_secret();
        let created =
            create_keystore(&path, Chain::Evm, &secret, "test-passphrase", test_params()).unwrap();

        let metadata = read_keystore_metadata(&path).unwrap();
        assert_eq!(metadata.address, created.address);
        assert_eq!(metadata.public_key, created.public_key);
        assert_eq!(metadata.chain.as_deref(), Some("evm"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn evm_addresses_use_the_eip55_checksum() {
        // Reference vector taken from EIP-55 itself. It is a public address,
        // not a key.
        assert_eq!(
            eip55_checksum("5aaeb6053f3e94c9b9a09f33669435e7ef1beaed"),
            "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed"
        );
    }

    #[test]
    fn base58_encoding_is_correct() {
        // Reference vector for base58, not a key.
        assert_eq!(
            bs58::encode(b"hello world").into_string(),
            "StV1DL6CwTryKyV"
        );
    }

    /// The public BIP-39 test vector. It is a published word list, not a key.
    const TEST_MNEMONIC: &str =
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    /// Writes a version 1 keystore by hand, the way an older build would.
    fn write_v1_keystore(
        path: &Path,
        chain: Chain,
        secret: &[u8],
        passphrase: &str,
        params: KdfParams,
    ) {
        let (public_key, address) = derive_public(chain, secret).unwrap();
        let mut salt = [0u8; SALT_LEN];
        let mut nonce = [0u8; NONCE_LEN];
        OsRng.try_fill_bytes(&mut salt).unwrap();
        OsRng.try_fill_bytes(&mut nonce).unwrap();
        let derived = derive_key(passphrase, &salt, params).unwrap();

        let mut file = KeystoreFile {
            version: KEYSTORE_VERSION_V1,
            kdf: KDF_NAME.to_string(),
            kdf_params: params,
            salt: BASE64.encode(salt),
            nonce: BASE64.encode(nonce),
            cipher: CIPHER_NAME.to_string(),
            ciphertext: String::new(),
            key_check: BASE64.encode(key_check_tag(&derived)),
            chain: chain_label(chain).to_string(),
            address: address.clone(),
            public_key: public_key.clone(),
            created_at_ms: now_ms(),
            wallet_type: String::new(),
            accounts: vec![AccountEntry {
                chain: chain_label(chain).to_string(),
                index: 0,
                path: String::new(),
                address,
                public_key,
            }],
        };

        let mut plaintext = Vec::new();
        plaintext.extend_from_slice(MAGIC);
        plaintext.push(chain_tag(chain));
        plaintext.extend_from_slice(&(secret.len() as u16).to_be_bytes());
        plaintext.extend_from_slice(secret);
        let plaintext = Zeroizing::new(plaintext);
        encrypt_keystore(&mut file, &plaintext, &derived);
        write_keystore(path, &file).unwrap();
    }

    #[test]
    fn a_version_1_keystore_still_opens_and_signs() {
        let dir = temp_dir("v1");
        let path = dir.join("wallet.json");
        let secret = fake_evm_secret();
        write_v1_keystore(&path, Chain::Evm, &secret, "test-passphrase", test_params());

        let file = load_keystore(&path).unwrap();
        assert_eq!(file.version, KEYSTORE_VERSION_V1);
        // A version 1 file has no wallet type; it is treated as a single key.
        assert_eq!(file.wallet_type, WALLET_TYPE_SINGLE);

        match open_keystore(&file, "test-passphrase").unwrap() {
            KeystoreMaterial::Single(material) => {
                assert_eq!(&material.bytes[..], &secret[..])
            }
            KeystoreMaterial::Hd(_) => panic!("a version 1 keystore must not open as hd"),
        }

        let service = WalletService::new(wallet_config(&path, Chain::Evm));
        let result = service
            .sign_dry_run_with_passphrase(
                "test-passphrase",
                &envelope(Chain::Evm),
                &AccountSelector::default(),
            )
            .unwrap();
        verify_dry_run(&result).expect("the signature must verify");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_hd_keystore_lists_every_account() {
        let dir = temp_dir("hd-list");
        let path = dir.join("wallet.json");
        let metadata = create_hd_keystore(
            &path,
            TEST_MNEMONIC,
            "test-passphrase",
            Chain::Evm,
            3,
            test_params(),
        )
        .unwrap();

        assert_eq!(metadata.wallet_type.as_deref(), Some(WALLET_TYPE_HD));
        assert_eq!(metadata.accounts.len(), 6);
        let evm = metadata
            .accounts
            .iter()
            .find(|a| a.chain == "evm" && a.index == 2)
            .unwrap();
        assert_eq!(evm.path, "m/44'/60'/0'/0/2");
        let solana = metadata
            .accounts
            .iter()
            .find(|a| a.chain == "solana" && a.index == 1)
            .unwrap();
        assert_eq!(solana.path, "m/44'/501'/1'/0'");
        // Every account must have its own address.
        let mut addresses: Vec<&str> = metadata
            .accounts
            .iter()
            .map(|a| a.address.as_str())
            .collect();
        addresses.sort_unstable();
        addresses.dedup();
        assert_eq!(addresses.len(), metadata.accounts.len());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_hd_keystore_never_stores_the_mnemonic_in_clear_text() {
        let dir = temp_dir("hd-plaintext");
        let path = dir.join("wallet.json");
        create_hd_keystore(
            &path,
            TEST_MNEMONIC,
            "test-passphrase",
            Chain::Evm,
            2,
            test_params(),
        )
        .unwrap();

        let raw = String::from_utf8_lossy(&std::fs::read(&path).unwrap()).to_string();
        assert!(
            !raw.contains("abandon"),
            "a mnemonic word leaked into the file"
        );
        assert!(!raw.contains(TEST_MNEMONIC));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_hd_keystore_signs_with_the_selected_account() {
        let dir = temp_dir("hd-sign");
        let path = dir.join("wallet.json");
        let metadata = create_hd_keystore(
            &path,
            TEST_MNEMONIC,
            "test-passphrase",
            Chain::Evm,
            3,
            test_params(),
        )
        .unwrap();

        let service = WalletService::new(wallet_config(&path, Chain::Evm));
        let selector = AccountSelector {
            chain: Some("evm".to_string()),
            index: Some(2),
        };
        let result = service
            .sign_dry_run_with_passphrase("test-passphrase", &envelope(Chain::Evm), &selector)
            .unwrap();

        let expected = metadata
            .accounts
            .iter()
            .find(|a| a.chain == "evm" && a.index == 2)
            .unwrap();
        assert_eq!(result.address, expected.address);
        assert_eq!(result.account.index, 2);
        assert_eq!(result.account.path, "m/44'/60'/0'/0/2");
        verify_dry_run(&result).expect("the signature must verify");

        // A different account must produce a different address.
        let other = service
            .sign_dry_run_with_passphrase(
                "test-passphrase",
                &envelope(Chain::Evm),
                &AccountSelector {
                    chain: Some("evm".to_string()),
                    index: Some(0),
                },
            )
            .unwrap();
        assert_ne!(other.address, result.address);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_hd_keystore_refuses_a_chain_mismatch() {
        let dir = temp_dir("hd-chain");
        let path = dir.join("wallet.json");
        create_hd_keystore(
            &path,
            TEST_MNEMONIC,
            "test-passphrase",
            Chain::Evm,
            2,
            test_params(),
        )
        .unwrap();

        let service = WalletService::new(wallet_config(&path, Chain::Evm));
        let error = service
            .sign_dry_run_with_passphrase(
                "test-passphrase",
                &envelope(Chain::Evm),
                &AccountSelector {
                    chain: Some("solana".to_string()),
                    index: Some(0),
                },
            )
            .unwrap_err();
        assert!(matches!(error, WalletError::ChainMismatch { .. }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_single_key_keystore_refuses_a_second_account() {
        let dir = temp_dir("single-account");
        let path = dir.join("wallet.json");
        let secret = fake_evm_secret();
        create_keystore(&path, Chain::Evm, &secret, "test-passphrase", test_params()).unwrap();

        let service = WalletService::new(wallet_config(&path, Chain::Evm));
        let error = service
            .sign_dry_run_with_passphrase(
                "test-passphrase",
                &envelope(Chain::Evm),
                &AccountSelector {
                    chain: Some("evm".to_string()),
                    index: Some(1),
                },
            )
            .unwrap_err();
        assert!(matches!(error, WalletError::AccountNotFound(_)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_tampered_account_list_breaks_decryption() {
        let dir = temp_dir("hd-tamper");
        let path = dir.join("wallet.json");
        create_hd_keystore(
            &path,
            TEST_MNEMONIC,
            "test-passphrase",
            Chain::Evm,
            2,
            test_params(),
        )
        .unwrap();

        let mut file = load_keystore(&path).unwrap();
        file.accounts[0].address = "0x000000000000000000000000000000000000dEaD".to_string();

        let error = open_keystore(&file, "test-passphrase").unwrap_err();
        assert!(matches!(error, WalletError::DecryptFailed));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_generated_mnemonic_restores_the_same_wallet() {
        let dir = temp_dir("hd-restore");
        let first = dir.join("first.json");
        let second = dir.join("second.json");

        let (mnemonic, metadata) =
            generate_hd_keystore(&first, "test-passphrase", Chain::Evm, 2, test_params()).unwrap();
        assert_eq!(mnemonic.split(' ').count(), 12);

        let restored = create_hd_keystore(
            &second,
            &mnemonic,
            "test-passphrase",
            Chain::Evm,
            2,
            test_params(),
        )
        .unwrap();
        assert_eq!(restored.accounts.len(), metadata.accounts.len());
        for (a, b) in restored.accounts.iter().zip(metadata.accounts.iter()) {
            assert_eq!(a.address, b.address);
            assert_eq!(a.path, b.path);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_invalid_mnemonic_is_refused() {
        let dir = temp_dir("hd-invalid");
        let path = dir.join("wallet.json");
        let error = create_hd_keystore(
            &path,
            "not actually a bip39 mnemonic at all",
            "test-passphrase",
            Chain::Evm,
            2,
            test_params(),
        )
        .unwrap_err();
        assert!(matches!(error, WalletError::UnsupportedFormat(_)));
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}

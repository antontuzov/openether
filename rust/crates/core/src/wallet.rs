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
//! A keystore is a single JSON document:
//!
//! ```json
//! {
//!   "version": 1,
//!   "kdf": "argon2id",
//!   "kdf_params": { "m_cost_kib": 19456, "t_cost": 2, "p_cost": 1 },
//!   "salt": "<base64 of 16 random bytes>",
//!   "nonce": "<base64 of 12 random bytes>",
//!   "cipher": "aes-256-gcm",
//!   "ciphertext": "<base64 of the encrypted secret>",
//!   "key_check": "<base64 of a 32 byte HMAC-SHA256 tag>",
//!   "chain": "evm",
//!   "address": "0x…",
//!   "public_key": "0x…",
//!   "created_at_ms": 1730000000000
//! }
//! ```
//!
//! * `kdf` is always `argon2id` (Argon2id, version 0x13) with the parameters in
//!   `kdf_params`. The parameters are stored so that a keystore written by an
//!   older build can still be unlocked.
//! * `salt` is 16 random bytes; `nonce` is the 12 byte GCM nonce.
//! * `ciphertext` is the AES-256-GCM encryption of
//!   `"OEKS1" || chain_tag || u16_be(len) || secret`.
//! * The AEAD associated data is a canonical serialisation of every public
//!   header field except `ciphertext` and `key_check`, so the ciphertext is
//!   bound to the address, chain and KDF parameters it was created with.
//! * `key_check` is `HMAC-SHA256(derived_key, "openether/keystore/key-check/v1")`
//!   and exists only to tell a wrong passphrase apart from a corrupted file.
//!   It is verified in constant time with `hmac::Mac::verify_slice`. It adds
//!   no attack surface beyond the GCM tag, which is offline verifiable anyway.
//! * The file is written with mode `0600` and contains no secret material in
//!   clear text: `address`, `public_key` and `chain` are public by design.
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
use crate::types::{now_ms, Chain};
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
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

/// Keystore format version understood by this build.
pub const KEYSTORE_VERSION: u16 = 1;
/// Name of the key derivation function recorded in the keystore.
pub const KDF_NAME: &str = "argon2id";
/// Name of the AEAD recorded in the keystore.
pub const CIPHER_NAME: &str = "aes-256-gcm";

const MAGIC: &[u8; 5] = b"OEKS1";
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;
const KEY_CHECK_LEN: usize = 32;
const KEY_CHECK_CONTEXT: &[u8] = b"openether/keystore/key-check/v1";

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
            .finish()
    }
}

impl Serialize for KeystoreFile {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("KeystoreFile", 13)?;
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
    /// When the signature was produced.
    pub signed_at_ms: i64,
    /// Human readable reminder that nothing was sent.
    pub note: String,
}

/// An unlocked key with a hard expiry.
struct KeyLease {
    chain: Chain,
    secret: SecretMaterial,
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
                    if let Some(chain) = config.chain {
                        if chain_label(chain) != file.chain {
                            tracing::warn!(
                                configured_chain = chain_label(chain),
                                keystore_chain = %file.chain,
                                "the configured wallet chain does not match the keystore; the keystore wins"
                            );
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
        }
    }

    /// Unlocks the keystore and starts an unlock lease.
    ///
    /// The lease expires after `unlock_timeout_ms`; the key is then wiped from
    /// memory. Repeated calls do not extend an existing lease.
    pub fn unlock(&self, passphrase: &str) -> Result<(), WalletError> {
        let keystore = self.keystore.as_ref().ok_or(WalletError::NotConfigured)?;
        let secret = open_keystore(keystore, passphrase)?;
        let chain = parse_chain_label(&keystore.chain).ok_or_else(|| {
            WalletError::UnsupportedFormat(format!("unknown chain: {}", keystore.chain))
        })?;
        let expires_at_ms = now_ms() + self.config.unlock_timeout_ms.max(1);
        let mut state = self.lock_lease();
        state.lease = Some(KeyLease {
            chain,
            secret,
            expires_at_ms,
        });
        state.expired = false;
        drop(state);
        tracing::info!(
            chain = chain_label(chain),
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
    pub fn sign_dry_run(&self, envelope: &TxEnvelope) -> Result<DryRunSignature, WalletError> {
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
        sign_with_secret(lease.chain, &lease.secret.bytes, envelope)
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
    ) -> Result<DryRunSignature, WalletError> {
        if !self.config.dry_run {
            return Err(WalletError::LiveBroadcastUnsupported);
        }
        let keystore = self.keystore.as_ref().ok_or(WalletError::NotConfigured)?;
        let chain = parse_chain_label(&keystore.chain).ok_or_else(|| {
            WalletError::UnsupportedFormat(format!("unknown chain: {}", keystore.chain))
        })?;
        let secret = open_keystore(keystore, passphrase)?;
        sign_with_secret(chain, &secret.bytes[..], envelope)
    }

    fn lock_lease(&self) -> std::sync::MutexGuard<'_, LeaseState> {
        self.lease.lock().unwrap_or_else(|e| e.into_inner())
    }
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

    let plaintext = seal_plaintext(chain, secret);
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
    };

    let aad = header_aad(&file);
    let cipher = Aes256Gcm::new_from_slice(&derived[..])
        .map_err(|_| WalletError::UnsupportedFormat("invalid aes key".into()))?;
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| WalletError::UnsupportedFormat("encryption failed".into()))?;
    file.ciphertext = BASE64.encode(ciphertext);

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
    })
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
    })
}

/// Signs `envelope` with `secret` and returns the dry-run result.
fn sign_with_secret(
    chain: Chain,
    secret: &[u8],
    envelope: &TxEnvelope,
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
        address,
        public_key,
        tx_hash,
        signature,
        envelope: envelope.clone(),
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
fn seal_plaintext(chain: Chain, secret: &[u8]) -> Zeroizing<Vec<u8>> {
    let mut plaintext = Vec::with_capacity(MAGIC.len() + 3 + secret.len());
    plaintext.extend_from_slice(MAGIC);
    plaintext.push(chain_tag(chain));
    plaintext.extend_from_slice(&(secret.len() as u16).to_be_bytes());
    plaintext.extend_from_slice(secret);
    Zeroizing::new(plaintext)
}

/// Canonical serialisation of the public header, used as AEAD associated data.
fn header_aad(file: &KeystoreFile) -> Vec<u8> {
    let mut aad = Vec::with_capacity(160);
    aad.extend_from_slice(b"openether/keystore/aad/v1");
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
    aad
}

fn load_keystore(path: &Path) -> Result<KeystoreFile, WalletError> {
    let raw = std::fs::read(path)?;
    let file: KeystoreFile = serde_json::from_slice(&raw)
        .map_err(|e| WalletError::UnsupportedFormat(format!("invalid keystore json: {e}")))?;
    if file.version != KEYSTORE_VERSION {
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
    Ok(file)
}

/// Decrypts a keystore and returns the secret material.
///
/// The passphrase is only used to derive the key; it is never stored.
fn open_keystore(file: &KeystoreFile, passphrase: &str) -> Result<SecretMaterial, WalletError> {
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

    parse_plaintext(&plaintext, &file.chain)
}

/// Extracts the secret from the decrypted plaintext and checks its integrity.
fn parse_plaintext(plaintext: &[u8], expected_chain: &str) -> Result<SecretMaterial, WalletError> {
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
    Ok(SecretMaterial {
        bytes: Zeroizing::new(plaintext[start..end].to_vec()),
    })
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
        assert_eq!(value["version"], 1);
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
        assert_eq!(&opened.bytes[..], &secret[..]);

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
            .sign_dry_run_with_passphrase("test-passphrase", &envelope(Chain::Evm))
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
            .sign_dry_run_with_passphrase("test-passphrase", &envelope(Chain::Solana))
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
        assert!(service.sign_dry_run(&envelope(Chain::Evm)).is_ok());

        std::thread::sleep(std::time::Duration::from_millis(80));
        assert!(service.reap_expired());
        assert!(service.lease_expires_at_ms().is_none());
        let error = service.sign_dry_run(&envelope(Chain::Evm)).unwrap_err();
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
        let error = service.sign_dry_run(&envelope(Chain::Evm)).unwrap_err();
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
            .sign_dry_run_with_passphrase("test-passphrase", &envelope(Chain::Evm))
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
            .sign_dry_run_with_passphrase("test-passphrase", &envelope(Chain::Solana))
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
        };
        let service = WalletService::new(config);
        assert!(!service.is_configured());
        let metadata = service.metadata();
        assert!(!metadata.configured);
        assert!(metadata.chain.is_none());
        assert!(metadata.address.is_none());
        let error = service
            .sign_dry_run_with_passphrase("whatever", &envelope(Chain::Evm))
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
}

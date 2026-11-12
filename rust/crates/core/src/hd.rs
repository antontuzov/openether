//! Hierarchical deterministic key derivation.
//!
//! One seed, several accounts, several chains:
//!
//! * **BIP-39** turns a mnemonic into a 64 byte seed,
//! * **BIP-32** derives secp256k1 keys for EVM accounts (BIP-44 paths),
//! * **SLIP-0010** derives ed25519 keys for Solana accounts.
//!
//! Nothing here touches the network or the filesystem. It is pure derivation,
//! so the wallet's dry-run path can use it without exposing anything beyond
//! the private keys it already had.

use bip39::Mnemonic;
use hmac::{Hmac, Mac};
use k256::elliptic_curve::sec1::ToEncodedPoint;
use k256::elliptic_curve::PrimeField;
use k256::{FieldBytes, Scalar, SecretKey};
use sha2::Sha512;
use std::fmt;
use zeroize::Zeroizing;

type HmacSha512 = Hmac<Sha512>;

/// The high bit of a BIP-32 index marks a hardened child.
const HARDENED: u32 = 0x8000_0000;

/// BIP-44 coin type for Ethereum and every other EVM chain.
pub const EVM_COIN_TYPE: u32 = 60;
/// SLIP-44 coin type for Solana.
pub const SOLANA_COIN_TYPE: u32 = 501;

/// Everything that can go wrong while deriving a key.
#[derive(Debug, thiserror::Error)]
pub enum HdError {
    #[error("invalid mnemonic: {0}")]
    InvalidMnemonic(String),
    #[error("invalid derivation path: {0}")]
    InvalidPath(String),
    #[error("key derivation failed: {0}")]
    Derivation(String),
}

/// A BIP-32 derivation path.
///
/// Each element carries the hardened flag in its high bit, exactly like the
/// indices BIP-32 serialises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivationPath(Vec<u32>);

impl DerivationPath {
    /// The BIP-44 path of an EVM account: `m/44'/60'/0'/0/{index}`.
    pub fn evm(index: u32) -> Self {
        Self(vec![
            44 | HARDENED,
            EVM_COIN_TYPE | HARDENED,
            HARDENED,
            0,
            index,
        ])
    }

    /// The SLIP-44 path of a Solana account: `m/44'/501'/{index}'/0'`.
    ///
    /// ed25519 only supports hardened derivation, so every element is
    /// hardened.
    pub fn solana(index: u32) -> Self {
        Self(vec![
            44 | HARDENED,
            SOLANA_COIN_TYPE | HARDENED,
            index | HARDENED,
            HARDENED,
        ])
    }

    /// Parses a path such as `m/44'/60'/0'/0/0`.
    ///
    /// `h` and `H` are accepted as hardened markers too, because that is what
    /// some wallets write.
    pub fn parse(raw: &str) -> Result<Self, HdError> {
        let raw = raw.trim();
        let mut parts = raw.split('/');
        let head = parts.next().unwrap_or_default();
        if head != "m" && head != "M" {
            return Err(HdError::InvalidPath(format!(
                "a derivation path must start with 'm': {raw}"
            )));
        }
        let mut indices = Vec::new();
        for part in parts {
            if part.is_empty() {
                return Err(HdError::InvalidPath(format!(
                    "the path has an empty element: {raw}"
                )));
            }
            let (digits, hardened) = match part
                .strip_suffix('\'')
                .or_else(|| part.strip_suffix('h'))
                .or_else(|| part.strip_suffix('H'))
            {
                Some(rest) => (rest, true),
                None => (part, false),
            };
            let index: u32 = digits.parse().map_err(|_| {
                HdError::InvalidPath(format!("'{digits}' is not a number in {raw}"))
            })?;
            if index >= HARDENED {
                return Err(HdError::InvalidPath(format!(
                    "the index {index} is out of range in {raw}"
                )));
            }
            indices.push(if hardened { index | HARDENED } else { index });
        }
        if indices.is_empty() {
            return Err(HdError::InvalidPath(format!(
                "a derivation path must have at least one element: {raw}"
            )));
        }
        Ok(Self(indices))
    }

    /// The raw indices, hardened flag included.
    pub fn indices(&self) -> &[u32] {
        &self.0
    }
}

impl fmt::Display for DerivationPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("m")?;
        for index in &self.0 {
            write!(f, "/{}", index & !HARDENED)?;
            if index & HARDENED != 0 {
                f.write_str("'")?;
            }
        }
        Ok(())
    }
}

/// A node of a BIP-32 tree: a private key and its chain code.
struct Bip32Node {
    key: Zeroizing<[u8; 32]>,
    chain_code: [u8; 32],
}

/// A hierarchical deterministic wallet: one seed, many accounts.
pub struct HdWallet {
    seed: Zeroizing<[u8; 64]>,
}

impl fmt::Debug for HdWallet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HdWallet(<redacted>)")
    }
}

impl HdWallet {
    /// Builds a wallet from a seed.
    pub fn from_seed(seed: [u8; 64]) -> Self {
        Self {
            seed: Zeroizing::new(seed),
        }
    }

    /// Builds a wallet from a BIP-39 mnemonic and its optional passphrase.
    ///
    /// The mnemonic is validated, so a typo is rejected instead of silently
    /// deriving a different wallet.
    pub fn from_mnemonic(mnemonic: &str, passphrase: &str) -> Result<Self, HdError> {
        let parsed = Mnemonic::parse_normalized(mnemonic)
            .map_err(|e| HdError::InvalidMnemonic(e.to_string()))?;
        Ok(Self::from_seed(parsed.to_seed(passphrase)))
    }

    /// Builds a wallet from raw entropy and returns the mnemonic with it.
    ///
    /// The mnemonic is the only thing the operator can restore the wallet
    /// from, so it is handed back exactly once, at creation time.
    pub fn from_entropy(entropy: &[u8]) -> Result<(String, Self), HdError> {
        let mnemonic =
            Mnemonic::from_entropy(entropy).map_err(|e| HdError::InvalidMnemonic(e.to_string()))?;
        let words = mnemonic.to_string();
        Ok((words, Self::from_seed(mnemonic.to_seed(""))))
    }

    /// Derives an EVM private key (secp256k1) at `path`.
    pub fn derive_evm(&self, path: &DerivationPath) -> Result<Zeroizing<[u8; 32]>, HdError> {
        let mut node = bip32_master(&self.seed[..])?;
        for index in path.indices() {
            node = bip32_ckd(&node, *index)?;
        }
        Ok(node.key)
    }

    /// Derives a Solana private key (ed25519) at `path`.
    pub fn derive_solana(&self, path: &DerivationPath) -> Result<Zeroizing<[u8; 32]>, HdError> {
        let (mut key, mut chain_code) = slip10_master(&self.seed[..])?;
        for index in path.indices() {
            if index & HARDENED == 0 {
                return Err(HdError::Derivation(
                    "ed25519 derivation only supports hardened children".into(),
                ));
            }
            let (child, next_chain_code) = slip10_ckd(&key, &chain_code, *index)?;
            key = child;
            chain_code = next_chain_code;
        }
        Ok(key)
    }
}

/// Derives the BIP-32 master node from a seed.
fn bip32_master(seed: &[u8]) -> Result<Bip32Node, HdError> {
    let mut mac = <HmacSha512 as Mac>::new_from_slice(b"Bitcoin seed")
        .map_err(|_| HdError::Derivation("hmac rejected the master key".into()))?;
    mac.update(seed);
    let out = mac.finalize().into_bytes();
    let mut key = [0u8; 32];
    key.copy_from_slice(&out[..32]);
    let mut chain_code = [0u8; 32];
    chain_code.copy_from_slice(&out[32..]);
    if scalar_from_bytes(key).is_none() {
        return Err(HdError::Derivation(
            "the seed does not produce a valid secp256k1 key".into(),
        ));
    }
    Ok(Bip32Node {
        key: Zeroizing::new(key),
        chain_code,
    })
}

/// Derives one BIP-32 child of `parent`.
///
/// BIP-32 says that a child whose tweak is out of range does not exist and
/// that the derivation should continue with the next index. The odds of that
/// are astronomical, but the rule is cheap to honour.
fn bip32_ckd(parent: &Bip32Node, index: u32) -> Result<Bip32Node, HdError> {
    let mut attempt = index;
    for _ in 0..256 {
        match bip32_ckd_at(parent, attempt) {
            Ok(node) => return Ok(node),
            Err(e) => {
                let next = attempt.checked_add(1).ok_or(e)?;
                // Keep the hardened flag: a hardened child never becomes a
                // soft one.
                attempt = if index & HARDENED != 0 {
                    next | HARDENED
                } else {
                    next & !HARDENED
                };
            }
        }
    }
    Err(HdError::Derivation(format!(
        "no valid child key at index {index}"
    )))
}

fn bip32_ckd_at(parent: &Bip32Node, index: u32) -> Result<Bip32Node, HdError> {
    let mut data = Vec::with_capacity(37);
    if index & HARDENED != 0 {
        data.push(0x00);
        data.extend_from_slice(&parent.key[..]);
    } else {
        let secret = SecretKey::from_slice(&parent.key[..])
            .map_err(|_| HdError::Derivation("the parent key is invalid".into()))?;
        let public = secret.public_key().to_encoded_point(true);
        data.extend_from_slice(public.as_bytes());
    }
    data.extend_from_slice(&index.to_be_bytes());

    let mut mac = <HmacSha512 as Mac>::new_from_slice(&parent.chain_code)
        .map_err(|_| HdError::Derivation("hmac rejected the chain code".into()))?;
    mac.update(&data);
    let out = mac.finalize().into_bytes();

    let mut tweak_bytes = [0u8; 32];
    tweak_bytes.copy_from_slice(&out[..32]);
    let mut chain_code = [0u8; 32];
    chain_code.copy_from_slice(&out[32..]);

    let Some(tweak) = scalar_from_bytes(tweak_bytes) else {
        return Err(HdError::Derivation("the tweak is out of range".into()));
    };
    let Some(parent_scalar) = scalar_from_bytes(*parent.key) else {
        return Err(HdError::Derivation("the parent key is invalid".into()));
    };
    let child = tweak + parent_scalar;
    let bytes: [u8; 32] = child.to_bytes().into();
    if bytes == [0u8; 32] {
        return Err(HdError::Derivation("the child key is zero".into()));
    }
    Ok(Bip32Node {
        key: Zeroizing::new(bytes),
        chain_code,
    })
}

/// Reads a 32 byte big-endian value as a secp256k1 scalar.
///
/// Returns `None` when the value is not a valid scalar, which is exactly the
/// case BIP-32 calls an invalid key.
fn scalar_from_bytes(bytes: [u8; 32]) -> Option<Scalar> {
    let repr: FieldBytes = bytes.into();
    Scalar::from_repr(repr).into_option()
}

/// Derives the SLIP-0010 ed25519 master node from a seed.
fn slip10_master(seed: &[u8]) -> Result<(Zeroizing<[u8; 32]>, [u8; 32]), HdError> {
    let mut mac = <HmacSha512 as Mac>::new_from_slice(b"ed25519 seed")
        .map_err(|_| HdError::Derivation("hmac rejected the master key".into()))?;
    mac.update(seed);
    let out = mac.finalize().into_bytes();
    let mut key = [0u8; 32];
    key.copy_from_slice(&out[..32]);
    let mut chain_code = [0u8; 32];
    chain_code.copy_from_slice(&out[32..]);
    Ok((Zeroizing::new(key), chain_code))
}

/// Derives one SLIP-0010 ed25519 child.
///
/// ed25519 has no public key derivation, so only hardened children exist.
fn slip10_ckd(
    key: &[u8; 32],
    chain_code: &[u8; 32],
    index: u32,
) -> Result<(Zeroizing<[u8; 32]>, [u8; 32]), HdError> {
    let mut data = Vec::with_capacity(37);
    data.push(0x00);
    data.extend_from_slice(key);
    data.extend_from_slice(&index.to_be_bytes());

    let mut mac = <HmacSha512 as Mac>::new_from_slice(chain_code)
        .map_err(|_| HdError::Derivation("hmac rejected the chain code".into()))?;
    mac.update(&data);
    let out = mac.finalize().into_bytes();

    let mut child = [0u8; 32];
    child.copy_from_slice(&out[..32]);
    let mut next_chain_code = [0u8; 32];
    next_chain_code.copy_from_slice(&out[32..]);
    Ok((Zeroizing::new(child), next_chain_code))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The BIP-39 test mnemonic: twelve `abandon` words and `about`.
    const ABANDON: &str = "abandon abandon abandon abandon abandon abandon \
                           abandon abandon abandon abandon abandon about";

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn a_mnemonic_is_validated() {
        assert!(HdWallet::from_mnemonic(ABANDON, "").is_ok());
        // A single wrong word breaks the checksum.
        assert!(HdWallet::from_mnemonic(
            "abandon abandon abandon abandon abandon abandon abandon abandon \
             abandon abandon abandon abandon",
            ""
        )
        .is_err());
        // The wrong number of words is rejected too.
        assert!(HdWallet::from_mnemonic("abandon abandon abandon", "").is_err());
    }

    #[test]
    fn the_bip39_seed_matches_the_reference_vector() {
        let wallet = HdWallet::from_mnemonic(ABANDON, "TREZOR").unwrap();
        assert_eq!(
            hex(&wallet.seed[..]),
            "c55257c360c07c72029aebc1b53c05ed0362ada38ead3e3e9efa3708e5349553\
             1f09a6987599d18264c1e1c92f2cf141630c7a3c4ab7c81b2f001698e7463b04"
        );
    }

    #[test]
    fn the_bip32_derivation_matches_the_reference_vectors() {
        // BIP-32 test vector 1: seed 000102030405060708090a0b0c0d0e0f.
        let seed = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        // BIP-32 accepts a seed of any length for the master key, so the
        // reference vector uses 16 bytes rather than a BIP-39 seed.
        let node = bip32_master(&seed).unwrap();
        assert_eq!(
            hex(&node.key[..]),
            "e8f32e723decf4051aefac8e2c93c9c5b214313817cdb01a1494b917c8436b35"
        );
        assert_eq!(
            hex(&node.chain_code),
            "873dff81c02f525623fd1fe5167eac3a55a049de3d314bb42ee227ffed37d508"
        );

        let child = bip32_ckd(&node, HARDENED).unwrap();
        assert_eq!(
            hex(&child.key[..]),
            "edb2e14f9ee77d26dd93b4ecede8d16ed408ce149b6cd80b0715a2d911a0afea"
        );
        assert_eq!(
            hex(&child.chain_code),
            "47fdacbd0f1097043b78c63c20c34ef4ed9a111d980047ad16282c7ae6236141"
        );

        let grandchild = bip32_ckd(&child, 1).unwrap();
        assert_eq!(
            hex(&grandchild.key[..]),
            "3c6cb8d0f6a264c91ea8b5030fadaa8e538b020f0a387421a12de9319dc93368"
        );
        assert_eq!(
            hex(&grandchild.chain_code),
            "2a7857631386ba23dacac34180dd1983734e444fdbf774041578e9b6adb37c19"
        );
    }

    #[test]
    fn the_evm_path_derives_the_expected_private_key() {
        let wallet = HdWallet::from_mnemonic(ABANDON, "").unwrap();
        let key = wallet.derive_evm(&DerivationPath::evm(0)).unwrap();
        assert_eq!(
            hex(&key[..]),
            "1ab42cc412b618bdea3a599e3c9bae199ebf030895b039e9db1e30dafb12b727"
        );
    }

    #[test]
    fn the_solana_path_derives_the_expected_private_key() {
        let wallet = HdWallet::from_mnemonic(ABANDON, "").unwrap();
        let key = wallet.derive_solana(&DerivationPath::solana(0)).unwrap();
        assert_eq!(
            hex(&key[..]),
            "37df573b3ac4ad5b522e064e25b63ea16bcbe79d449e81a0268d1047948bb445"
        );
    }

    #[test]
    fn a_soft_evm_path_is_accepted_and_a_soft_solana_path_is_not() {
        let wallet = HdWallet::from_mnemonic(ABANDON, "").unwrap();
        assert!(wallet.derive_evm(&DerivationPath::evm(3)).is_ok());
        // ed25519 has no soft derivation.
        let soft = DerivationPath(vec![44 | HARDENED, SOLANA_COIN_TYPE | HARDENED, 0]);
        assert!(wallet.derive_solana(&soft).is_err());
    }

    #[test]
    fn paths_round_trip_through_their_text_form() {
        for raw in ["m/44'/60'/0'/0/0", "m/44'/501'/0'/0'", "m/0/1"] {
            let path = DerivationPath::parse(raw).unwrap();
            assert_eq!(path.to_string(), raw);
        }
        // The `h` marker is accepted as well.
        assert_eq!(
            DerivationPath::parse("m/44h/60h/0h/0/0")
                .unwrap()
                .to_string(),
            "m/44'/60'/0'/0/0"
        );
        assert!(DerivationPath::parse("44'/60'").is_err());
        assert!(DerivationPath::parse("m/x").is_err());
        assert!(DerivationPath::parse("m/").is_err());
    }

    #[test]
    fn different_paths_give_different_keys() {
        let wallet = HdWallet::from_mnemonic(ABANDON, "").unwrap();
        let first = wallet.derive_evm(&DerivationPath::evm(0)).unwrap();
        let second = wallet.derive_evm(&DerivationPath::evm(1)).unwrap();
        assert_ne!(first[..], second[..]);
    }

    #[test]
    fn the_solana_derivation_is_deterministic() {
        let wallet = HdWallet::from_mnemonic(ABANDON, "").unwrap();
        let first = wallet.derive_solana(&DerivationPath::solana(0)).unwrap();
        let second = wallet.derive_solana(&DerivationPath::solana(0)).unwrap();
        assert_eq!(first[..], second[..]);
        let other = wallet.derive_solana(&DerivationPath::solana(1)).unwrap();
        assert_ne!(first[..], other[..]);
    }
}

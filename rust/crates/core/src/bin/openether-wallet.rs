//! `openether-wallet` — the operator facing keystore tool.
//!
//! The core never writes a keystore on its own: a key must be created by a
//! human who holds the passphrase. This binary is that tool. It
//!
//! * creates a single key keystore from a raw secret,
//! * creates an HD keystore from fresh entropy and prints the BIP-39
//!   mnemonic **once**,
//! * imports an HD keystore from an existing mnemonic,
//! * and shows the public metadata of a keystore without unlocking it.
//!
//! The passphrase is read from `OPENETHER_WALLET_PASSPHRASE` and never from
//! the command line, so it does not end up in the shell history or in the
//! process list.
//!
//! ```text
//! openether-wallet create-hd --path wallet.json --chain evm --accounts 5
//! openether-wallet show --path wallet.json
//! ```

use openether_core::onchain::parse_chain;
use openether_core::types::Chain;
use openether_core::wallet::{
    create_hd_keystore, create_keystore, generate_hd_keystore, read_keystore_metadata, KdfParams,
    WalletMetadata, MAX_ACCOUNTS,
};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "\
openether-wallet — manage the encrypted openEther keystore

usage:
  openether-wallet create     --path <file> [--chain evm|solana] --secret <hex>
  openether-wallet create-hd  --path <file> [--chain evm|solana] [--accounts <n>]
  openether-wallet import-hd  --path <file> [--chain evm|solana] [--accounts <n>] --mnemonic \"<words>\"
  openether-wallet show       --path <file>

The passphrase is read from OPENETHER_WALLET_PASSPHRASE.
The account count defaults to OPENETHER_WALLET_ACCOUNTS (5).
";

/// The environment variable the passphrase is read from.
const PASSPHRASE_ENV: &str = "OPENETHER_WALLET_PASSPHRASE";
/// The environment variable the account count defaults to.
const ACCOUNTS_ENV: &str = "OPENETHER_WALLET_ACCOUNTS";

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
    let path = PathBuf::from(require(&options, "path")?);

    match command.as_str() {
        "create" => create_single(&path, &options),
        "create-hd" => create_hd(&path, &options, None),
        "import-hd" => {
            let mnemonic = require(&options, "mnemonic")?;
            create_hd(&path, &options, Some(&mnemonic))
        }
        "show" => show(&path),
        other => {
            print!("{USAGE}");
            Err(format!("unknown command: {other}"))
        }
    }
}

/// Creates a keystore around one raw private key.
fn create_single(path: &Path, options: &Options) -> Result<(), String> {
    let chain = chain_of(options)?;
    let secret = require(options, "secret")?;
    let secret = hex::decode(secret.trim_start_matches("0x"))
        .map_err(|e| format!("the secret is not valid hex: {e}"))?;
    let expected = match chain {
        Chain::Evm => 32,
        Chain::Solana => 32,
    };
    if secret.len() != expected {
        return Err(format!(
            "the secret must be {expected} bytes, got {}",
            secret.len()
        ));
    }
    let passphrase = passphrase()?;
    let metadata = create_keystore(path, chain, &secret, &passphrase, KdfParams::default())
        .map_err(|e| e.to_string())?;
    print_metadata(&metadata, path);
    Ok(())
}

/// Creates an HD keystore. Without a mnemonic, one is generated and printed
/// exactly once.
fn create_hd(path: &Path, options: &Options, mnemonic: Option<&str>) -> Result<(), String> {
    let chain = chain_of(options)?;
    let accounts = accounts_of(options)?;
    let passphrase = passphrase()?;

    let (words, metadata) = match mnemonic {
        Some(words) => {
            let metadata = create_hd_keystore(
                path,
                words,
                &passphrase,
                chain,
                accounts,
                KdfParams::default(),
            )
            .map_err(|e| e.to_string())?;
            (None, metadata)
        }
        None => {
            let (words, metadata) =
                generate_hd_keystore(path, &passphrase, chain, accounts, KdfParams::default())
                    .map_err(|e| e.to_string())?;
            (Some(words), metadata)
        }
    };

    print_metadata(&metadata, path);
    if let Some(words) = words {
        println!();
        println!("Your recovery phrase (write it down, it is shown only once):");
        println!();
        println!("    {words}");
        println!();
        println!("Anyone with these words controls the wallet. openEther cannot");
        println!("recover them for you.");
    }
    Ok(())
}

/// Prints the public metadata of a keystore. Nothing secret is unlocked.
fn show(path: &Path) -> Result<(), String> {
    let metadata = read_keystore_metadata(path).map_err(|e| e.to_string())?;
    println!(
        "{}",
        serde_json::to_string_pretty(&metadata).map_err(|e| e.to_string())?
    );
    Ok(())
}

fn print_metadata(metadata: &WalletMetadata, path: &Path) {
    println!("keystore: {}", path.display());
    println!(
        "type:     {}",
        metadata.wallet_type.clone().unwrap_or_default()
    );
    println!("chain:    {}", metadata.chain.clone().unwrap_or_default());
    println!("address:  {}", metadata.address.clone().unwrap_or_default());
    if !metadata.accounts.is_empty() {
        println!("accounts:");
        for account in &metadata.accounts {
            println!(
                "  {:<7} #{}  {}  {}",
                account.chain, account.index, account.path, account.address
            );
        }
    }
}

fn chain_of(options: &Options) -> Result<Chain, String> {
    match options.get("chain") {
        Some(value) => parse_chain(&value).ok_or_else(|| format!("unknown chain: {value}")),
        None => Ok(Chain::Evm),
    }
}

fn accounts_of(options: &Options) -> Result<u32, String> {
    let raw = match options.get("accounts") {
        Some(value) => value.clone(),
        None => std::env::var(ACCOUNTS_ENV).unwrap_or_else(|_| "5".to_string()),
    };
    let accounts: u32 = raw
        .trim()
        .parse()
        .map_err(|_| format!("the account count is not a number: {raw}"))?;
    if accounts == 0 || accounts > MAX_ACCOUNTS {
        return Err(format!(
            "the account count must be between 1 and {MAX_ACCOUNTS}"
        ));
    }
    Ok(accounts)
}

fn passphrase() -> Result<String, String> {
    let passphrase =
        std::env::var(PASSPHRASE_ENV).map_err(|_| format!("{PASSPHRASE_ENV} is not set"))?;
    if passphrase.is_empty() {
        return Err(format!("{PASSPHRASE_ENV} is empty"));
    }
    Ok(passphrase)
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

fn require(options: &Options, key: &str) -> Result<String, String> {
    options
        .get(key)
        .ok_or_else(|| format!("--{key} is required"))
}

//! The live trading arming ceremony.
//!
//! Live trading is not a flag. Setting `OPENETHER_LIVE_TRADING=1` only says
//! *"I would like to trade live"* — it arms nothing on its own. Arming is a
//! separate, deliberate act that writes a record to disk, and that record is
//! what the core checks before it lets an order reach a real exchange.
//!
//! The record is:
//!
//! * **deliberate** — the operator confirms a phrase and states a notional cap;
//! * **bound** — it carries a digest of the effective risk configuration, so
//!   widening a limit after arming invalidates the arming;
//! * **temporary** — it expires, and the core refuses it afterwards;
//! * **tightening only** — the armed notional cap can only lower the risk
//!   layer's cap, never raise it;
//! * **offline** — arming is a CLI act, not an HTTP call, so an agent that can
//!   reach the API cannot arm live trading.
//!
//! The digest covers exactly the settings that decide how much money can be
//! lost: the position, gross exposure, daily loss and drawdown caps, the
//! order size caps, the open position count, the breaker cooldown and the
//! three allowlists. Everything else about the configuration is free to
//! change without re-arming.

use crate::risk::RiskConfig;
use crate::types::{now_ms, Venue};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The version of the arming record this build writes and understands.
pub const ARMING_VERSION: u16 = 1;
/// The environment variable that points at the arming record.
pub const ARMING_ENV: &str = "OPENETHER_LIVE_ARMING_FILE";
/// The environment variable that asks for live trading.
pub const LIVE_ENV: &str = "OPENETHER_LIVE_TRADING";
/// The phrase the operator has to confirm before anything is armed.
pub const CONFIRMATION_PHRASE: &str = "I UNDERSTAND THIS SENDS REAL ORDERS";
/// The longest an arming may stay valid.
pub const MAX_ARMING_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// The target that arms the on-chain wallet.
///
/// A transfer is not an order, so it cannot ride on a venue's arming record:
/// moving coins out of the wallet is armed separately, with its own record,
/// its own expiry and its own cap.
pub const ONCHAIN_TARGET: &str = "onchain";

/// The record that arms live trading.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveArming {
    /// The record format version.
    pub version: u16,
    /// The target the operator armed: a venue (`binance`), or `onchain` for
    /// the wallet.
    pub venue: String,
    /// When the record was written.
    pub armed_at_ms: i64,
    /// When the record stops being valid.
    pub expires_at_ms: i64,
    /// SHA-256 of the risk configuration the record was written for.
    pub config_digest: String,
    /// The per-order cap the operator chose. It may only tighten the cap of
    /// the risk layer, never raise it.
    pub max_order_notional_usd: f64,
    /// The confirmed phrase. It proves the operator read what they armed.
    pub confirmation: String,
    /// A free-form note from the operator, e.g. "desk account, small size".
    pub operator_note: String,
}

/// The state of the arming, as the core sees it.
#[derive(Debug, Clone, Serialize)]
pub struct LiveArmingState {
    /// `true` when live trading is enabled right now.
    pub live_enabled: bool,
    /// The record, when one could be read and parsed.
    pub record: Option<LiveArming>,
    /// A machine-readable refusal code. `None` when live trading is enabled.
    pub reason: Option<&'static str>,
    /// A human-readable explanation of the refusal.
    pub detail: Option<String>,
    /// The per-order cap that is actually in force.
    pub effective_max_order_notional_usd: f64,
}

impl LiveArmingState {
    /// The state of a build that never armed anything.
    pub fn not_armed(reason: &'static str, detail: impl Into<String>) -> Self {
        Self {
            live_enabled: false,
            record: None,
            reason: Some(reason),
            detail: Some(detail.into()),
            effective_max_order_notional_usd: 0.0,
        }
    }
}

/// Everything that can go wrong while arming.
#[derive(Debug, thiserror::Error)]
pub enum LiveArmError {
    #[error("the arming record could not be written: {0}")]
    Io(#[from] std::io::Error),
    #[error("the arming record is not valid json: {0}")]
    Json(String),
    #[error("the arming record is missing")]
    Missing,
    #[error("the venue {0} does not trade live")]
    UnsupportedVenue(String),
    #[error("the confirmation phrase did not match")]
    BadConfirmation,
    #[error("the armed notional cap must be positive")]
    BadNotional,
    #[error("the armed notional cap of ${0:.2} is above the risk layer's cap of ${1:.2}; arming may only tighten the limit")]
    LoosensLimit(f64, f64),
    #[error("the arming window must be between 1 minute and 30 days")]
    BadWindow,
}

/// The path of the arming record, when one is configured.
pub fn arming_path() -> Option<PathBuf> {
    std::env::var(ARMING_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
}

/// `true` when the environment asks for live trading at all.
///
/// This is necessary but never sufficient: the arming record decides.
pub fn live_requested() -> bool {
    matches!(
        std::env::var(LIVE_ENV)
            .ok()
            .map(|v| v.trim().to_ascii_lowercase())
            .as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// A SHA-256 digest of the settings that decide how much money can be lost.
///
/// The allowlists are sorted so that the digest does not depend on the hash
/// order.
///
/// The per-order cap is deliberately *not* part of the digest. The arming
/// tightens that cap itself, so a digest taken over it would never match the
/// record it was written with. The cap is bound by the arming instead: the
/// effective cap is the smaller of the risk layer's cap and the armed cap, so
/// it can only ever tighten.
pub fn config_digest(venue: &str, risk: &RiskConfig) -> String {
    let mut canonical = String::new();
    canonical.push_str("openether/live-arming/config/v2\n");
    canonical.push_str(&format!("venue={venue}\n"));
    canonical.push_str(&format!(
        "max_position_pct={}\n",
        risk.max_position_pct_of_equity
    ));
    canonical.push_str(&format!(
        "max_gross_exposure_pct={}\n",
        risk.max_gross_exposure_pct
    ));
    canonical.push_str(&format!("max_daily_loss_pct={}\n", risk.max_daily_loss_pct));
    canonical.push_str(&format!("max_drawdown_pct={}\n", risk.max_drawdown_pct));
    canonical.push_str(&format!("max_open_positions={}\n", risk.max_open_positions));
    canonical.push_str(&format!(
        "min_order_notional_usd={}\n",
        risk.min_order_notional_usd
    ));
    canonical.push_str(&format!(
        "breaker_cooldown_ms={}\n",
        risk.breaker_cooldown_ms
    ));
    canonical.push_str(&format!(
        "hard_halt_on_oversized_order={}\n",
        risk.hard_halt_on_oversized_order
    ));
    canonical.push_str(&format!("symbols={}\n", sorted(&risk.symbol_allowlist)));
    canonical.push_str(&format!(
        "venues={}\n",
        sorted(
            &risk
                .venue_allowlist
                .iter()
                .map(|v| v.as_str().to_string())
                .collect()
        )
    ));
    canonical.push_str(&format!("contracts={}\n", sorted(&risk.contract_allowlist)));

    let digest = Sha256::digest(canonical.as_bytes());
    hex::encode(digest)
}

fn sorted(values: &HashSet<String>) -> String {
    let mut items: Vec<&str> = values.iter().map(|s| s.as_str()).collect();
    items.sort_unstable();
    items.join(",")
}

/// Writes an arming record.
///
/// This is the ceremony: it refuses a cap that would loosen the risk layer,
/// refuses a venue that cannot trade live and refuses a confirmation phrase
/// that does not match.
pub fn arm(
    path: &Path,
    venue: Venue,
    risk: &RiskConfig,
    max_order_notional_usd: f64,
    window_ms: i64,
    confirmation: &str,
    operator_note: &str,
) -> Result<LiveArming, LiveArmError> {
    if !venue.trades_live() {
        return Err(LiveArmError::UnsupportedVenue(venue.as_str().to_string()));
    }
    write_record(
        path,
        venue.as_str(),
        risk,
        max_order_notional_usd,
        window_ms,
        confirmation,
        operator_note,
    )
}

/// Arms the on-chain wallet.
///
/// This is the same ceremony as [`arm`], with the same cap and the same
/// expiry, but it writes a record whose target is [`ONCHAIN_TARGET`]. A record
/// that arms a venue does not arm the wallet, because placing an order and
/// moving coins out of the wallet are different acts.
pub fn arm_onchain(
    path: &Path,
    risk: &RiskConfig,
    max_transfer_notional_usd: f64,
    window_ms: i64,
    confirmation: &str,
    operator_note: &str,
) -> Result<LiveArming, LiveArmError> {
    write_record(
        path,
        ONCHAIN_TARGET,
        risk,
        max_transfer_notional_usd,
        window_ms,
        confirmation,
        operator_note,
    )
}

fn write_record(
    path: &Path,
    target: &str,
    risk: &RiskConfig,
    max_order_notional_usd: f64,
    window_ms: i64,
    confirmation: &str,
    operator_note: &str,
) -> Result<LiveArming, LiveArmError> {
    if confirmation.trim() != CONFIRMATION_PHRASE {
        return Err(LiveArmError::BadConfirmation);
    }
    if !max_order_notional_usd.is_finite() || max_order_notional_usd <= 0.0 {
        return Err(LiveArmError::BadNotional);
    }
    if max_order_notional_usd > risk.max_order_notional_usd {
        return Err(LiveArmError::LoosensLimit(
            max_order_notional_usd,
            risk.max_order_notional_usd,
        ));
    }
    if window_ms <= 0 || window_ms > MAX_ARMING_MS {
        return Err(LiveArmError::BadWindow);
    }

    let now = now_ms();
    let record = LiveArming {
        version: ARMING_VERSION,
        venue: target.to_string(),
        armed_at_ms: now,
        expires_at_ms: now + window_ms,
        config_digest: config_digest(target, risk),
        max_order_notional_usd,
        confirmation: confirmation.trim().to_string(),
        operator_note: operator_note.trim().to_string(),
    };

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let bytes =
        serde_json::to_vec_pretty(&record).map_err(|e| LiveArmError::Json(e.to_string()))?;
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
    Ok(record)
}

/// Reads an arming record.
pub fn read(path: &Path) -> Result<LiveArming, LiveArmError> {
    let raw = std::fs::read(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            LiveArmError::Missing
        } else {
            LiveArmError::Io(e)
        }
    })?;
    serde_json::from_slice(&raw).map_err(|e| LiveArmError::Json(e.to_string()))
}

/// Removes an arming record.
pub fn disarm(path: &Path) -> Result<(), LiveArmError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(LiveArmError::Io(e)),
    }
}

/// Decides whether live trading may run, and why not when it may not.
///
/// This is the only place the decision is made: the core, the connectors and
/// the API all read the outcome of this function.
pub fn evaluate(venue: Venue, risk: &RiskConfig) -> LiveArmingState {
    evaluate_at(arming_path().as_deref(), venue, risk, live_requested())
}

/// The decision for a venue, with every input spelled out.
///
/// Splitting this out keeps the rule testable without touching the
/// environment, which would otherwise make the tests race each other.
pub fn evaluate_at(
    path: Option<&Path>,
    venue: Venue,
    risk: &RiskConfig,
    requested: bool,
) -> LiveArmingState {
    evaluate_target_at(path, venue.as_str(), risk, requested)
}

/// The decision for the on-chain wallet.
pub fn evaluate_onchain(risk: &RiskConfig) -> LiveArmingState {
    evaluate_onchain_at(arming_path().as_deref(), risk, live_requested())
}

/// The on-chain decision, with the path and the request supplied.
pub fn evaluate_onchain_at(
    path: Option<&Path>,
    risk: &RiskConfig,
    requested: bool,
) -> LiveArmingState {
    evaluate_target_at(path, ONCHAIN_TARGET, risk, requested)
}

/// The decision for whatever a record names: a venue, or `onchain`.
fn evaluate_target_at(
    path: Option<&Path>,
    target: &str,
    risk: &RiskConfig,
    requested: bool,
) -> LiveArmingState {
    let effective_cap = risk.max_order_notional_usd;
    let noun = if target == ONCHAIN_TARGET {
        "transfers"
    } else {
        "trading"
    };
    let Some(path) = path else {
        let mut state = LiveArmingState::not_armed(
            "arming_file_unset",
            format!(
                "live {noun} is off: {ARMING_ENV} is not set. Run `openether-live arm` to arm it."
            ),
        );
        state.effective_max_order_notional_usd = effective_cap;
        return state;
    };

    let record = match read(path) {
        Ok(record) => record,
        Err(LiveArmError::Missing) => {
            let mut state = LiveArmingState::not_armed(
                "arming_file_missing",
                format!(
                    "live {noun} is off: the arming record {} does not exist. Run `openether-live arm` to arm it.",
                    path.display()
                ),
            );
            state.effective_max_order_notional_usd = effective_cap;
            return state;
        }
        Err(error) => {
            let mut state = LiveArmingState::not_armed(
                "arming_file_unreadable",
                format!("live {noun} is off: the arming record cannot be read: {error}"),
            );
            state.effective_max_order_notional_usd = effective_cap;
            return state;
        }
    };

    if record.version != ARMING_VERSION {
        let mut state = LiveArmingState::not_armed(
            "arming_version_unsupported",
            format!(
                "live {noun} is off: the arming record is version {}, this build understands {ARMING_VERSION}",
                record.version
            ),
        );
        state.record = Some(record);
        state.effective_max_order_notional_usd = effective_cap;
        return state;
    }

    if record.venue != target {
        let mut state = LiveArmingState::not_armed(
            "arming_target_mismatch",
            format!(
                "live {noun} is off: the arming record is for {}, not {target}",
                record.venue
            ),
        );
        state.record = Some(record);
        state.effective_max_order_notional_usd = effective_cap;
        return state;
    }

    if record.confirmation != CONFIRMATION_PHRASE {
        let mut state = LiveArmingState::not_armed(
            "arming_confirmation_invalid",
            format!("live {noun} is off: the arming record was not confirmed by the operator"),
        );
        state.record = Some(record);
        state.effective_max_order_notional_usd = effective_cap;
        return state;
    }

    let now = now_ms();
    if now >= record.expires_at_ms {
        let mut state = LiveArmingState::not_armed(
            "arming_expired",
            format!(
                "live {noun} is off: the arming record expired at {}. Run `openether-live arm` again.",
                record.expires_at_ms
            ),
        );
        state.record = Some(record);
        state.effective_max_order_notional_usd = effective_cap;
        return state;
    }

    let digest = config_digest(target, risk);
    if digest != record.config_digest {
        let mut state = LiveArmingState::not_armed(
            "arming_config_changed",
            format!("live {noun} is off: the risk configuration changed after the arming was written. Arm it again."),
        );
        state.record = Some(record);
        state.effective_max_order_notional_usd = effective_cap;
        return state;
    }

    if !requested {
        let mut state = LiveArmingState::not_armed(
            "live_not_requested",
            format!(
                "live {noun} is off: {LIVE_ENV} is not set. The arming record is valid, but live {noun} was not requested."
            ),
        );
        state.record = Some(record);
        state.effective_max_order_notional_usd = effective_cap;
        return state;
    }

    // The armed cap may only tighten the risk layer's cap.
    let effective_cap = effective_cap.min(record.max_order_notional_usd);
    LiveArmingState {
        live_enabled: true,
        record: Some(record),
        reason: None,
        detail: None,
        effective_max_order_notional_usd: effective_cap,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Venue;

    fn risk() -> RiskConfig {
        RiskConfig::default()
    }

    fn temp_path(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "openether-live-arm-test-{tag}-{}-{}",
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("arming.json")
    }

    #[test]
    fn arming_requires_the_confirmation_phrase() {
        let path = temp_path("confirm");
        let error = arm(&path, Venue::Binance, &risk(), 100.0, 60_000, "yes", "").unwrap_err();
        assert!(matches!(error, LiveArmError::BadConfirmation));
        assert!(!path.exists());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn arming_may_only_tighten_the_notional_cap() {
        let path = temp_path("tighten");
        let error = arm(
            &path,
            Venue::Binance,
            &risk(),
            9_999_999.0,
            60_000,
            CONFIRMATION_PHRASE,
            "",
        )
        .unwrap_err();
        assert!(matches!(error, LiveArmError::LoosensLimit(_, _)));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn arming_refuses_a_venue_that_cannot_trade_live() {
        let path = temp_path("venue");
        let error = arm(
            &path,
            Venue::Paper,
            &risk(),
            100.0,
            60_000,
            CONFIRMATION_PHRASE,
            "",
        )
        .unwrap_err();
        assert!(matches!(error, LiveArmError::UnsupportedVenue(_)));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn the_digest_changes_when_a_limit_changes() {
        let base = risk();
        let mut changed = base.clone();
        changed.max_position_pct_of_equity = 7.0;
        assert_ne!(
            config_digest("binance", &base),
            config_digest("binance", &changed)
        );
        // The venue is part of the digest too.
        assert_ne!(
            config_digest("binance", &base),
            config_digest("kraken", &base)
        );
    }

    #[test]
    fn the_digest_survives_the_arming_tightening_the_order_cap() {
        // The armed cap is applied to the risk config, so a digest taken over
        // the cap would never match the record it was written with. The cap is
        // bound by the `min` in `evaluate_at` instead.
        let base = risk();
        let mut tightened = base.clone();
        tightened.max_order_notional_usd = 100.0;
        assert_eq!(
            config_digest("binance", &base),
            config_digest("binance", &tightened)
        );
    }

    #[test]
    fn the_digest_does_not_depend_on_the_allowlist_order() {
        let mut a = risk();
        a.symbol_allowlist = HashSet::from(["A".to_string(), "B".to_string()]);
        let mut b = risk();
        b.symbol_allowlist = HashSet::from(["B".to_string(), "A".to_string()]);
        assert_eq!(config_digest("binance", &a), config_digest("binance", &b));
    }

    #[test]
    fn live_trading_is_off_without_an_arming_record() {
        let state = evaluate_at(None, Venue::Binance, &risk(), true);
        assert!(!state.live_enabled);
        assert_eq!(state.reason, Some("arming_file_unset"));
    }

    #[test]
    fn an_arming_record_alone_does_not_enable_live_trading() {
        let path = temp_path("not-requested");
        arm(
            &path,
            Venue::Binance,
            &risk(),
            100.0,
            60_000,
            CONFIRMATION_PHRASE,
            "",
        )
        .unwrap();

        // The record is valid, but live trading was not requested.
        let state = evaluate_at(Some(&path), Venue::Binance, &risk(), false);
        assert!(!state.live_enabled);
        assert_eq!(state.reason, Some("live_not_requested"));

        // Requested, the same record enables live trading.
        let state = evaluate_at(Some(&path), Venue::Binance, &risk(), true);
        assert!(state.live_enabled);
        assert!(state.reason.is_none());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn changing_a_limit_invalidates_the_arming() {
        let path = temp_path("changed");
        let base = risk();
        arm(
            &path,
            Venue::Binance,
            &base,
            100.0,
            60_000,
            CONFIRMATION_PHRASE,
            "",
        )
        .unwrap();

        let mut widened = base.clone();
        widened.max_position_pct_of_equity = 50.0;
        let state = evaluate_at(Some(&path), Venue::Binance, &widened, true);
        assert!(!state.live_enabled);
        assert_eq!(state.reason, Some("arming_config_changed"));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn the_arming_survives_its_own_tightening_of_the_order_cap() {
        // This is the flow `Config::from_env` runs: evaluate, write the
        // tightened cap back into the risk config, then look at the state
        // again. The second look must not decide the arming is stale.
        let path = temp_path("tighten-flow");
        let base = risk();
        arm(
            &path,
            Venue::Binance,
            &base,
            100.0,
            60_000,
            CONFIRMATION_PHRASE,
            "",
        )
        .unwrap();

        let first = evaluate_at(Some(&path), Venue::Binance, &base, true);
        assert!(first.live_enabled);
        assert_eq!(first.effective_max_order_notional_usd, 100.0);

        let mut tightened = base.clone();
        tightened.max_order_notional_usd = first.effective_max_order_notional_usd;
        let second = evaluate_at(Some(&path), Venue::Binance, &tightened, true);
        assert!(second.live_enabled);
        assert_eq!(second.effective_max_order_notional_usd, 100.0);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn the_armed_cap_never_exceeds_the_risk_layer_cap() {
        // If the operator lowers the risk layer's cap after arming, the
        // effective cap follows it down.
        let path = temp_path("lowered");
        let base = risk();
        arm(
            &path,
            Venue::Binance,
            &base,
            100.0,
            60_000,
            CONFIRMATION_PHRASE,
            "",
        )
        .unwrap();

        let mut lowered = base.clone();
        lowered.max_order_notional_usd = 40.0;
        let state = evaluate_at(Some(&path), Venue::Binance, &lowered, true);
        assert!(state.live_enabled);
        assert_eq!(state.effective_max_order_notional_usd, 40.0);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn an_arming_for_another_venue_is_refused() {
        let path = temp_path("venue");
        arm(
            &path,
            Venue::Binance,
            &risk(),
            100.0,
            60_000,
            CONFIRMATION_PHRASE,
            "",
        )
        .unwrap();
        let state = evaluate_at(Some(&path), Venue::Kraken, &risk(), true);
        assert!(!state.live_enabled);
        assert_eq!(state.reason, Some("arming_target_mismatch"));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn an_onchain_arming_is_separate_from_a_venue_arming() {
        let path = temp_path("onchain-separate");
        arm_onchain(&path, &risk(), 100.0, 60_000, CONFIRMATION_PHRASE, "wallet").unwrap();

        // The record arms the wallet, not Binance.
        let venue = evaluate_at(Some(&path), Venue::Binance, &risk(), true);
        assert!(!venue.live_enabled);
        assert_eq!(venue.reason, Some("arming_target_mismatch"));

        let wallet = evaluate_onchain_at(Some(&path), &risk(), true);
        assert!(wallet.live_enabled);
        assert_eq!(wallet.effective_max_order_notional_usd, 100.0);

        // A venue record does not arm the wallet either.
        let venue_path = temp_path("onchain-venue");
        arm(
            &venue_path,
            Venue::Binance,
            &risk(),
            100.0,
            60_000,
            CONFIRMATION_PHRASE,
            "",
        )
        .unwrap();
        let wallet = evaluate_onchain_at(Some(&venue_path), &risk(), true);
        assert!(!wallet.live_enabled);
        assert_eq!(wallet.reason, Some("arming_target_mismatch"));

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
        std::fs::remove_dir_all(venue_path.parent().unwrap()).ok();
    }

    #[test]
    fn an_onchain_arming_may_only_tighten_the_notional_cap() {
        let path = temp_path("onchain-tighten");
        let error =
            arm_onchain(&path, &risk(), 9_999_999.0, 60_000, CONFIRMATION_PHRASE, "").unwrap_err();
        assert!(matches!(error, LiveArmError::LoosensLimit(_, _)));
        assert!(!path.exists());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn an_expired_arming_is_refused() {
        let path = temp_path("expired");
        let mut record = arm(
            &path,
            Venue::Binance,
            &risk(),
            100.0,
            60_000,
            CONFIRMATION_PHRASE,
            "",
        )
        .unwrap();
        // Move the expiry into the past.
        record.expires_at_ms = now_ms() - 1;
        std::fs::write(&path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();

        let state = evaluate_at(Some(&path), Venue::Binance, &risk(), true);
        assert!(!state.live_enabled);
        assert_eq!(state.reason, Some("arming_expired"));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn the_armed_cap_tightens_the_risk_cap() {
        let path = temp_path("cap");
        let base = risk();
        arm(
            &path,
            Venue::Binance,
            &base,
            250.0,
            60_000,
            CONFIRMATION_PHRASE,
            "",
        )
        .unwrap();

        let state = evaluate_at(Some(&path), Venue::Binance, &base, true);
        assert!(state.live_enabled);
        assert_eq!(state.effective_max_order_notional_usd, 250.0);
        assert!(state.effective_max_order_notional_usd < base.max_order_notional_usd);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}

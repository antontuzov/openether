use crate::engine::AppState;
use crate::holdings::WalletHoldings;
use crate::types::*;
use crate::wallet::{AccountSelector, TxEnvelope, WalletError, WalletService};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use zeroize::Zeroizing;

pub fn router(state: Arc<AppState>) -> Router {
    // The wallet service lives on the engine, so the unlock lease is process
    // wide and the holdings loop and the API share one keystore.
    let wallet = Arc::clone(&state.wallet);

    Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .route("/api/config", get(config))
        .route("/api/portfolio", get(portfolio))
        .route("/api/positions", get(positions))
        .route("/api/trades", get(trades))
        .route("/api/logs", get(logs))
        .route("/api/logs", post(push_log))
        .route("/api/ticks", get(ticks))
        .route("/api/risk", get(risk))
        .route("/api/risk/decisions", get(risk_decisions))
        .route("/api/risk/rules", get(risk_rules))
        .route("/api/onchain", post(onchain))
        .route("/api/strategy", get(strategy))
        .route("/api/strategy/reload", post(strategy_reload))
        .route("/api/orders", post(place_order))
        .route("/api/orders", get(orders))
        .route("/api/orders/{order_id}", delete(cancel_order))
        .route("/api/breaker/reset", post(reset_breaker))
        .route("/api/venues", get(venues))
        .route("/api/wallet", get(wallet_metadata))
        .route("/api/wallet/holdings", get(wallet_holdings))
        .route("/api/wallet/sign-dry-run", post(wallet_sign_dry_run))
        .route("/api/live", get(live_status))
        .route("/ws", get(ws_handler))
        .layer(Extension(wallet))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn healthz(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "live_trading": state.config.live_trading,
        "venue": state.venue().as_str(),
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn metrics(State(state): State<Arc<AppState>>) -> Response {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.metrics.encode(),
    )
        .into_response()
}

async fn config(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let cfg = &state.config;
    Json(serde_json::json!({
        "venue": cfg.venue.as_str(),
        "live_trading": cfg.live_trading,
        "live_arming": live_arming_json(&cfg.live_arming),
        "symbols": cfg.symbols,
        "risk": {
            "max_position_pct_of_equity": cfg.risk.max_position_pct_of_equity,
            "max_gross_exposure_pct": cfg.risk.max_gross_exposure_pct,
            "max_daily_loss_pct": cfg.risk.max_daily_loss_pct,
            "max_drawdown_pct": cfg.risk.max_drawdown_pct,
            "max_open_positions": cfg.risk.max_open_positions,
            "max_order_notional_usd": cfg.risk.max_order_notional_usd,
            "min_order_notional_usd": cfg.risk.min_order_notional_usd,
            "symbol_allowlist": cfg.risk.symbol_allowlist.iter().collect::<Vec<_>>(),
            "venue_allowlist": cfg.risk.venue_allowlist.iter().map(|v| v.as_str()).collect::<Vec<_>>(),
            "contract_allowlist": cfg.risk.contract_allowlist.iter().collect::<Vec<_>>(),
        },
        "strategy_wasm": cfg.strategy_wasm.as_ref().map(|p| p.display().to_string()),
        "chains": state.onchain.chains(),
    }))
}

/// `GET /api/live`
///
/// Whether live trading is armed, and why not when it is not. The response is
/// read-only on purpose: arming is a CLI act, so an agent that can reach this
/// API can never arm live trading.
async fn live_status(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let cfg = &state.config;
    Json(serde_json::json!({
        "live_trading": cfg.live_trading,
        "venue": cfg.venue.as_str(),
        "arming": live_arming_json(&cfg.live_arming),
    }))
}

/// The arming state as plain JSON.
///
/// The confirmation phrase is not repeated here: it is not a secret, but there
/// is no reason to echo it into a log.
fn live_arming_json(state: &crate::live_arm::LiveArmingState) -> serde_json::Value {
    let record = state.record.as_ref();
    serde_json::json!({
        "live_enabled": state.live_enabled,
        "reason": state.reason,
        "detail": state.detail,
        "effective_max_order_notional_usd": state.effective_max_order_notional_usd,
        "venue": record.map(|r| r.venue.clone()),
        "armed_at_ms": record.map(|r| r.armed_at_ms),
        "expires_at_ms": record.map(|r| r.expires_at_ms),
        "max_order_notional_usd": record.map(|r| r.max_order_notional_usd),
        "operator_note": record.map(|r| r.operator_note.clone()),
    })
}

async fn portfolio(State(state): State<Arc<AppState>>) -> Json<PortfolioSnapshot> {
    Json(state.portfolio())
}

async fn positions(State(state): State<Arc<AppState>>) -> Json<Vec<Position>> {
    Json(state.portfolio().positions)
}

/// Every order the engine submitted, newest first, with the fills the venue
/// has actually confirmed. A resting order carries an empty `fills` list — it
/// is not a position until the venue says it filled.
async fn orders(State(state): State<Arc<AppState>>) -> Json<Vec<TrackedOrder>> {
    Json(state.orders())
}

/// Cancel a resting order. The venue must confirm before the engine marks it
/// cancelled, so a failed cancel leaves the order live and reconciling.
async fn cancel_order(
    State(state): State<Arc<AppState>>,
    Path(order_id): Path<String>,
) -> Response {
    match state.cancel_order(&order_id).await {
        Ok(()) => (StatusCode::OK, Json(json!({"ok": true}))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

/// The canonical risk rules this engine is running with.
///
/// The browser simulator reads the same contract, so the dashboard and the
/// engine refuse the same orders for the same reasons.
async fn risk_rules(State(state): State<Arc<AppState>>) -> Json<crate::risk_rules::RiskContract> {
    Json(crate::risk_rules::contract(&state.config.risk))
}

#[derive(Debug, Deserialize)]
struct LimitQuery {
    limit: Option<usize>,
}

async fn trades(
    State(state): State<Arc<AppState>>,
    Query(query): Query<LimitQuery>,
) -> Json<Vec<TradeRecord>> {
    Json(state.trades(query.limit.unwrap_or(200)))
}

async fn logs(
    State(state): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
) -> Json<Vec<LogEntry>> {
    let limit = query
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(200);
    let source = query.get("source").cloned();
    let mut entries = state.logs(limit);
    if let Some(source) = source {
        entries.retain(|entry| entry.source == source);
    }
    Json(entries)
}

#[derive(Debug, Deserialize)]
struct IncomingLog {
    source: Option<String>,
    level: Option<String>,
    message: String,
    #[serde(default)]
    context: serde_json::Value,
}

async fn push_log(
    State(state): State<Arc<AppState>>,
    Json(entry): Json<IncomingLog>,
) -> Json<serde_json::Value> {
    state.log(
        &entry.source.unwrap_or_else(|| "agent".into()),
        &entry.level.unwrap_or_else(|| "info".into()),
        &entry.message,
        entry.context,
    );
    Json(serde_json::json!({"ok": true}))
}

async fn ticks(
    State(state): State<Arc<AppState>>,
    Query(query): Query<LimitQuery>,
) -> Json<Vec<MarketTick>> {
    Json(state.ticks(query.limit.unwrap_or(200)))
}

async fn risk(State(state): State<Arc<AppState>>) -> Json<crate::risk::RiskState> {
    Json(state.risk_state())
}

async fn risk_decisions(
    State(state): State<Arc<AppState>>,
    Query(query): Query<LimitQuery>,
) -> Json<Vec<crate::risk::RiskDecision>> {
    Json(state.risk.decisions(query.limit.unwrap_or(100)))
}

async fn strategy(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let guard = state.strategy.lock().unwrap_or_else(|e| e.into_inner());
    let value = match guard.as_ref() {
        Some(host) => serde_json::to_value(host.info()).unwrap_or_else(|_| serde_json::json!({})),
        None => serde_json::json!({
            "name": "none",
            "path": "",
            "loaded_at_ms": 0,
            "wasm_sha256": "",
            "ticks_processed": 0,
            "fuel_used": 0,
            "last_error": "no strategy loaded",
        }),
    };
    Json(value)
}

#[derive(Debug, Deserialize)]
struct OnchainRequest {
    chain: String,
    address: String,
    #[serde(default)]
    token: Option<String>,
}

async fn onchain(
    State(state): State<Arc<AppState>>,
    Json(request): Json<OnchainRequest>,
) -> Response {
    let result = match crate::onchain::parse_chain(&request.chain) {
        Some(Chain::Solana) => state
            .onchain
            .solana_account(&request.address)
            .await
            .map(|account| json!({"ok": true, "account": account}))
            .map_err(|e| e.to_string()),
        Some(Chain::Evm) => {
            let chain = if request.chain.eq_ignore_ascii_case("base") {
                "base"
            } else {
                "ethereum"
            };
            match &request.token {
                Some(token) => state
                    .onchain
                    .evm_token_balance(chain, token, &request.address)
                    .await
                    .map(|balance| json!({"ok": true, "token": balance}))
                    .map_err(|e| e.to_string()),
                None => state
                    .onchain
                    .evm_balance(chain, &request.address)
                    .await
                    .map(|account| json!({"ok": true, "account": account}))
                    .map_err(|e| e.to_string()),
            }
        }
        None => Err(format!("unsupported chain: {}", request.chain)),
    };

    match result {
        Ok(value) => (StatusCode::OK, Json(value)).into_response(),
        Err(message) => (StatusCode::BAD_REQUEST, message).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct ReloadRequest {
    path: String,
}

async fn strategy_reload(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ReloadRequest>,
) -> Response {
    let guard = state.strategy.lock().unwrap_or_else(|e| e.into_inner());
    let Some(host) = guard.as_ref() else {
        return (StatusCode::CONFLICT, "no strategy is loaded").into_response();
    };
    match host.reload(&request.path) {
        Ok(info) => {
            state.log(
                "strategy",
                "info",
                &format!("strategy reloaded from {}", request.path),
                serde_json::json!({"path": request.path}),
            );
            (StatusCode::OK, Json(info)).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

async fn place_order(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    // The engine trades on the venue it was configured with and overrides
    // whatever the caller names, so an omitted venue means "the engine's".
    // Requiring it would refuse every agent order: the MCP tool only sends a
    // venue when the caller explicitly overrides it.
    let mut body = body;
    if let Some(object) = body.as_object_mut() {
        object
            .entry("venue")
            .or_insert_with(|| serde_json::json!(state.venue()));
    }
    let request: OrderRequest = match serde_json::from_value(body) {
        Ok(request) => request,
        Err(e) => return (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()).into_response(),
    };
    let outcome = state.submit_order(request, "api").await;
    let status = if outcome.approved {
        StatusCode::OK
    } else {
        StatusCode::FORBIDDEN
    };
    (status, Json(outcome)).into_response()
}

#[derive(Debug, Deserialize)]
struct ResetRequest {
    #[serde(default)]
    confirm: bool,
}

async fn reset_breaker(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ResetRequest>,
) -> Response {
    match state.risk.reset_breaker(request.confirm) {
        Ok(()) => {
            state.log(
                "risk",
                "warn",
                "circuit breaker was reset manually",
                serde_json::json!({"confirmed": true}),
            );
            (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

async fn venues(State(state): State<Arc<AppState>>) -> Json<Vec<serde_json::Value>> {
    let mut out = Vec::new();
    for venue in Venue::ALL {
        let healthy = if venue == state.venue() {
            state.connector.health().await.is_ok()
        } else {
            false
        };
        out.push(serde_json::json!({
            "venue": venue.as_str(),
            "healthy": healthy,
            "live": state.config.live_trading && venue != Venue::Paper,
            "active": venue == state.venue(),
        }));
    }
    Json(out)
}

/// `GET /api/wallet`
///
/// Public wallet metadata only: whether a keystore is configured, which chain
/// it belongs to and the derived address. The response never contains a key,
/// a passphrase, a ciphertext or any other secret.
async fn wallet_metadata(
    Extension(wallet): Extension<Arc<WalletService>>,
) -> Json<serde_json::Value> {
    let metadata = wallet.metadata();
    Json(
        serde_json::to_value(metadata).unwrap_or_else(|_| serde_json::json!({"configured": false})),
    )
}

/// `GET /api/wallet/holdings`
///
/// What the wallet holds, valued in dollars: the native coin of every chain
/// plus the ERC-20 and SPL tokens. Balances come from public RPC endpoints and
/// prices from a free public feed, so no key is unlocked to answer this.
async fn wallet_holdings(State(state): State<Arc<AppState>>) -> Json<WalletHoldings> {
    Json(state.wallet_holdings())
}

/// `POST /api/wallet/sign-dry-run`
///
/// Signs an envelope locally and returns the signature. The transaction is
/// **never** broadcast: this build has no broadcast code path at all, and the
/// answer always carries `broadcast: false` / `dry_run: true`.
///
/// The unlock passphrase arrives in the request body, is used once and is
/// wiped from memory before the response is sent. The `Debug` implementation
/// is deliberately absent so that the passphrase can never be logged.
#[derive(Deserialize)]
struct WalletSignRequest {
    /// Unlock passphrase for the encrypted keystore.
    passphrase: String,
    /// The transaction envelope to sign.
    envelope: TxEnvelope,
    /// Which account signs. Omitted means the primary account.
    #[serde(default)]
    account: AccountSelector,
}

async fn wallet_sign_dry_run(
    Extension(wallet): Extension<Arc<WalletService>>,
    Json(request): Json<WalletSignRequest>,
) -> Response {
    let passphrase = Zeroizing::new(request.passphrase);
    match wallet.sign_dry_run_with_passphrase(&passphrase, &request.envelope, &request.account) {
        Ok(result) => (StatusCode::OK, Json(result)).into_response(),
        Err(error) => {
            let status = wallet_error_status(&error);
            (
                status,
                Json(serde_json::json!({
                    "ok": false,
                    "error": error.to_string(),
                    "code": wallet_error_code(&error),
                    "broadcast": false,
                    "dry_run": true,
                })),
            )
                .into_response()
        }
    }
}

fn wallet_error_status(error: &WalletError) -> StatusCode {
    match error {
        WalletError::InvalidPassphrase => StatusCode::UNAUTHORIZED,
        WalletError::NotUnlocked | WalletError::LeaseExpired => StatusCode::CONFLICT,
        WalletError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        WalletError::DecryptFailed | WalletError::SignatureInvalid => StatusCode::BAD_REQUEST,
        WalletError::NotConfigured
        | WalletError::LiveBroadcastUnsupported
        | WalletError::ChainMismatch { .. }
        | WalletError::AccountNotFound(_)
        | WalletError::InvalidKey
        | WalletError::InvalidEnvelope(_)
        | WalletError::UnsupportedFormat(_) => StatusCode::BAD_REQUEST,
    }
}

fn wallet_error_code(error: &WalletError) -> &'static str {
    match error {
        WalletError::NotConfigured => "wallet_not_configured",
        WalletError::NotUnlocked => "wallet_locked",
        WalletError::LeaseExpired => "wallet_lease_expired",
        WalletError::InvalidPassphrase => "invalid_passphrase",
        WalletError::DecryptFailed => "keystore_decrypt_failed",
        WalletError::LiveBroadcastUnsupported => "live_broadcast_unsupported",
        WalletError::ChainMismatch { .. } => "chain_mismatch",
        WalletError::AccountNotFound(_) => "account_not_found",
        WalletError::InvalidKey => "invalid_key",
        WalletError::InvalidEnvelope(_) => "invalid_envelope",
        WalletError::SignatureInvalid => "signature_invalid",
        WalletError::UnsupportedFormat(_) => "unsupported_keystore_format",
        WalletError::Io(_) => "keystore_io_error",
    }
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(move |socket| ws_connection(socket, state))
}

async fn ws_connection(mut socket: WebSocket, state: Arc<AppState>) {
    let mut events = state.bus.subscribe();
    let clients = state
        .ws_clients
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        + 1;
    state
        .metrics
        .gauge_set("openether_ws_connections", &[], clients as f64);

    let portfolio = state.portfolio();
    let risk = state.risk_state();
    let initial = serde_json::json!([
        {"type": "portfolio", "data": portfolio},
        {"type": "risk", "data": risk},
    ]);
    if socket
        .send(Message::Text(initial.to_string().into()))
        .await
        .is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            event = events.recv() => {
                match event {
                    Ok(event) => {
                        let frame = serde_json::to_string(&event).unwrap_or_default();
                        if socket.send(Message::Text(frame.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "websocket client fell behind");
                    }
                    Err(_) => break,
                }
            }
            message = socket.recv() => {
                match message {
                    Some(Ok(Message::Text(text))) => {
                        if text.contains("ping")
                            && socket
                                .send(Message::Text("{\"type\":\"pong\"}".into()))
                                .await
                                .is_err()
                        {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    Some(Ok(_)) => {}
                }
            }
        }
    }
    let remaining = state
        .ws_clients
        .fetch_sub(1, std::sync::atomic::Ordering::SeqCst)
        - 1;
    state
        .metrics
        .gauge_set("openether_ws_connections", &[], remaining as f64);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, WalletConfig};
    use crate::types::Chain;
    use axum::body::Body;
    use axum::http::Request;
    use rand::rngs::OsRng;
    use rand::TryRngCore;
    use std::path::PathBuf;
    use tower::ServiceExt;

    fn unconfigured_wallet() -> Arc<WalletService> {
        Arc::new(WalletService::new(WalletConfig {
            keystore_path: None,
            chain: None,
            dry_run: true,
            unlock_timeout_ms: 1_000,
            accounts: 5,
            ..Default::default()
        }))
    }

    fn envelope() -> TxEnvelope {
        TxEnvelope {
            chain: Chain::Evm,
            nonce: 1,
            timestamp_ms: 1_700_000_000_000,
            to: "0x000000000000000000000000000000000000dEaD".to_string(),
            amount: 1.0,
            amount_raw: 1_000_000,
            fee: 21_000,
            memo: String::new(),
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "openether-api-test-{tag}-{}-{}",
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[tokio::test]
    async fn wallet_metadata_response_has_no_secret_fields() {
        let response = wallet_metadata(Extension(unconfigured_wallet())).await;
        let value = serde_json::to_value(&response.0).unwrap();
        assert_eq!(value["configured"], serde_json::json!(false));
        assert_eq!(value["encrypted"], serde_json::json!(true));
        assert_eq!(value["dry_run"], serde_json::json!(true));
        assert!(value["chain"].is_null());
        assert!(value["address"].is_null());

        let object = value.as_object().unwrap();
        for key in object.keys() {
            let key = key.to_lowercase();
            assert!(!key.contains("secret"));
            assert!(!key.contains("passphrase"));
            assert!(!key.contains("private"));
            assert!(!key.contains("ciphertext"));
        }
    }

    #[tokio::test]
    async fn signing_is_refused_when_no_keystore_is_configured() {
        let passphrase = "this-passphrase-must-not-leak";
        let response = wallet_sign_dry_run(
            Extension(unconfigured_wallet()),
            Json(WalletSignRequest {
                passphrase: passphrase.to_string(),
                envelope: envelope(),
                account: AccountSelector::default(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, 64 * 1024).await.unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains(passphrase));
        assert!(text.contains("wallet_not_configured"));
    }

    #[tokio::test]
    async fn live_broadcast_is_refused_even_with_a_keystore() {
        let wallet = Arc::new(WalletService::new(WalletConfig {
            keystore_path: None,
            chain: Some(Chain::Evm),
            dry_run: false,
            unlock_timeout_ms: 1_000,
            accounts: 5,
            ..Default::default()
        }));
        let response = wallet_sign_dry_run(
            Extension(wallet),
            Json(WalletSignRequest {
                passphrase: "unused".to_string(),
                envelope: envelope(),
                account: AccountSelector::default(),
            }),
        )
        .await;
        // The wallet is not configured, so the request is refused before the
        // dry-run flag is even considered.
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    /// End to end test through the real router: the wallet metadata is
    /// public and the dry-run endpoint signs without ever broadcasting.
    #[tokio::test]
    async fn the_router_exposes_the_wallet_endpoints() {
        let dir = temp_dir("router");
        let path = dir.join("wallet.json");
        // Obviously fake, runtime generated key.
        let mut secret = vec![0u8; 32];
        OsRng.try_fill_bytes(&mut secret).unwrap();
        let passphrase = "a-test-passphrase";
        let metadata = crate::wallet::create_keystore(
            &path,
            Chain::Evm,
            &secret,
            passphrase,
            crate::wallet::KdfParams {
                m_cost_kib: 8 * 1024,
                t_cost: 1,
                p_cost: 1,
            },
        )
        .unwrap();

        let mut config = Config::from_env();
        config.venue = Venue::Paper;
        config.live_trading = false;
        config.wallet = WalletConfig {
            keystore_path: Some(path.clone()),
            chain: Some(Chain::Evm),
            dry_run: true,
            unlock_timeout_ms: 5_000,
            accounts: 5,
            ..Default::default()
        };
        let state = crate::engine::AppState::new(config).unwrap();
        let app = router(Arc::clone(&state));

        // GET /api/wallet
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/wallet")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes).to_string();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["configured"], serde_json::json!(true));
        assert_eq!(value["chain"], serde_json::json!("evm"));
        assert_eq!(value["address"], serde_json::json!(metadata.address));
        assert_eq!(value["encrypted"], serde_json::json!(true));
        assert_eq!(value["dry_run"], serde_json::json!(true));
        // No secret may appear in the metadata.
        assert!(!text.contains(&hex::encode(&secret)));
        assert!(!text.contains("passphrase"));
        assert!(!text.contains("secret"));
        assert!(!text.contains("ciphertext"));

        // POST /api/wallet/sign-dry-run
        let body = serde_json::json!({
            "passphrase": passphrase,
            "envelope": envelope(),
        })
        .to_string();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/wallet/sign-dry-run")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes).to_string();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["dry_run"], serde_json::json!(true));
        assert_eq!(value["broadcast"], serde_json::json!(false));
        assert!(value["tx_hash"].as_str().unwrap().starts_with("0x"));
        assert!(value["signature"].as_str().unwrap().starts_with("0x"));
        assert!(!text.contains(passphrase));
        assert!(!text.contains(&hex::encode(&secret)));

        // A wrong passphrase is refused with 401 and leaks nothing.
        let body = serde_json::json!({
            "passphrase": "wrong-passphrase",
            "envelope": envelope(),
        })
        .to_string();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/wallet/sign-dry-run")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes).to_string();
        assert!(!text.contains("wrong-passphrase"));
        assert!(text.contains("invalid_passphrase"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The order surface end to end through the real router.
    ///
    /// The agent's MCP tool sends no venue unless the caller overrides it, so an
    /// omitted venue must mean "the engine's" rather than a 422 — otherwise the
    /// agent can never trade at all.
    #[tokio::test]
    async fn an_order_without_a_venue_rests_and_can_be_cancelled() {
        let dir = temp_dir("orders");
        let paper = Arc::new(crate::exchange::paper::PaperConnector::new());
        paper.set_price("SOL/USDT", 142.0);
        let mut config = Config::from_env();
        config.venue = Venue::Paper;
        config.live_trading = false;
        config.paper_start_equity = 10_000.0;
        config.state_db_path = Some(dir.join("state.db"));
        config.strategy_wasm = None;
        let state = crate::engine::AppState::with_connector(config, paper).unwrap();
        let app = router(Arc::clone(&state));

        let post = |body: serde_json::Value| {
            let app = app.clone();
            async move {
                let response = app
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri("/api/orders")
                            .header("content-type", "application/json")
                            .body(Body::from(body.to_string()))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let status = response.status();
                let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
                    .await
                    .unwrap();
                (status, String::from_utf8_lossy(&bytes).to_string())
            }
        };

        let (status, text) = post(json!({
            "symbol": "SOL/USDT", "side": "buy", "qty": 1.0,
            "order_type": "limit", "limit_price": 130.0,
        }))
        .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let outcome: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(outcome["approved"], json!(true));
        assert_eq!(outcome["status"], json!("accepted"));
        let order_id = outcome["order_id"].as_str().unwrap().to_string();

        // Resting is not a position: nothing is booked and no cash moved.
        let orders = state.orders();
        assert_eq!(orders.len(), 1);
        assert!(orders[0].fills.is_empty());
        assert!(state.portfolio().positions.is_empty());
        assert_eq!(state.equity(), 10_000.0);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/orders")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let listed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(listed[0]["order_id"], json!(order_id));
        assert_eq!(listed[0]["venue"], json!("paper"));
        assert_eq!(listed[0]["fills"], json!([]));

        let cancel = |id: &str| {
            let app = app.clone();
            let uri = format!("/api/orders/{id}");
            async move {
                let response = app
                    .oneshot(
                        Request::builder()
                            .method("DELETE")
                            .uri(uri)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let status = response.status();
                let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
                    .await
                    .unwrap();
                (status, String::from_utf8_lossy(&bytes).to_string())
            }
        };

        let (status, text) = cancel(&order_id).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert_eq!(state.orders()[0].status, OrderStatus::Cancelled);

        // A second cancel is refused: the order is no longer resting.
        let (status, text) = cancel(&order_id).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            text.contains("only a resting order can be cancelled"),
            "{text}"
        );

        let (status, text) = cancel("does-not-exist").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(text.contains("unknown order"), "{text}");

        // A body that is not an object is a 422, not a panic.
        let (status, _) = post(json!(5)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        std::fs::remove_dir_all(&dir).ok();
    }
}

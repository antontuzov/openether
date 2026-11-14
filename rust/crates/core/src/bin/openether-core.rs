use openether_core::api;
use openether_core::config::Config;
use openether_core::engine::{self, AppState};
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env();
    config.check_serve_safety().map_err(anyhow::Error::msg)?;
    if !config.admin_auth.enabled() {
        tracing::warn!(
            "admin auth is disabled: whoever can reach this port can reset the \
             breaker. Set OPENETHER_ADMIN_TOKEN before arming live trading."
        );
    }
    let addr = config.addr();
    let venue = config.venue;
    let live = config.live_trading;

    let state = AppState::new(config)?;
    state.publish_status("running", "engine started");
    state.log(
        "engine",
        "info",
        &format!(
            "openEther core started on venue {venue} (live trading: {})",
            if live { "ENABLED" } else { "disabled" }
        ),
        serde_json::json!({"venue": venue.as_str(), "live_trading": live}),
    );

    engine::run_background_tasks(Arc::clone(&state)).await;

    let app = api::router(Arc::clone(&state));
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "openEther core is listening");
    axum::serve(listener, app).await?;
    Ok(())
}

//! Webhook alerts for the events an operator must not sleep through.
//!
//! The breaker opening is the one event that stops trading by design, so it is
//! pushed out as soon as it happens instead of waiting for someone to load a
//! dashboard. Delivery is fire-and-forget on a spawned task: a slow or dead
//! webhook must never stall the path that just halted trading.

use crate::risk::{BreakerState, RiskState};
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Clone)]
pub struct Alerter {
    webhook: Option<String>,
    http: reqwest::Client,
}

impl Alerter {
    pub fn new(webhook: Option<String>) -> Self {
        let webhook = webhook
            .map(|url| url.trim().to_string())
            .filter(|url| !url.is_empty());
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        Self { webhook, http }
    }

    pub fn from_env() -> Self {
        Self::new(std::env::var("OPENETHER_ALERT_WEBHOOK").ok())
    }

    pub fn enabled(&self) -> bool {
        self.webhook.is_some()
    }

    /// The body a webhook receives when the breaker halts trading.
    pub fn breaker_opened(&self, risk: &RiskState, ts_ms: i64) -> Value {
        json!({
            "source": "openether",
            "event": "breaker_opened",
            "ts_ms": ts_ms,
            "severity": "critical",
            "reason": risk.breaker_reason,
            "breaker_state": risk.breaker_state,
            "equity_usd": risk.equity_usd,
            "peak_equity_usd": risk.peak_equity_usd,
            "drawdown_pct": risk.drawdown_pct,
            "daily_pnl_usd": risk.daily_pnl_usd,
            "gross_exposure_usd": risk.gross_exposure_usd,
        })
    }

    /// The body a webhook receives when an operator resets the breaker.
    pub fn breaker_reset(&self, ts_ms: i64) -> Value {
        json!({
            "source": "openether",
            "event": "breaker_reset",
            "ts_ms": ts_ms,
            "severity": "warning",
        })
    }

    /// Post the payload without waiting for it. A webhook failure is logged and
    /// otherwise ignored: alerting is a convenience, not a trading condition.
    pub fn send(&self, payload: Value) {
        let (Some(url), http) = (self.webhook.clone(), self.http.clone()) else {
            return;
        };
        tokio::spawn(async move {
            match http.post(&url).json(&payload).send().await {
                Ok(response) if response.status().is_success() => {
                    tracing::info!(event = %payload["event"], "alert delivered");
                }
                Ok(response) => {
                    tracing::warn!(status = %response.status(), "alert webhook refused the alert");
                }
                Err(error) => {
                    tracing::warn!(error = %error, "alert webhook could not be reached");
                }
            }
        });
    }
}

/// Remembers the last breaker state so one opening produces one alert.
#[derive(Debug, Default)]
pub struct BreakerWatch {
    last: Option<BreakerState>,
}

impl BreakerWatch {
    /// Returns `true` only on a transition *into* `open`.
    pub fn note(&mut self, state: BreakerState) -> bool {
        let previous = self.last.replace(state);
        previous != Some(state) && state == BreakerState::Open
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn risk_state() -> RiskState {
        RiskState {
            breaker_state: BreakerState::Open,
            breaker_reason: Some("drawdown_limit".into()),
            equity_usd: 8_800.0,
            peak_equity_usd: 10_000.0,
            drawdown_pct: 12.0,
            daily_pnl_usd: -1200.0,
            open_positions: 1,
            gross_exposure_usd: 1500.0,
            limits: crate::risk::RiskLimits::from(&crate::risk::RiskConfig::default()),
            ts_ms: 1,
        }
    }

    #[test]
    fn a_blank_webhook_is_the_same_as_no_webhook() {
        assert!(!Alerter::new(None).enabled());
        assert!(!Alerter::new(Some("  ".into())).enabled());
        assert!(Alerter::new(Some("http://127.0.0.1:9/hook".into())).enabled());
    }

    #[test]
    fn the_breaker_payload_says_what_tripped_and_what_it_cost() {
        let body = Alerter::new(None).breaker_opened(&risk_state(), 1700000000000);
        assert_eq!(body["event"], "breaker_opened");
        assert_eq!(body["severity"], "critical");
        assert_eq!(body["reason"], "drawdown_limit");
        assert_eq!(body["breaker_state"], "open");
        assert_eq!(body["equity_usd"], 8_800.0);
        assert_eq!(body["drawdown_pct"], 12.0);
        assert_eq!(body["ts_ms"], 1_700_000_000_000i64);
    }

    #[test]
    fn one_opening_is_reported_once() {
        let mut watch = BreakerWatch::default();
        assert!(!watch.note(BreakerState::Closed));
        assert!(watch.note(BreakerState::Open), "the opening is news");
        assert!(!watch.note(BreakerState::Open), "still open is not news");
        assert!(!watch.note(BreakerState::HalfOpen));
        assert!(watch.note(BreakerState::Open), "a fresh opening is news");
    }

    /// Deliver a real HTTP request to a socket we own, so the transport is
    /// exercised rather than mocked.
    #[tokio::test]
    async fn a_webhook_receives_the_json_body() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0u8; 8192];
            let read = socket.read(&mut buffer).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
                .await
                .unwrap();
            String::from_utf8_lossy(&buffer[..read]).to_string()
        });

        let alerter = Alerter::new(Some(format!("http://{address}/hook")));
        alerter.send(alerter.breaker_reset(42));
        let request = server.await.unwrap();
        assert!(request.starts_with("POST /hook HTTP/1.1"), "{request}");
        assert!(request.contains("\"event\":\"breaker_reset\""), "{request}");
        assert!(request.contains("\"ts_ms\":42"), "{request}");
    }
}

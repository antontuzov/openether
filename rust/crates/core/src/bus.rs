use crate::metrics::Metrics;
use crate::risk::RiskState;
use crate::types::*;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// Events routed through the engine. The serialized shape matches the WebSocket
/// contract: `{"type": "<kind>", "data": { ... }}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "lowercase")]
pub enum Event {
    Tick(MarketTick),
    Trade(TradeRecord),
    Log(LogEntry),
    Portfolio(PortfolioSnapshot),
    Risk(RiskState),
    Status(StatusEvent),
}

impl Event {
    pub fn kind(&self) -> &'static str {
        match self {
            Event::Tick(_) => "tick",
            Event::Trade(_) => "trade",
            Event::Log(_) => "log",
            Event::Portfolio(_) => "portfolio",
            Event::Risk(_) => "risk",
            Event::Status(_) => "status",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusEvent {
    pub state: String,
    #[serde(default)]
    pub detail: String,
    pub ts_ms: i64,
}

/// Broadcast bus shared by market data, execution, risk and the API layer.
pub struct EventBus {
    tx: broadcast::Sender<Event>,
    published: AtomicU64,
    metrics: Arc<Metrics>,
}

impl EventBus {
    pub fn new(capacity: usize, metrics: Arc<Metrics>) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self {
            tx,
            published: AtomicU64::new(0),
            metrics,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    /// Publishes an event and returns the time spent enqueuing it.
    ///
    /// Dispatch cost is the enqueue only: subscribers are woken asynchronously by
    /// the runtime, so a slow consumer can never stall the publisher.
    pub fn publish(&self, event: Event) -> Duration {
        let started = Instant::now();
        let _ = self.tx.send(event);
        let elapsed = started.elapsed();
        self.published.fetch_add(1, Ordering::Relaxed);
        self.metrics
            .counter_add("openether_events_published_total", &[], 1.0);
        self.metrics.observe(
            "openether_event_dispatch_seconds",
            &[],
            elapsed.as_secs_f64(),
        );
        elapsed
    }

    pub fn published_count(&self) -> u64 {
        self.published.load(Ordering::Relaxed)
    }

    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_latency_stays_below_target() {
        let metrics = Arc::new(Metrics::new());
        let bus = EventBus::new(1024, metrics.clone());

        let mut samples = Vec::with_capacity(20_000);
        for i in 0..20_000 {
            let ev = Event::Tick(MarketTick {
                venue: Venue::Paper,
                symbol: "SOL/USDT".into(),
                price: 142.0 + (i as f64) * 0.0001,
                size: 1.0,
                ts_ms: now_ms(),
            });
            samples.push(bus.publish(ev));
        }

        samples.sort();
        let p50 = samples[samples.len() / 2];
        let p99 = samples[samples.len() * 99 / 100];
        let max = *samples.last().unwrap();

        // The dispatch target is < 10us. Generous headroom keeps the test stable
        // on loaded CI machines while still catching a regression to queue-based
        // dispatch.
        assert!(
            p50 < Duration::from_micros(10),
            "p50 dispatch latency {:?} exceeds the 10us target",
            p50
        );
        assert!(
            p99 < Duration::from_micros(500),
            "p99 dispatch latency {:?}",
            p99
        );
        assert!(
            max < Duration::from_millis(5),
            "max dispatch latency {:?}",
            max
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subscribers_receive_events() {
        let metrics = Arc::new(Metrics::new());
        let bus = EventBus::new(16, metrics);
        let mut rx = bus.subscribe();

        bus.publish(Event::Status(StatusEvent {
            state: "running".into(),
            detail: "test".into(),
            ts_ms: now_ms(),
        }));

        match rx.recv().await.unwrap() {
            Event::Status(s) => assert_eq!(s.state, "running"),
            other => panic!("unexpected event {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_subscriber_does_not_block_publisher() {
        let metrics = Arc::new(Metrics::new());
        let bus = EventBus::new(4, metrics);
        let _slow = bus.subscribe(); // never polled: the channel fills up
        for _ in 0..100 {
            bus.publish(Event::Tick(MarketTick {
                venue: Venue::Paper,
                symbol: "SOL/USDT".into(),
                price: 1.0,
                size: 1.0,
                ts_ms: now_ms(),
            }));
        }
        assert_eq!(bus.published_count(), 100);
    }
}

use std::collections::HashMap;
use std::sync::Mutex;

/// Minimal Prometheus registry.
///
/// The engine only needs counters, gauges and one histogram family, so the
/// implementation stays dependency-free and the exposition format is
/// guaranteed to be valid Prometheus text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    Counter,
    Gauge,
    Histogram,
}

#[derive(Debug, Clone)]
struct MetricDef {
    name: &'static str,
    help: &'static str,
    kind: MetricKind,
    buckets: &'static [f64],
}

const DEFAULT_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

const DEFS: &[MetricDef] = &[
    MetricDef {
        name: "openether_events_published_total",
        help: "Events published on the internal bus",
        kind: MetricKind::Counter,
        buckets: &[],
    },
    MetricDef {
        name: "openether_event_dispatch_seconds",
        help: "Bus dispatch latency in seconds",
        kind: MetricKind::Histogram,
        buckets: DEFAULT_BUCKETS,
    },
    MetricDef {
        name: "openether_orders_submitted_total",
        help: "Orders submitted to a venue",
        kind: MetricKind::Counter,
        buckets: &[],
    },
    MetricDef {
        name: "openether_orders_filled_total",
        help: "Orders that produced a fill",
        kind: MetricKind::Counter,
        buckets: &[],
    },
    MetricDef {
        name: "openether_orders_rejected_total",
        help: "Orders rejected by the risk layer",
        kind: MetricKind::Counter,
        buckets: &[],
    },
    MetricDef {
        name: "openether_risk_checks_total",
        help: "Risk evaluations by result",
        kind: MetricKind::Counter,
        buckets: &[],
    },
    MetricDef {
        name: "openether_breaker_trips_total",
        help: "Circuit breaker trips",
        kind: MetricKind::Counter,
        buckets: &[],
    },
    MetricDef {
        name: "openether_breaker_state",
        help: "Circuit breaker state (0=closed 1=half_open 2=open)",
        kind: MetricKind::Gauge,
        buckets: &[],
    },
    MetricDef {
        name: "openether_equity_usd",
        help: "Portfolio equity in USD",
        kind: MetricKind::Gauge,
        buckets: &[],
    },
    MetricDef {
        name: "openether_pnl_usd",
        help: "Realized PnL in USD",
        kind: MetricKind::Gauge,
        buckets: &[],
    },
    MetricDef {
        name: "openether_drawdown_pct",
        help: "Drawdown from peak equity in percent",
        kind: MetricKind::Gauge,
        buckets: &[],
    },
    MetricDef {
        name: "openether_gross_exposure_usd",
        help: "Gross exposure in USD",
        kind: MetricKind::Gauge,
        buckets: &[],
    },
    MetricDef {
        name: "openether_strategy_ticks_total",
        help: "Ticks processed by the WASM strategy",
        kind: MetricKind::Counter,
        buckets: &[],
    },
    MetricDef {
        name: "openether_strategy_errors_total",
        help: "Strategy sandbox errors",
        kind: MetricKind::Counter,
        buckets: &[],
    },
    MetricDef {
        name: "openether_wasm_fuel_used",
        help: "Fuel consumed by the strategy sandbox",
        kind: MetricKind::Gauge,
        buckets: &[],
    },
    MetricDef {
        name: "openether_ws_connections",
        help: "Open WebSocket clients on the core",
        kind: MetricKind::Gauge,
        buckets: &[],
    },
    MetricDef {
        name: "openether_http_requests_total",
        help: "HTTP requests handled by the core",
        kind: MetricKind::Counter,
        buckets: &[],
    },
    MetricDef {
        name: "openether_agent_llm_calls_total",
        help: "LLM calls made by the AI agent",
        kind: MetricKind::Counter,
        buckets: &[],
    },
    MetricDef {
        name: "openether_agent_llm_latency_seconds",
        help: "LLM call latency in seconds",
        kind: MetricKind::Histogram,
        buckets: DEFAULT_BUCKETS,
    },
    MetricDef {
        name: "openether_agent_steps_total",
        help: "Agent plan/execute/verify steps",
        kind: MetricKind::Counter,
        buckets: &[],
    },
];

#[derive(Debug, Default, Clone)]
struct Histogram {
    buckets: Vec<f64>,
    counts: Vec<u64>,
    sum: f64,
    count: u64,
}

impl Histogram {
    fn new(buckets: &[f64]) -> Self {
        Self {
            buckets: buckets.to_vec(),
            counts: vec![0; buckets.len()],
            sum: 0.0,
            count: 0,
        }
    }

    fn observe(&mut self, value: f64) {
        self.sum += value;
        self.count += 1;
        for (i, upper) in self.buckets.iter().enumerate() {
            if value <= *upper {
                self.counts[i] += 1;
            }
        }
    }
}

#[derive(Debug, Default)]
struct Inner {
    counters: HashMap<(String, String), f64>,
    gauges: HashMap<(String, String), f64>,
    histograms: HashMap<(String, String), Histogram>,
}

#[derive(Debug, Default)]
pub struct Metrics {
    inner: Mutex<Inner>,
}

fn label_key(labels: &[(&str, &str)]) -> String {
    if labels.is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = labels
        .iter()
        .map(|(k, v)| format!("{}=\"{}\"", k, escape_label(v)))
        .collect();
    parts.sort();
    parts.join(",")
}

fn escape_label(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn counter_add(&self, name: &str, labels: &[(&str, &str)], value: f64) {
        let mut inner = self.lock();
        *inner
            .counters
            .entry((name.to_string(), label_key(labels)))
            .or_insert(0.0) += value;
    }

    pub fn gauge_set(&self, name: &str, labels: &[(&str, &str)], value: f64) {
        let mut inner = self.lock();
        inner
            .gauges
            .insert((name.to_string(), label_key(labels)), value);
    }

    pub fn observe(&self, name: &str, labels: &[(&str, &str)], value: f64) {
        let def = DEFS.iter().find(|d| d.name == name);
        let buckets = def.map(|d| d.buckets).unwrap_or(DEFAULT_BUCKETS);
        let mut inner = self.lock();
        let entry = inner
            .histograms
            .entry((name.to_string(), label_key(labels)))
            .or_insert_with(|| Histogram::new(buckets));
        entry.observe(value);
    }

    pub fn counter_value(&self, name: &str, labels: &[(&str, &str)]) -> f64 {
        self.lock()
            .counters
            .get(&(name.to_string(), label_key(labels)))
            .copied()
            .unwrap_or_default()
    }

    pub fn gauge_value(&self, name: &str, labels: &[(&str, &str)]) -> f64 {
        self.lock()
            .gauges
            .get(&(name.to_string(), label_key(labels)))
            .copied()
            .unwrap_or_default()
    }

    pub fn histogram_snapshot(&self, name: &str) -> (u64, f64) {
        let inner = self.lock();
        inner
            .histograms
            .iter()
            .filter(|((n, _), _)| n == name)
            .fold((0u64, 0.0), |(c, s), (_, h)| (c + h.count, s + h.sum))
    }

    pub fn encode(&self) -> String {
        let inner = self.lock();
        let mut out = String::with_capacity(4096);

        for def in DEFS {
            out.push_str(&format!("# HELP {} {}\n", def.name, def.help));
            match def.kind {
                MetricKind::Counter => out.push_str(&format!("# TYPE {} counter\n", def.name)),
                MetricKind::Gauge => out.push_str(&format!("# TYPE {} gauge\n", def.name)),
                MetricKind::Histogram => out.push_str(&format!("# TYPE {} histogram\n", def.name)),
            }

            match def.kind {
                MetricKind::Counter => {
                    let mut entries: Vec<_> = inner
                        .counters
                        .iter()
                        .filter(|((n, _), _)| n == def.name)
                        .collect();
                    entries.sort_by(|a, b| a.0 .1.cmp(&b.0 .1));
                    for ((_, labels), value) in entries {
                        out.push_str(&format!("{}{} {}\n", def.name, label_suffix(labels), value));
                    }
                }
                MetricKind::Gauge => {
                    let mut entries: Vec<_> = inner
                        .gauges
                        .iter()
                        .filter(|((n, _), _)| n == def.name)
                        .collect();
                    entries.sort_by(|a, b| a.0 .1.cmp(&b.0 .1));
                    for ((_, labels), value) in entries {
                        out.push_str(&format!("{}{} {}\n", def.name, label_suffix(labels), value));
                    }
                }
                MetricKind::Histogram => {
                    let mut entries: Vec<_> = inner
                        .histograms
                        .iter()
                        .filter(|((n, _), _)| n == def.name)
                        .collect();
                    entries.sort_by(|a, b| a.0 .1.cmp(&b.0 .1));
                    for ((_, labels), hist) in entries {
                        for (i, upper) in hist.buckets.iter().enumerate() {
                            out.push_str(&format!(
                                "{}_bucket{} {}\n",
                                def.name,
                                bucket_suffix(labels, *upper),
                                hist.counts[i]
                            ));
                        }
                        let suffix = label_suffix(labels);
                        out.push_str(&format!("{}_sum{} {}\n", def.name, suffix, hist.sum));
                        out.push_str(&format!("{}_count{} {}\n", def.name, suffix, hist.count));
                    }
                }
            }
        }
        out
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn label_suffix(labels: &str) -> String {
    if labels.is_empty() {
        String::new()
    } else {
        format!("{{{}}}", labels)
    }
}

fn bucket_suffix(labels: &str, upper: f64) -> String {
    let combined = if labels.is_empty() {
        format!("le=\"{}\"", upper)
    } else {
        format!("{},le=\"{}\"", labels, upper)
    };
    format!("{{{}}}", combined)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_gauges_and_histograms_encode() {
        let m = Metrics::new();
        m.counter_add(
            "openether_orders_submitted_total",
            &[("venue", "paper")],
            2.0,
        );
        m.gauge_set("openether_equity_usd", &[], 12_345.5);
        m.observe("openether_event_dispatch_seconds", &[], 0.000_004);

        let text = m.encode();
        assert!(text.contains("openether_orders_submitted_total{venue=\"paper\"} 2"));
        assert!(text.contains("openether_equity_usd 12345.5"));
        assert!(text.contains("openether_event_dispatch_seconds_count"));
        assert!(text.contains("# TYPE openether_equity_usd gauge"));
        assert!(text.contains("openether_event_dispatch_seconds_bucket{le=\"0.001\"} 1"));
    }

    #[test]
    fn labels_are_escaped_and_sorted() {
        let m = Metrics::new();
        m.counter_add(
            "openether_risk_checks_total",
            &[("b", "2"), ("a", "x\"y")],
            1.0,
        );
        let text = m.encode();
        assert!(text.contains("openether_risk_checks_total{a=\"x\\\"y\",b=\"2\"} 1"));
    }
}

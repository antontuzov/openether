use crate::metrics::Metrics;
use crate::types::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use wasmtime::{Caller, Config, Engine, Extern, Linker, Module, Store, StoreLimitsBuilder};

/// A trade proposal produced by a sandboxed strategy.
///
/// Strategies never place orders themselves: they return proposals that the
/// engine forwards to the risk layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategySignal {
    pub symbol: String,
    pub side: Side,
    pub qty: f64,
    #[serde(default)]
    pub confidence: f64,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, thiserror::Error)]
pub enum StrategyError {
    #[error("strategy module could not be loaded: {0}")]
    Load(String),
    #[error("strategy call failed: {0}")]
    Call(String),
    #[error("strategy produced an invalid result: {0}")]
    InvalidResult(String),
    #[error("strategy is not loaded")]
    NotLoaded,
}

#[derive(Debug, Default)]
struct HostState {
    limits: wasmtime::StoreLimits,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyInfo {
    pub name: String,
    pub path: String,
    pub loaded_at_ms: i64,
    pub wasm_sha256: String,
    pub ticks_processed: u64,
    pub fuel_used: u64,
    pub last_error: Option<String>,
}

struct Loaded {
    path: String,
    store: Store<HostState>,
    instance: wasmtime::Instance,
    name: String,
    sha256: String,
    loaded_at_ms: i64,
    ticks_processed: u64,
    fuel_used: u64,
    last_error: Option<String>,
}

/// Executes WASM strategies inside a sandbox with fuel and memory limits.
///
/// Guest ABI (all exports are `i32` based):
///   `openether_alloc(len) -> ptr`       allocate `len` bytes of guest memory
///   `openether_dealloc(ptr, len)`       release a buffer
///   `openether_on_tick(ctx_ptr, ctx_len) -> ptr`  run one tick, returns a
///       NUL-terminated JSON document allocated by the guest
///
/// Host imports (module `openether`):
///   `log(ptr, len)`                     guest log line
pub struct StrategyHost {
    engine: Engine,
    path: PathBuf,
    fuel_per_tick: u64,
    max_memory_bytes: usize,
    logs: Arc<Mutex<Vec<String>>>,
    metrics: Arc<Metrics>,
    state: Mutex<Option<Loaded>>,
}

impl StrategyHost {
    pub fn new(
        path: impl AsRef<Path>,
        fuel_per_tick: u64,
        max_memory_pages: u32,
        metrics: Arc<Metrics>,
    ) -> Result<Self, StrategyError> {
        let mut config = Config::new();
        config.consume_fuel(true);
        config.wasm_multi_memory(false);
        config.wasm_bulk_memory(true);
        config.wasm_reference_types(true);
        // The guest cannot reach the host clock or random number generator, and
        // it cannot open files or sockets: the sandbox is capability-free.
        config.epoch_interruption(false);
        let engine = Engine::new(&config).map_err(|e| StrategyError::Load(e.to_string()))?;

        let path = path.as_ref().to_path_buf();
        let (module, sha256, name) = load_module(&engine, &path)?;
        let max_memory_bytes = (max_memory_pages as usize) * 64 * 1024;
        let host = Self {
            engine,
            path,
            fuel_per_tick,
            max_memory_bytes,
            logs: Arc::new(Mutex::new(Vec::new())),
            metrics,
            state: Mutex::new(None),
        };
        let loaded = host.instantiate(module, sha256, name)?;
        *host.state.lock().unwrap_or_else(|e| e.into_inner()) = Some(loaded);
        Ok(host)
    }

    fn instantiate(
        &self,
        module: Module,
        sha256: String,
        name: String,
    ) -> Result<Loaded, StrategyError> {
        let mut store = Store::new(
            &self.engine,
            HostState {
                limits: StoreLimitsBuilder::new()
                    .memory_size(self.max_memory_bytes)
                    .build(),
            },
        );
        store.limiter(|s| &mut s.limits);

        let logs = Arc::clone(&self.logs);
        let mut linker = Linker::new(&self.engine);
        linker
            .func_wrap(
                "openether",
                "log",
                move |mut caller: Caller<'_, HostState>,
                      ptr: i32,
                      len: i32|
                      -> Result<(), wasmtime::Error> {
                    if let Some(Extern::Memory(memory)) = caller.get_export("memory") {
                        let data = memory.data(&caller);
                        let start = ptr.max(0) as usize;
                        let end = (start + len.max(0) as usize).min(data.len());
                        if start < end {
                            let message = String::from_utf8_lossy(&data[start..end]).to_string();
                            if let Ok(mut buf) = logs.lock() {
                                buf.push(message.clone());
                                if buf.len() > 512 {
                                    buf.remove(0);
                                }
                            }
                            tracing::debug!(target: "wasm_strategy", "{message}");
                        }
                    }
                    Ok(())
                },
            )
            .map_err(|e| StrategyError::Load(e.to_string()))?;

        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|e| StrategyError::Load(e.to_string()))?;

        Ok(Loaded {
            path: self.path.display().to_string(),
            store,
            instance,
            name,
            sha256,
            loaded_at_ms: now_ms(),
            ticks_processed: 0,
            fuel_used: 0,
            last_error: None,
        })
    }

    /// Runs one tick against the sandboxed strategy.
    pub fn on_tick(&self, ctx: &serde_json::Value) -> Result<Vec<StrategySignal>, StrategyError> {
        let ctx_bytes =
            serde_json::to_vec(ctx).map_err(|e| StrategyError::InvalidResult(e.to_string()))?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let loaded = state.as_mut().ok_or(StrategyError::NotLoaded)?;

        let alloc = loaded
            .instance
            .get_typed_func::<i32, i32>(&mut loaded.store, "openether_alloc")
            .map_err(|e| StrategyError::Load(format!("missing openether_alloc: {e}")))?;
        let dealloc = loaded
            .instance
            .get_typed_func::<(i32, i32), ()>(&mut loaded.store, "openether_dealloc")
            .map_err(|e| StrategyError::Load(format!("missing openether_dealloc: {e}")))?;
        let on_tick = loaded
            .instance
            .get_typed_func::<(i32, i32), i32>(&mut loaded.store, "openether_on_tick")
            .map_err(|e| StrategyError::Load(format!("missing openether_on_tick: {e}")))?;

        loaded
            .store
            .set_fuel(self.fuel_per_tick)
            .map_err(|e| StrategyError::Call(e.to_string()))?;

        let ctx_ptr = alloc
            .call(&mut loaded.store, ctx_bytes.len() as i32)
            .map_err(|e| StrategyError::Call(format!("alloc failed: {e}")))?;
        if ctx_ptr == 0 {
            return Err(StrategyError::Call("guest allocation failed".into()));
        }
        {
            let memory = loaded
                .instance
                .get_memory(&mut loaded.store, "memory")
                .ok_or_else(|| StrategyError::Load("guest does not export memory".into()))?;
            let data = memory.data_mut(&mut loaded.store);
            let start = ctx_ptr as usize;
            let end = start + ctx_bytes.len();
            if end > data.len() {
                return Err(StrategyError::Call("guest memory too small".into()));
            }
            data[start..end].copy_from_slice(&ctx_bytes);
        }

        let result_ptr = on_tick
            .call(&mut loaded.store, (ctx_ptr, ctx_bytes.len() as i32))
            .map_err(|e| StrategyError::Call(format!("on_tick trapped: {e}")))?;

        let remaining = loaded.store.get_fuel().unwrap_or(0);
        let used = self.fuel_per_tick.saturating_sub(remaining);
        loaded.fuel_used += used;
        loaded.ticks_processed += 1;
        self.metrics
            .counter_add("openether_strategy_ticks_total", &[], 1.0);
        self.metrics
            .gauge_set("openether_wasm_fuel_used", &[], loaded.fuel_used as f64);

        let raw = {
            let memory = loaded
                .instance
                .get_memory(&mut loaded.store, "memory")
                .ok_or_else(|| StrategyError::Load("guest does not export memory".into()))?;
            let (bytes, len) = {
                let data = memory.data(&loaded.store);
                let start = (result_ptr.max(0)) as usize;
                if start >= data.len() {
                    return Err(StrategyError::InvalidResult(
                        "strategy returned an out-of-bounds pointer".into(),
                    ));
                }
                let tail = &data[start..];
                let len = tail.iter().position(|b| *b == 0).unwrap_or(tail.len());
                (tail[..len].to_vec(), len)
            };
            let _ = dealloc.call(&mut loaded.store, (result_ptr, len as i32));
            bytes
        };
        let _ = dealloc.call(&mut loaded.store, (ctx_ptr, ctx_bytes.len() as i32));

        if raw.is_empty() {
            loaded.last_error = None;
            return Ok(Vec::new());
        }

        let parsed: serde_json::Value = serde_json::from_slice(&raw).map_err(|e| {
            StrategyError::InvalidResult(format!("{e}: {}", String::from_utf8_lossy(&raw)))
        })?;

        let signals = parsed
            .get("signals")
            .and_then(|s| s.as_array())
            .map(|arr| arr.iter().filter_map(parse_signal).collect::<Vec<_>>())
            .unwrap_or_default();

        loaded.last_error = None;
        Ok(signals)
    }

    /// Hot-swaps the strategy module without restarting the engine.
    pub fn reload(&self, path: impl AsRef<Path>) -> Result<StrategyInfo, StrategyError> {
        let path = path.as_ref().to_path_buf();
        let (module, sha256, name) = load_module(&self.engine, &path)?;
        let loaded = self.instantiate(module, sha256, name)?;
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            *state = Some(loaded);
        }
        Ok(self.info())
    }

    pub fn info(&self) -> StrategyInfo {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match state.as_ref() {
            Some(l) => StrategyInfo {
                name: l.name.clone(),
                path: l.path.clone(),
                loaded_at_ms: l.loaded_at_ms,
                wasm_sha256: l.sha256.clone(),
                ticks_processed: l.ticks_processed,
                fuel_used: l.fuel_used,
                last_error: l.last_error.clone(),
            },
            None => StrategyInfo {
                name: "none".into(),
                path: self.path.display().to_string(),
                loaded_at_ms: 0,
                wasm_sha256: String::new(),
                ticks_processed: 0,
                fuel_used: 0,
                last_error: Some("no strategy loaded".into()),
            },
        }
    }

    pub fn logs(&self) -> Vec<String> {
        self.logs.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn parse_signal(value: &serde_json::Value) -> Option<StrategySignal> {
    let symbol = value.get("symbol")?.as_str()?.to_string();
    let side = Side::parse(value.get("side")?.as_str()?)?;
    let qty = value.get("qty")?.as_f64()?;
    if !qty.is_finite() || qty <= 0.0 {
        return None;
    }
    Some(StrategySignal {
        symbol,
        side,
        qty,
        confidence: value
            .get("confidence")
            .and_then(|c| c.as_f64())
            .unwrap_or(0.0)
            .clamp(0.0, 1.0),
        reason: value
            .get("reason")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

fn load_module(engine: &Engine, path: &Path) -> Result<(Module, String, String), StrategyError> {
    let bytes =
        std::fs::read(path).map_err(|e| StrategyError::Load(format!("{}: {e}", path.display())))?;
    let sha256 = to_hex(&Sha256::digest(&bytes));
    let module = Module::new(engine, &bytes)
        .map_err(|e| StrategyError::Load(format!("{}: {e}", path.display())))?;
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "strategy".into());
    Ok((module, sha256, name))
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((b & 0x0f) as u32, 16).unwrap_or('0'));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a tiny WASM module that returns a fixed JSON document, exercising
    /// the host ABI without needing a Rust guest toolchain.
    fn wasm_returning(payload: &str, min_pages: u32) -> Vec<u8> {
        // (module
        //   (memory (export "memory") 1)
        //   (func (export "openether_alloc") (param i32) (result i32) local.get 0)
        //   (func (export "openether_dealloc"))
        //   (func (export "openether_on_tick") (param i32 i32) (result i32)
        //     i32.const 64 i32.const N i32.store8 ... )
        // )
        // The payload is written byte by byte into memory at offset 64.
        let mut body = String::new();
        body.push_str(&format!("(module (memory (export \"memory\") {min_pages})"));
        body.push_str("(func (export \"openether_alloc\") (param i32) (result i32) local.get 0)");
        body.push_str("(func (export \"openether_dealloc\") (param i32 i32))");
        body.push_str("(func (export \"openether_on_tick\") (param i32 i32) (result i32)");
        let bytes = payload.as_bytes();
        for (i, b) in bytes.iter().enumerate() {
            body.push_str(&format!(
                " i32.const {} i32.const {} i32.store8 ",
                64 + i,
                b
            ));
        }
        body.push_str(&format!(
            " i32.const {} i32.const 0 i32.store8 ",
            64 + bytes.len()
        ));
        body.push_str(" i32.const 64 )");
        body.push(')');
        wat::parse_str(&body).expect("valid wat")
    }

    fn temp_wasm(payload: &str, min_pages: u32) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("openether-strategy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("strategy.wasm");
        std::fs::write(&path, wasm_returning(payload, min_pages)).expect("write wasm");
        path
    }

    /// A guest that burns fuel in a tight loop, used to prove the fuel cap works.
    fn wasm_burning() -> Vec<u8> {
        let wat = r#"
            (module
                (memory (export "memory") 1)
                (func (export "openether_alloc") (param i32) (result i32) local.get 0)
                (func (export "openether_dealloc") (param i32 i32))
                (func (export "openether_on_tick") (param i32 i32) (result i32)
                    (local $i i32)
                    (loop $l
                        local.get $i
                        i32.const 1
                        i32.add
                        local.set $i
                        local.get $i
                        i32.const 1000000
                        i32.lt_s
                        br_if $l
                    )
                    i32.const 64))
        "#;
        wat::parse_str(wat).expect("valid wat")
    }

    fn temp_burning_wasm() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("openether-strategy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("burner.wasm");
        std::fs::write(&path, wasm_burning()).expect("write wasm");
        path
    }

    #[test]
    fn strategy_round_trip() {
        let path = temp_wasm(
            r#"{"signals":[{"symbol":"SOL/USDT","side":"buy","qty":1.5,"confidence":0.8,"reason":"momentum"}]}"#,
            1,
        );
        let metrics = Arc::new(Metrics::new());
        let host = StrategyHost::new(&path, 5_000_000, 16, metrics).expect("load strategy");
        let signals = host
            .on_tick(&serde_json::json!({"price": 142.0}))
            .expect("tick");
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].symbol, "SOL/USDT");
        assert_eq!(signals[0].side, Side::Buy);
        assert_eq!(signals[0].qty, 1.5);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn invalid_signals_are_dropped() {
        let path = temp_wasm(
            r#"{"signals":[{"symbol":"SOL/USDT","side":"buy","qty":-1},{"symbol":"SOL/USDT","side":"hold","qty":1}]}"#,
            1,
        );
        let metrics = Arc::new(Metrics::new());
        let host = StrategyHost::new(&path, 5_000_000, 16, metrics).expect("load strategy");
        let signals = host.on_tick(&serde_json::json!({})).expect("tick");
        assert!(signals.is_empty());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// A Rust `wasm32-unknown-unknown` guest declares a 17-page memory minimum
    /// (1 MiB stack plus data), so a 16-page cap cannot load the shipped
    /// strategy at all. The default cap must stay above that minimum.
    #[test]
    fn memory_cap_admits_a_real_rust_guest() {
        let payload = r#"{"signals":[]}"#;
        let path = temp_wasm(payload, 17);
        let metrics = Arc::new(Metrics::new());

        let too_small = StrategyHost::new(&path, 5_000_000, 16, Arc::clone(&metrics));
        assert!(
            too_small.is_err(),
            "a 16-page cap must refuse a guest that needs 17 pages"
        );

        let host = StrategyHost::new(&path, 5_000_000, 32, metrics).expect("load strategy");
        assert!(host
            .on_tick(&serde_json::json!({}))
            .expect("tick")
            .is_empty());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn fuel_limit_stops_a_runaway_guest() {
        let path = temp_burning_wasm();
        let metrics = Arc::new(Metrics::new());
        let host = StrategyHost::new(&path, 10_000, 16, metrics).expect("load strategy");
        let result = host.on_tick(&serde_json::json!({"price": 142.0}));
        assert!(result.is_err(), "expected the fuel limit to trap the guest");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}

//! Durable state for the trading core.
//!
//! The engine used to keep everything in memory, which meant a restart reset
//! equity to zero and silently re-opened every risk limit. That is fine for a
//! demo and dangerous for real money, so the pieces that the risk layer reads
//! are written to SQLite: the portfolio scalars, the open positions, the trade
//! journal, the open orders and the risk state itself (including whether the
//! breaker is open — a tripped breaker must survive a restart, otherwise
//! restarting the process would be a way to bypass it).
//!
//! Ticks and log lines are deliberately *not* persisted: they are high-volume
//! and reconstructible, and they are not inputs to any risk decision.

use crate::exchange::OpenOrder;
use crate::risk::{BreakerState, RiskState};
use crate::types::{Side, TradeRecord, Venue};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;

/// Scalars that define the account the risk layer measures against.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PortfolioRow {
    pub equity_usd: f64,
    pub cash_usd: f64,
    pub realized_pnl: f64,
}

impl Default for PortfolioRow {
    fn default() -> Self {
        Self {
            equity_usd: 0.0,
            cash_usd: 0.0,
            realized_pnl: 0.0,
        }
    }
}

/// The risk engine's mutable state, as persisted.
///
/// Re-exported from [`crate::risk`] so the engine and the store agree on one
/// shape.
pub use crate::risk::RiskRow;

/// One open position: signed quantity and average entry price.
#[derive(Debug, Clone, PartialEq)]
pub struct PositionRow {
    pub symbol: String,
    pub qty: f64,
    pub avg_entry: f64,
}

/// Durable store for the engine's risk-relevant state.
///
/// The connection is guarded by a mutex and every statement is short, so the
/// lock is held for microseconds. Writes happen once per fill, not per tick.
pub struct Store {
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

/// The schema. Every statement is idempotent so it can run on every boot.
const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS portfolio (
    id            INTEGER PRIMARY KEY CHECK (id = 1),
    equity_usd    REAL    NOT NULL,
    cash_usd      REAL    NOT NULL,
    realized_pnl  REAL    NOT NULL
);

CREATE TABLE IF NOT EXISTS positions (
    symbol     TEXT PRIMARY KEY,
    qty        REAL NOT NULL,
    avg_entry  REAL NOT NULL
);

CREATE TABLE IF NOT EXISTS trades (
    id           TEXT PRIMARY KEY,
    venue        TEXT NOT NULL,
    symbol       TEXT NOT NULL,
    side         TEXT NOT NULL,
    qty          REAL NOT NULL,
    price        REAL NOT NULL,
    fee          REAL NOT NULL,
    realized_pnl REAL NOT NULL,
    ts_ms        INTEGER NOT NULL,
    rationale    TEXT NOT NULL,
    source       TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS trades_ts_idx ON trades (ts_ms DESC);

CREATE TABLE IF NOT EXISTS open_orders (
    venue    TEXT NOT NULL,
    order_id TEXT NOT NULL,
    symbol   TEXT NOT NULL,
    side     TEXT NOT NULL,
    qty      REAL NOT NULL,
    price    REAL NOT NULL,
    ts_ms    INTEGER NOT NULL,
    PRIMARY KEY (venue, order_id)
);

CREATE TABLE IF NOT EXISTS order_clients (
    client_id TEXT PRIMARY KEY,
    venue     TEXT NOT NULL,
    order_id  TEXT NOT NULL,
    symbol    TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS risk_state (
    id                 INTEGER PRIMARY KEY CHECK (id = 1),
    breaker            TEXT    NOT NULL,
    breaker_reason     TEXT,
    breaker_opened_at_ms INTEGER NOT NULL,
    peak_equity        REAL    NOT NULL,
    day_start_equity   REAL    NOT NULL,
    day_start_ms       INTEGER NOT NULL,
    daily_realized_pnl REAL    NOT NULL
);
"#;

impl Store {
    /// Open a store at `path`, creating the parent directory and the schema.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// Open a store that lives only for the lifetime of the process.
    pub fn open_in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> anyhow::Result<Self> {
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> anyhow::Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| anyhow::anyhow!("store mutex poisoned"))
    }

    /* ------------------------------------------------------------ portfolio */

    /// Write the account scalars.
    pub fn save_portfolio(&self, row: PortfolioRow) -> anyhow::Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO portfolio (id, equity_usd, cash_usd, realized_pnl)
             VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET
                equity_usd = excluded.equity_usd,
                cash_usd = excluded.cash_usd,
                realized_pnl = excluded.realized_pnl",
            params![row.equity_usd, row.cash_usd, row.realized_pnl],
        )?;
        Ok(())
    }

    /// Read the account scalars, or `None` when nothing was ever written.
    pub fn load_portfolio(&self) -> anyhow::Result<Option<PortfolioRow>> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT equity_usd, cash_usd, realized_pnl FROM portfolio WHERE id = 1",
                [],
                |r| {
                    Ok(PortfolioRow {
                        equity_usd: r.get(0)?,
                        cash_usd: r.get(1)?,
                        realized_pnl: r.get(2)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /* ------------------------------------------------------------ positions */

    /// Replace the position table with `positions`.
    ///
    /// The whole set is written in one transaction so a crash can never leave
    /// half the book on disk.
    pub fn save_positions(&self, positions: &[PositionRow]) -> anyhow::Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM positions", [])?;
        for p in positions {
            tx.execute(
                "INSERT INTO positions (symbol, qty, avg_entry) VALUES (?1, ?2, ?3)",
                params![p.symbol, p.qty, p.avg_entry],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Read every open position.
    pub fn load_positions(&self) -> anyhow::Result<Vec<PositionRow>> {
        let conn = self.lock()?;
        let mut stmt =
            conn.prepare("SELECT symbol, qty, avg_entry FROM positions ORDER BY symbol")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(PositionRow {
                    symbol: r.get(0)?,
                    qty: r.get(1)?,
                    avg_entry: r.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /* ---------------------------------------------------------------- trades */

    /// Append a fill to the journal.
    pub fn record_trade(&self, trade: &TradeRecord) -> anyhow::Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT OR REPLACE INTO trades
                (id, venue, symbol, side, qty, price, fee, realized_pnl, ts_ms, rationale, source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                trade.id,
                trade.venue.as_str(),
                trade.symbol,
                trade.side.as_str(),
                trade.qty,
                trade.price,
                trade.fee,
                trade.realized_pnl,
                trade.ts_ms,
                trade.rationale,
                trade.source,
            ],
        )?;
        Ok(())
    }

    /// Read the most recent fills, newest first.
    pub fn trades(&self, limit: usize) -> anyhow::Result<Vec<TradeRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, venue, symbol, side, qty, price, fee, realized_pnl, ts_ms, rationale, source
             FROM trades ORDER BY ts_ms DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], |r| {
                let venue: String = r.get(1)?;
                let side: String = r.get(3)?;
                Ok(TradeRecord {
                    id: r.get(0)?,
                    venue: Venue::parse(&venue).unwrap_or(Venue::Paper),
                    symbol: r.get(2)?,
                    side: Side::parse(&side).unwrap_or(Side::Buy),
                    qty: r.get(4)?,
                    price: r.get(5)?,
                    fee: r.get(6)?,
                    realized_pnl: r.get(7)?,
                    ts_ms: r.get(8)?,
                    rationale: r.get(9)?,
                    source: r.get(10)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Number of fills in the journal.
    pub fn trade_count(&self) -> anyhow::Result<i64> {
        let conn = self.lock()?;
        Ok(conn.query_row("SELECT COUNT(*) FROM trades", [], |r| r.get(0))?)
    }

    /* ----------------------------------------------------------- open orders */

    /// Replace the open-order table with `orders`.
    pub fn save_open_orders(&self, orders: &[OpenOrder]) -> anyhow::Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM open_orders", [])?;
        for o in orders {
            tx.execute(
                "INSERT OR REPLACE INTO open_orders (venue, order_id, symbol, side, qty, price, ts_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    o.venue.as_str(),
                    o.order_id,
                    o.symbol,
                    o.side.as_str(),
                    o.qty,
                    o.price,
                    o.ts_ms,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Read every open order.
    pub fn load_open_orders(&self) -> anyhow::Result<Vec<OpenOrder>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT venue, order_id, symbol, side, qty, price, ts_ms
             FROM open_orders ORDER BY ts_ms ASC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let venue: String = r.get(0)?;
                let side: String = r.get(3)?;
                Ok(OpenOrder {
                    venue: Venue::parse(&venue).unwrap_or(Venue::Paper),
                    order_id: r.get(1)?,
                    symbol: r.get(2)?,
                    side: Side::parse(&side).unwrap_or(Side::Buy),
                    qty: r.get(4)?,
                    price: r.get(5)?,
                    ts_ms: r.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Remember which venue order a client order id refers to.
    ///
    /// The connectors keep this mapping in memory, so it is lost on restart
    /// and cancelling an order stops working. Persisting it makes the
    /// client id the stable handle across restarts.
    pub fn remember_order_client(
        &self,
        client_id: &str,
        venue: Venue,
        order_id: &str,
        symbol: &str,
    ) -> anyhow::Result<()> {
        if client_id.is_empty() || order_id.is_empty() {
            return Ok(());
        }
        let conn = self.lock()?;
        conn.execute(
            "INSERT OR REPLACE INTO order_clients (client_id, venue, order_id, symbol)
             VALUES (?1, ?2, ?3, ?4)",
            params![client_id, venue.as_str(), order_id, symbol],
        )?;
        Ok(())
    }

    /// Look up the venue order id for a client order id.
    pub fn order_id_for_client(&self, client_id: &str) -> anyhow::Result<Option<String>> {
        let conn = self.lock()?;
        let order_id = conn
            .query_row(
                "SELECT order_id FROM order_clients WHERE client_id = ?1",
                params![client_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(order_id)
    }

    /* ----------------------------------------------------------- risk state */

    /// Write the risk engine's mutable state.
    pub fn save_risk_state(&self, row: &RiskRow) -> anyhow::Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO risk_state
                (id, breaker, breaker_reason, breaker_opened_at_ms, peak_equity, day_start_equity, day_start_ms, daily_realized_pnl)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                breaker = excluded.breaker,
                breaker_reason = excluded.breaker_reason,
                breaker_opened_at_ms = excluded.breaker_opened_at_ms,
                peak_equity = excluded.peak_equity,
                day_start_equity = excluded.day_start_equity,
                day_start_ms = excluded.day_start_ms,
                daily_realized_pnl = excluded.daily_realized_pnl",
            params![
                row.breaker.as_str(),
                row.breaker_reason,
                row.breaker_opened_at_ms,
                row.peak_equity,
                row.day_start_equity,
                row.day_start_ms,
                row.daily_realized_pnl,
            ],
        )?;
        Ok(())
    }

    /// Read the risk engine's mutable state.
    pub fn load_risk_state(&self) -> anyhow::Result<Option<RiskRow>> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT breaker, breaker_reason, breaker_opened_at_ms, peak_equity, day_start_equity, day_start_ms, daily_realized_pnl
                 FROM risk_state WHERE id = 1",
                [],
                |r| {
                    let breaker: String = r.get(0)?;
                    Ok(RiskRow {
                        breaker: BreakerState::parse(&breaker).unwrap_or(BreakerState::Closed),
                        breaker_reason: r.get(1)?,
                        breaker_opened_at_ms: r.get(2)?,
                        peak_equity: r.get(3)?,
                        day_start_equity: r.get(4)?,
                        day_start_ms: r.get(5)?,
                        daily_realized_pnl: r.get(6)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Persist the risk state described by a live [`RiskState`] snapshot.
    pub fn save_risk_snapshot(&self, state: &RiskState) -> anyhow::Result<()> {
        self.save_risk_state(&RiskRow {
            breaker: state.breaker_state,
            breaker_reason: state.breaker_reason.clone(),
            breaker_opened_at_ms: 0,
            peak_equity: state.peak_equity_usd,
            day_start_equity: 0.0,
            day_start_ms: 0,
            daily_realized_pnl: state.daily_pnl_usd,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::risk::BreakerState;

    fn store() -> Store {
        Store::open_in_memory().expect("open store")
    }

    fn trade(id: &str, ts_ms: i64) -> TradeRecord {
        TradeRecord {
            id: id.to_string(),
            venue: Venue::Paper,
            symbol: "SOL/USDT".into(),
            side: Side::Buy,
            qty: 1.5,
            price: 142.0,
            fee: 0.02,
            realized_pnl: 0.0,
            ts_ms,
            rationale: "test".into(),
            source: "engine".into(),
        }
    }

    #[test]
    fn portfolio_round_trips() {
        let s = store();
        assert!(s.load_portfolio().unwrap().is_none());
        let row = PortfolioRow {
            equity_usd: 10_000.0,
            cash_usd: 9_999.0,
            realized_pnl: 12.5,
        };
        s.save_portfolio(row).unwrap();
        assert_eq!(s.load_portfolio().unwrap(), Some(row));
    }

    #[test]
    fn portfolio_saves_are_upserts_not_appends() {
        let s = store();
        s.save_portfolio(PortfolioRow {
            equity_usd: 1.0,
            cash_usd: 1.0,
            realized_pnl: 0.0,
        })
        .unwrap();
        s.save_portfolio(PortfolioRow {
            equity_usd: 2.0,
            cash_usd: 2.0,
            realized_pnl: 0.0,
        })
        .unwrap();
        let row = s.load_portfolio().unwrap().unwrap();
        assert_eq!(row.equity_usd, 2.0);
    }

    #[test]
    fn positions_round_trip_and_replace() {
        let s = store();
        s.save_positions(&[
            PositionRow {
                symbol: "SOL/USDT".into(),
                qty: 2.0,
                avg_entry: 140.0,
            },
            PositionRow {
                symbol: "BTC/USDT".into(),
                qty: -0.5,
                avg_entry: 60_000.0,
            },
        ])
        .unwrap();
        let loaded = s.load_positions().unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].symbol, "BTC/USDT");
        assert_eq!(loaded[0].qty, -0.5);

        // A closed position must disappear, not linger with a stale quantity.
        s.save_positions(&[PositionRow {
            symbol: "SOL/USDT".into(),
            qty: 2.0,
            avg_entry: 140.0,
        }])
        .unwrap();
        let loaded = s.load_positions().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].symbol, "SOL/USDT");
    }

    #[test]
    fn trades_are_journalled_newest_first() {
        let s = store();
        s.record_trade(&trade("a", 1_000)).unwrap();
        s.record_trade(&trade("b", 2_000)).unwrap();
        s.record_trade(&trade("c", 3_000)).unwrap();
        assert_eq!(s.trade_count().unwrap(), 3);
        let loaded = s.trades(10).unwrap();
        assert_eq!(loaded[0].id, "c");
        assert_eq!(loaded[2].id, "a");
        assert_eq!(loaded[0].venue, Venue::Paper);
        assert_eq!(loaded[0].side, Side::Buy);
        assert!((loaded[0].price - 142.0).abs() < 1e-12);
    }

    #[test]
    fn trade_limit_applies() {
        let s = store();
        for i in 0..5 {
            s.record_trade(&trade(&format!("t{i}"), i as i64)).unwrap();
        }
        assert_eq!(s.trades(2).unwrap().len(), 2);
    }

    #[test]
    fn open_orders_round_trip() {
        let s = store();
        let order = OpenOrder {
            venue: Venue::Binance,
            order_id: "42".into(),
            symbol: "ETH/USDT".into(),
            side: Side::Sell,
            qty: 1.0,
            price: 3_000.0,
            ts_ms: 5_000,
        };
        s.save_open_orders(std::slice::from_ref(&order)).unwrap();
        let loaded = s.load_open_orders().unwrap();
        assert_eq!(loaded, vec![order]);
    }

    #[test]
    fn client_order_ids_survive_a_restart() {
        let dir = std::env::temp_dir().join(format!("oe-store-{}", uuid::Uuid::new_v4()));
        let path = dir.join("state.db");
        {
            let s = Store::open(&path).unwrap();
            s.remember_order_client("oe-abc", Venue::Binance, "999", "ETH/USDT")
                .unwrap();
        }
        // A fresh handle on the same file is what a restarted process gets.
        let s = Store::open(&path).unwrap();
        assert_eq!(
            s.order_id_for_client("oe-abc").unwrap().as_deref(),
            Some("999")
        );
        assert_eq!(s.order_id_for_client("oe-missing").unwrap(), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn risk_state_round_trips_including_an_open_breaker() {
        let s = store();
        let row = RiskRow {
            breaker: BreakerState::Open,
            breaker_reason: Some("position_size_exceeds_max_pct".into()),
            breaker_opened_at_ms: 1_700_000_000_000,
            peak_equity: 10_500.0,
            day_start_equity: 10_000.0,
            day_start_ms: 1_700_000_000_000,
            daily_realized_pnl: -120.0,
        };
        s.save_risk_state(&row).unwrap();
        assert_eq!(s.load_risk_state().unwrap(), Some(row));
    }

    #[test]
    fn opening_a_store_creates_the_parent_directory() {
        let dir = std::env::temp_dir().join(format!("oe-store-{}", uuid::Uuid::new_v4()));
        let path = dir.join("nested").join("state.db");
        let s = Store::open(&path).expect("open");
        s.save_portfolio(PortfolioRow::default()).unwrap();
        assert!(path.exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}

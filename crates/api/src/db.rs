//! SQLite persistence layer for trade history

use rusqlite::{Connection, params};
use std::sync::Mutex;
use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryPosition {
    pub id: String,
    pub pipeline: String,
    pub asset: String,
    pub timeframe: String,
    pub direction: String,
    pub entry_price: f64,
    pub exit_price: f64,
    pub size: f64,
    pub pnl: f64,
    pub opened_at: String,
    pub closed_at: String,
    pub exit_reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistorySignal {
    pub id: String,
    pub pipeline: String,
    pub strategy: String,
    pub asset: String,
    pub direction: String,
    pub edge: f64,
    pub confidence: f64,
    pub action: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    pub id: i64,
    pub pipeline: String,
    pub total_pnl: f64,
    pub win_rate: f64,
    pub sharpe: f64,
    pub trades: i64,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundHistory {
    pub id: i64,
    pub asset: String,
    pub timeframe: String,
    pub reference_price: f64,
    pub close_price: f64,
    pub our_p_up: f64,
    pub market_p_up: f64,
    pub edge: f64,
    pub resolved_direction: String,
    pub round_start: String,
    pub round_end: String,
}

pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    pub fn new(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("
            CREATE TABLE IF NOT EXISTS positions (
                id TEXT PRIMARY KEY,
                pipeline TEXT NOT NULL,
                asset TEXT NOT NULL,
                timeframe TEXT NOT NULL DEFAULT '',
                direction TEXT NOT NULL,
                entry_price REAL NOT NULL,
                exit_price REAL NOT NULL,
                size REAL NOT NULL,
                pnl REAL NOT NULL,
                opened_at TEXT NOT NULL,
                closed_at TEXT NOT NULL,
                exit_reason TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE IF NOT EXISTS signals (
                id TEXT PRIMARY KEY,
                pipeline TEXT NOT NULL,
                strategy TEXT NOT NULL DEFAULT '',
                asset TEXT NOT NULL,
                direction TEXT NOT NULL DEFAULT '',
                edge REAL NOT NULL,
                confidence REAL NOT NULL DEFAULT 0,
                action TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS metrics_snapshots (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                pipeline TEXT NOT NULL,
                total_pnl REAL NOT NULL,
                win_rate REAL NOT NULL,
                sharpe REAL NOT NULL,
                trades INTEGER NOT NULL,
                timestamp TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS crypto_rounds_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                asset TEXT NOT NULL,
                timeframe TEXT NOT NULL,
                reference_price REAL NOT NULL,
                close_price REAL NOT NULL,
                our_p_up REAL NOT NULL DEFAULT 0,
                market_p_up REAL NOT NULL DEFAULT 0,
                edge REAL NOT NULL DEFAULT 0,
                resolved_direction TEXT NOT NULL DEFAULT '',
                round_start TEXT NOT NULL,
                round_end TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS open_positions (
                id TEXT PRIMARY KEY,
                pipeline TEXT NOT NULL,
                market_id TEXT NOT NULL,
                question TEXT NOT NULL DEFAULT '',
                direction TEXT NOT NULL,
                entry_price REAL NOT NULL,
                size REAL NOT NULL,
                entry_edge REAL NOT NULL DEFAULT 0,
                opened_at TEXT NOT NULL,
                is_live INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS bot_state (
                key TEXT PRIMARY KEY,
                value REAL NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_positions_pipeline ON positions(pipeline);
            CREATE INDEX IF NOT EXISTS idx_signals_pipeline ON signals(pipeline);
            CREATE INDEX IF NOT EXISTS idx_metrics_pipeline ON metrics_snapshots(pipeline);
            CREATE INDEX IF NOT EXISTS idx_rounds_asset ON crypto_rounds_history(asset, timeframe);
        ")?;

        // Migration: add columns if they don't exist (for existing databases)
        let _ = conn.execute("ALTER TABLE open_positions ADD COLUMN is_live INTEGER NOT NULL DEFAULT 0", []);
        let _ = conn.execute("ALTER TABLE open_positions ADD COLUMN fee_rate_bps REAL NOT NULL DEFAULT 0", []);

        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn insert_position(&self, pos: &HistoryPosition) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO positions (id, pipeline, asset, timeframe, direction, entry_price, exit_price, size, pnl, opened_at, closed_at, exit_reason) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![pos.id, pos.pipeline, pos.asset, pos.timeframe, pos.direction, pos.entry_price, pos.exit_price, pos.size, pos.pnl, pos.opened_at, pos.closed_at, pos.exit_reason],
        )?;
        Ok(())
    }

    pub fn insert_signal(&self, sig: &HistorySignal) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO signals (id, pipeline, strategy, asset, direction, edge, confidence, action, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![sig.id, sig.pipeline, sig.strategy, sig.asset, sig.direction, sig.edge, sig.confidence, sig.action, sig.created_at],
        )?;
        Ok(())
    }

    pub fn insert_metrics_snapshot(&self, pipeline: &str, total_pnl: f64, win_rate: f64, sharpe: f64, trades: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO metrics_snapshots (pipeline, total_pnl, win_rate, sharpe, trades, timestamp) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![pipeline, total_pnl, win_rate, sharpe, trades, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn insert_round_history(&self, round: &RoundHistory) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO crypto_rounds_history (asset, timeframe, reference_price, close_price, our_p_up, market_p_up, edge, resolved_direction, round_start, round_end) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![round.asset, round.timeframe, round.reference_price, round.close_price, round.our_p_up, round.market_p_up, round.edge, round.resolved_direction, round.round_start, round.round_end],
        )?;
        Ok(())
    }

    pub fn query_positions(&self, pipeline: &str, limit: i64, offset: i64) -> Result<Vec<HistoryPosition>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, pipeline, asset, timeframe, direction, entry_price, exit_price, size, pnl, opened_at, closed_at, exit_reason FROM positions WHERE pipeline = ?1 ORDER BY closed_at DESC LIMIT ?2 OFFSET ?3"
        )?;
        let rows = stmt.query_map(params![pipeline, limit, offset], |row| {
            Ok(HistoryPosition {
                id: row.get(0)?,
                pipeline: row.get(1)?,
                asset: row.get(2)?,
                timeframe: row.get(3)?,
                direction: row.get(4)?,
                entry_price: row.get(5)?,
                exit_price: row.get(6)?,
                size: row.get(7)?,
                pnl: row.get(8)?,
                opened_at: row.get(9)?,
                closed_at: row.get(10)?,
                exit_reason: row.get(11)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn query_signals(&self, pipeline: &str, limit: i64) -> Result<Vec<HistorySignal>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, pipeline, strategy, asset, direction, edge, confidence, action, created_at FROM signals WHERE pipeline = ?1 ORDER BY created_at DESC LIMIT ?2"
        )?;
        let rows = stmt.query_map(params![pipeline, limit], |row| {
            Ok(HistorySignal {
                id: row.get(0)?,
                pipeline: row.get(1)?,
                strategy: row.get(2)?,
                asset: row.get(3)?,
                direction: row.get(4)?,
                edge: row.get(5)?,
                confidence: row.get(6)?,
                action: row.get(7)?,
                created_at: row.get(8)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn query_round_history(&self, asset: &str, timeframe: &str, limit: i64) -> Result<Vec<RoundHistory>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, asset, timeframe, reference_price, close_price, our_p_up, market_p_up, edge, resolved_direction, round_start, round_end FROM crypto_rounds_history WHERE asset = ?1 AND timeframe = ?2 ORDER BY round_end DESC LIMIT ?3"
        )?;
        let rows = stmt.query_map(params![asset, timeframe, limit], |row| {
            Ok(RoundHistory {
                id: row.get(0)?,
                asset: row.get(1)?,
                timeframe: row.get(2)?,
                reference_price: row.get(3)?,
                close_price: row.get(4)?,
                our_p_up: row.get(5)?,
                market_p_up: row.get(6)?,
                edge: row.get(7)?,
                resolved_direction: row.get(8)?,
                round_start: row.get(9)?,
                round_end: row.get(10)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    // === Open Position Persistence (crash recovery) ===

    pub fn save_open_position(&self, pipeline: &str, pos: &crate::state::Position) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO open_positions (id, pipeline, market_id, question, direction, entry_price, size, entry_edge, opened_at, is_live, fee_rate_bps) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![pos.id, pipeline, pos.market_id, pos.question, pos.direction, pos.entry_price, pos.size, pos.entry_edge, pos.opened_at.to_rfc3339(), pos.is_live as i64, pos.fee_rate_bps],
        )?;
        Ok(())
    }

    pub fn delete_open_position(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM open_positions WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn load_open_positions(&self, pipeline: &str) -> Result<Vec<crate::state::Position>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, market_id, question, direction, entry_price, size, entry_edge, opened_at, is_live, fee_rate_bps FROM open_positions WHERE pipeline = ?1"
        )?;
        let rows = stmt.query_map(params![pipeline], |row| {
            let opened_str: String = row.get(7)?;
            let opened_at = chrono::DateTime::parse_from_rfc3339(&opened_str)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::Utc::now());
            Ok(crate::state::Position {
                id: row.get(0)?,
                market_id: row.get(1)?,
                question: row.get(2)?,
                direction: row.get(3)?,
                entry_price: row.get(4)?,
                current_price: row.get(4)?, // set to entry price, will update on next tick
                size: row.get(5)?,
                unrealized_pnl: 0.0,
                opened_at,
                entry_edge: row.get(6)?,
                is_live: row.get::<_, i64>(8).unwrap_or(0) != 0,
                fee_rate_bps: row.get(9).unwrap_or(0.0),
                hold_to_resolution: false,
                closed_at: None,
                exit_reason: None,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn save_equity(&self, pipeline: &str, equity: f64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let key = format!("{}_equity", pipeline);
        conn.execute(
            "INSERT OR REPLACE INTO bot_state (key, value, updated_at) VALUES (?1, ?2, ?3)",
            params![key, equity, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn load_equity(&self, pipeline: &str) -> Result<Option<f64>> {
        let conn = self.conn.lock().unwrap();
        let key = format!("{}_equity", pipeline);
        let result = conn.query_row(
            "SELECT value FROM bot_state WHERE key = ?1",
            params![key],
            |row| row.get(0),
        );
        match result {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Save the session start timestamp so we only count trades from this activation.
    pub fn save_session_start(&self, pipeline: &str, ts: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let key = format!("{}_session_start", pipeline);
        conn.execute(
            "INSERT OR REPLACE INTO bot_state (key, value, updated_at) VALUES (?1, 0, ?2)",
            params![key, ts],
        )?;
        Ok(())
    }

    /// Load session start timestamp; returns the `updated_at` field.
    pub fn load_session_start(&self, pipeline: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let key = format!("{}_session_start", pipeline);
        let result = conn.query_row(
            "SELECT updated_at FROM bot_state WHERE key = ?1",
            params![key],
            |row| row.get::<_, String>(0),
        );
        match result {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Sum PnL of all positions for a pipeline (efficient single-row query).
    pub fn sum_pnl(&self, pipeline: &str) -> Result<f64> {
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            "SELECT COALESCE(SUM(pnl), 0.0) FROM positions WHERE pipeline = ?1",
            params![pipeline],
            |row| row.get(0),
        )?;
        Ok(result)
    }

    /// Query positions closed on or after `since` timestamp (for current-session metrics).
    pub fn query_positions_since(&self, pipeline: &str, since: &str) -> Result<Vec<HistoryPosition>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, pipeline, asset, timeframe, direction, entry_price, exit_price, size, pnl, opened_at, closed_at, exit_reason FROM positions WHERE pipeline = ?1 AND closed_at >= ?2 ORDER BY closed_at ASC"
        )?;
        let rows = stmt.query_map(params![pipeline, since], |row| {
            Ok(HistoryPosition {
                id: row.get(0)?,
                pipeline: row.get(1)?,
                asset: row.get(2)?,
                timeframe: row.get(3)?,
                direction: row.get(4)?,
                entry_price: row.get(5)?,
                exit_price: row.get(6)?,
                size: row.get(7)?,
                pnl: row.get(8)?,
                opened_at: row.get(9)?,
                closed_at: row.get(10)?,
                exit_reason: row.get(11)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn query_metrics(&self, pipeline: &str, limit: i64) -> Result<Vec<MetricsSnapshot>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, pipeline, total_pnl, win_rate, sharpe, trades, timestamp FROM metrics_snapshots WHERE pipeline = ?1 ORDER BY timestamp DESC LIMIT ?2"
        )?;
        let rows = stmt.query_map(params![pipeline, limit], |row| {
            Ok(MetricsSnapshot {
                id: row.get(0)?,
                pipeline: row.get(1)?,
                total_pnl: row.get(2)?,
                win_rate: row.get(3)?,
                sharpe: row.get(4)?,
                trades: row.get(5)?,
                timestamp: row.get(6)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }
}

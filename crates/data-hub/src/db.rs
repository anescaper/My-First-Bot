use std::sync::Mutex;

use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};

use polybot_scanner::crypto::{Asset, Timeframe};
use polybot_scanner::price_feed::Candle;

use crate::round_tracker::ResolvedRound;

pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    pub fn new(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS candles (
                asset TEXT NOT NULL,
                interval TEXT NOT NULL,
                open_time INTEGER NOT NULL,
                close_time INTEGER NOT NULL,
                open REAL NOT NULL,
                high REAL NOT NULL,
                low REAL NOT NULL,
                close REAL NOT NULL,
                volume REAL NOT NULL,
                PRIMARY KEY (asset, interval, open_time)
            );

            CREATE TABLE IF NOT EXISTS resolved_rounds (
                condition_id TEXT PRIMARY KEY,
                asset TEXT NOT NULL,
                timeframe TEXT NOT NULL,
                reference_price REAL NOT NULL,
                close_price REAL NOT NULL,
                resolved_direction TEXT NOT NULL,
                resolved_at TEXT NOT NULL,
                round_start TEXT NOT NULL DEFAULT ''
            );

            CREATE TABLE IF NOT EXISTS reference_prices (
                asset TEXT NOT NULL,
                timeframe TEXT NOT NULL,
                price REAL NOT NULL,
                recorded_at TEXT NOT NULL,
                PRIMARY KEY (asset, timeframe)
            );

            CREATE INDEX IF NOT EXISTS idx_candles_asset_interval
                ON candles(asset, interval, open_time DESC);
            CREATE INDEX IF NOT EXISTS idx_resolved_at
                ON resolved_rounds(resolved_at DESC);
            ",
        )?;

        // Migration: add round_start column if missing (for existing DBs)
        let _ = conn.execute(
            "ALTER TABLE resolved_rounds ADD COLUMN round_start TEXT NOT NULL DEFAULT ''",
            [],
        ); // Ignore error if column already exists

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    // --- Candles ---

    pub fn upsert_candle(&self, asset: Asset, interval: &str, candle: &Candle) {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        let _ = conn.execute(
            "INSERT OR REPLACE INTO candles (asset, interval, open_time, close_time, open, high, low, close, volume)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                asset.slug_str(),
                interval,
                candle.open_time,
                candle.close_time,
                candle.open,
                candle.high,
                candle.low,
                candle.close,
                candle.volume,
            ],
        );
    }

    pub fn upsert_candles(&self, asset: Asset, interval: &str, candles: &[Candle]) {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        for candle in candles {
            let _ = conn.execute(
                "INSERT OR REPLACE INTO candles (asset, interval, open_time, close_time, open, high, low, close, volume)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    asset.slug_str(),
                    interval,
                    candle.open_time,
                    candle.close_time,
                    candle.open,
                    candle.high,
                    candle.low,
                    candle.close,
                    candle.volume,
                ],
            );
        }
    }

    pub fn load_candles(&self, asset: Asset, interval: &str, limit: usize) -> Vec<Candle> {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let mut stmt = match conn.prepare(
            "SELECT open_time, close_time, open, high, low, close, volume
             FROM candles WHERE asset = ?1 AND interval = ?2
             ORDER BY open_time DESC LIMIT ?3",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt
            .query_map(params![asset.slug_str(), interval, limit], |row| {
                Ok(Candle {
                    open_time: row.get(0)?,
                    close_time: row.get(1)?,
                    open: row.get(2)?,
                    high: row.get(3)?,
                    low: row.get(4)?,
                    close: row.get(5)?,
                    volume: row.get(6)?,
                })
            })
            .ok();
        let mut candles: Vec<Candle> = rows
            .map(|r| r.filter_map(|c| c.ok()).collect())
            .unwrap_or_default();
        candles.reverse(); // oldest first
        candles
    }

    pub fn latest_candle_time(&self, asset: Asset, interval: &str) -> Option<i64> {
        let conn = self.conn.lock().ok()?;
        conn.query_row(
            "SELECT MAX(close_time) FROM candles WHERE asset = ?1 AND interval = ?2",
            params![asset.slug_str(), interval],
            |row| row.get(0),
        )
        .ok()
    }

    pub fn prune_candles(&self, asset: Asset, interval: &str, keep: usize) {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        let _ = conn.execute(
            "DELETE FROM candles WHERE asset = ?1 AND interval = ?2
             AND open_time NOT IN (
                 SELECT open_time FROM candles WHERE asset = ?1 AND interval = ?2
                 ORDER BY open_time DESC LIMIT ?3
             )",
            params![asset.slug_str(), interval, keep],
        );
    }

    // --- Resolved rounds ---

    pub fn insert_resolved(&self, round: &ResolvedRound) {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        let _ = conn.execute(
            "INSERT OR REPLACE INTO resolved_rounds
             (condition_id, asset, timeframe, reference_price, close_price, resolved_direction, resolved_at, round_start)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                round.condition_id,
                format!("{:?}", round.asset),
                round.timeframe.slug(),
                round.reference_price,
                round.close_price,
                round.resolved_direction,
                round.resolved_at.to_rfc3339(),
                round.round_start.to_rfc3339(),
            ],
        );
    }

    pub fn load_resolved(&self, limit: usize) -> Vec<ResolvedRound> {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let mut stmt = match conn.prepare(
            "SELECT condition_id, asset, timeframe, reference_price, close_price, resolved_direction, resolved_at, round_start
             FROM resolved_rounds ORDER BY resolved_at DESC LIMIT ?1",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt
            .query_map(params![limit], |row| {
                let asset_str: String = row.get(1)?;
                let tf_str: String = row.get(2)?;
                let resolved_at_str: String = row.get(6)?;
                let round_start_str: String = row.get::<_, String>(7).unwrap_or_default();
                Ok((
                    row.get::<_, String>(0)?,
                    asset_str,
                    tf_str,
                    row.get::<_, f64>(3)?,
                    row.get::<_, f64>(4)?,
                    row.get::<_, String>(5)?,
                    resolved_at_str,
                    round_start_str,
                ))
            })
            .ok();

        let mut result = Vec::new();
        if let Some(rows) = rows {
            for row in rows.flatten() {
                let asset = match row.1.as_str() {
                    "BTC" => Asset::BTC,
                    "ETH" => Asset::ETH,
                    "SOL" => Asset::SOL,
                    "XRP" => Asset::XRP,
                    _ => continue,
                };
                let timeframe = match Timeframe::from_slug(&row.2) {
                    Some(tf) => tf,
                    None => continue,
                };
                let resolved_at = match row.6.parse::<DateTime<Utc>>() {
                    Ok(dt) => dt,
                    Err(_) => continue,
                };
                let round_start = row.7.parse::<DateTime<Utc>>()
                    .unwrap_or(resolved_at); // fallback for old rows without round_start
                result.push(ResolvedRound {
                    condition_id: row.0,
                    asset,
                    timeframe,
                    reference_price: row.3,
                    close_price: row.4,
                    resolved_direction: row.5,
                    resolved_at,
                    round_start,
                });
            }
        }
        result.reverse(); // oldest first
        result
    }

    // --- Reference prices ---

    pub fn upsert_reference(&self, asset: Asset, timeframe: Timeframe, price: f64) {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        let _ = conn.execute(
            "INSERT OR REPLACE INTO reference_prices (asset, timeframe, price, recorded_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                asset.slug_str(),
                timeframe.slug(),
                price,
                Utc::now().to_rfc3339(),
            ],
        );
    }

    pub fn load_references(&self) -> Vec<(Asset, Timeframe, f64)> {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let mut stmt = match conn.prepare(
            "SELECT asset, timeframe, price FROM reference_prices",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, f64>(2)?,
                ))
            })
            .ok();
        let mut result = Vec::new();
        if let Some(rows) = rows {
            for row in rows.flatten() {
                let asset = match row.0.as_str() {
                    "btc" | "BTC" => Asset::BTC,
                    "eth" | "ETH" => Asset::ETH,
                    "sol" | "SOL" => Asset::SOL,
                    "xrp" | "XRP" => Asset::XRP,
                    _ => continue,
                };
                let tf = match Timeframe::from_slug(&row.1) {
                    Some(t) => t,
                    None => continue,
                };
                result.push((asset, tf, row.2));
            }
        }
        result
    }
}

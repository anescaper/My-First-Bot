//! DataHub entry point -- the central market data service.
//!
//! The data-hub is a standalone binary that:
//! 1. Runs data adapters (Binance spot/futures WS, Polymarket CLOB WS, Deribit WS,
//!    Binance futures REST, Coinbase REST) to collect real-time market data.
//! 2. Builds 1m/5m/15m/1h candles from 5-second micro-candles.
//! 3. Tracks Polymarket prediction round lifecycles (active -> resolved).
//! 4. Records reference (strike) prices at timeframe boundaries.
//! 5. Serves all collected data via an HTTP API on the configured port.
//!
//! The bot binary (`polybot`) consumes the data-hub's `/api/cycle-data` endpoint
//! every cycle (~5s) to get all market data in a single request.
//!
//! # Environment variables
//! - `PORT`: HTTP server port (default: 4250)
//! - `DB_PATH`: SQLite database path for persistence (default: "data/data-hub.db")
//! - `CRYPTO_TIMEFRAMES`: Comma-separated timeframe list (default: "5m,15m,1h")
//! - `ENABLE_DERIBIT`: Set to "1" or "true" to enable the Deribit WS adapter
//!
//! # Architecture
//! The main function starts the following concurrent tasks:
//! - DataHub adapter tasks (managed by `polybot_data::DataHub`)
//! - Round scanner (every 5s): discovers active rounds, resolves expired ones
//! - Reference recorder (every 1s): captures spot prices at timeframe boundaries
//! - Candle builder (every 1s): aggregates micro-candles into 1m candles,
//!   then rolls 1m into 5m/15m/1h
//! - HTTP API server (axum): serves enriched data to bot pipelines

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Result;
use tower_http::cors::CorsLayer;
use tracing_subscriber::EnvFilter;

use polybot_data::state::TokenSide;
use polybot_data::{
    BinanceFuturesRestAdapter, BinanceFuturesWsAdapter, BinanceSpotWsAdapter,
    CoinbaseRestAdapter, DataHub, DeribitWsAdapter, PolymarketClobWsAdapter,
};
use polybot_scanner::crypto::{Asset, Timeframe};
use polybot_scanner::price_feed::{Candle, PriceFeedManager};

mod api;
mod candle_store;
mod db;
mod intel;
mod round_tracker;
mod types;

use candle_store::CandleStore;
use db::Database;
use round_tracker::RoundTracker;


#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    tracing::info!("data-hub v{}", env!("CARGO_PKG_VERSION"));

    // Default port 4250 chosen to avoid collisions with:
    // - bot API (4200), frontend (3007), liquidation bot (4300/4400)
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4250);

    let db_path = std::env::var("DB_PATH").unwrap_or_else(|_| "data/data-hub.db".to_string());
    if let Some(parent) = std::path::Path::new(&db_path).parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let db = match Database::new(&db_path) {
        Ok(db) => {
            tracing::info!("Database opened at {db_path}");
            Arc::new(db)
        }
        Err(e) => {
            tracing::error!("Failed to open database: {e}");
            return Err(e);
        }
    };

    let timeframes = std::env::var("CRYPTO_TIMEFRAMES")
        .map(|s| Timeframe::parse_list(&s))
        .unwrap_or_else(|_| Timeframe::DEFAULT.to_vec());

    tracing::info!("Timeframes: {:?}", timeframes);

    // --- Start DataHub (adapters) ---
    // The DataHub manages all data adapter lifecycles. Each adapter runs its own
    // tokio task for WebSocket connections or REST polling.
    let assets = Asset::ALL.to_vec();
    // poly_subs: maps Polymarket token_id -> (condition_id, TokenSide).
    // Shared with the CLOB WS adapter so it knows which tokens to subscribe to.
    // Updated by the scanner task whenever active rounds change.
    let poly_subs: Arc<RwLock<HashMap<String, (String, TokenSide)>>> =
        Arc::new(RwLock::new(HashMap::new()));

    let enable_deribit = std::env::var("ENABLE_DERIBIT")
        .map(|v| v == "1" || v.to_lowercase() == "true")
        .unwrap_or(false);

    let mut hub = DataHub::new()
        .add_tick(BinanceSpotWsAdapter::new(assets.clone()))
        .add_tick(BinanceFuturesWsAdapter::new(assets.clone()))
        .add_tick(PolymarketClobWsAdapter::new(poly_subs.clone()))
        .add_rest(BinanceFuturesRestAdapter::new(assets.clone()))
        .add_rest(CoinbaseRestAdapter::new(assets));

    if enable_deribit {
        tracing::info!("Deribit WS adapter enabled");
        hub = hub.add_tick(DeribitWsAdapter::new());
    }

    let data_state = hub.state();

    if let Err(e) = hub.start().await {
        tracing::error!("DataHub failed to start: {e}");
    }

    // Keep hub alive: the DataHub owns the adapter tasks and their shared state.
    // If dropped, the adapters would be cancelled. We move it into a background
    // task that never completes to keep it alive for the process lifetime.
    tokio::spawn(async move {
        let _hub = hub;
        std::future::pending::<()>().await;
    });

    tracing::info!("DataHub started");

    // --- Shared state ---
    let crypto_scanner =
        polybot_scanner::crypto::CryptoScanner::with_timeframes(timeframes.clone());
    let price_feed = Arc::new(PriceFeedManager::new());
    let candle_store = Arc::new(RwLock::new(CandleStore::new()));
    let round_tracker = Arc::new(RwLock::new(RoundTracker::new()));

    // --- Load persisted data from DB ---
    // On startup, we restore state from SQLite to minimize cold-start time.
    // The warm-start logic checks whether 1m candles are fresh enough (<2h old);
    // if so, it uses them directly (and rebuilds higher TFs via `backfill()`).
    // If stale, it falls back to loading higher-TF candles from DB and
    // backfilling 1m candles from Binance REST.
    {
        // Load resolved rounds
        let resolved = db.load_resolved(200);
        if !resolved.is_empty() {
            tracing::info!("Loaded {} resolved rounds from DB", resolved.len());
            if let Ok(mut tracker) = round_tracker.write() {
                tracker.load_resolved(resolved);
            }
        }

        // Load reference prices
        let refs = db.load_references();
        if !refs.is_empty() {
            tracing::info!("Loaded {} reference prices from DB", refs.len());
            if let Ok(mut tracker) = round_tracker.write() {
                for (asset, tf, price) in &refs {
                    tracker.record_reference(*asset, *tf, *price);
                }
            }
        }

        // Load persisted candles (1m) and check freshness
        let now_ms = chrono::Utc::now().timestamp_millis();
        let two_hours_ms = 2 * 3600 * 1000;
        let mut warm_assets = Vec::new();

        for asset in Asset::ALL {
            let candles = db.load_candles(asset, "1m", 120);
            if !candles.is_empty() {
                let latest = candles.last().unwrap().close_time;
                let age_ms = now_ms - latest;
                if age_ms < two_hours_ms {
                    // Fresh enough — backfill() loads 1m candles AND rebuilds 5m/15m/1h
                    let count = candles.len();
                    if let Ok(mut store) = candle_store.write() {
                        store.backfill(asset, candles);
                    }
                    tracing::info!(
                        "Warm start: loaded {count} 1m candles for {asset:?} from DB (age: {}s)",
                        age_ms / 1000
                    );
                    warm_assets.push(asset);
                    // Skip higher-TF DB load — backfill() already rebuilt them from 1m
                    continue;
                }
            }

            // Cold asset or stale 1m data: load higher TF candles directly from DB
            for (interval, limit) in [("5m", 60usize), ("15m", 40), ("1h", 24)] {
                let candles = db.load_candles(asset, interval, limit);
                if !candles.is_empty() {
                    let count = candles.len();
                    if let Ok(mut store) = candle_store.write() {
                        let deque = match interval {
                            "5m" => store.candles_5m.entry(asset).or_default(),
                            "15m" => store.candles_15m.entry(asset).or_default(),
                            "1h" => store.candles_1h.entry(asset).or_default(),
                            _ => continue,
                        };
                        for c in candles {
                            deque.push_back(c);
                        }
                    }
                    tracing::info!("Loaded {count} {interval} candles for {asset:?} from DB");
                }
            }
        }

        // Backfill from Binance REST only for assets that need it
        let cold_assets: Vec<Asset> = Asset::ALL
            .iter()
            .filter(|a| !warm_assets.contains(a))
            .copied()
            .collect();

        if cold_assets.is_empty() {
            tracing::info!("All assets warm — skipping Binance REST backfill");
        } else {
            tracing::info!(
                "Cold start for {:?} — backfilling from Binance REST",
                cold_assets
            );
            let pf = price_feed.clone();
            let cs = candle_store.clone();
            let backfill_db = db.clone();
            for asset in cold_assets {
                match pf.fetch_candles(asset, Timeframe::FiveMin, 60).await {
                    Ok(candles) => {
                        let count = candles.len();
                        // Persist to DB
                        backfill_db.upsert_candles(asset, "1m", &candles);
                        if let Ok(mut store) = cs.write() {
                            store.backfill(asset, candles);
                        }
                        tracing::info!("Backfilled {count} 1m candles for {asset:?}");
                    }
                    Err(e) => {
                        tracing::warn!("Failed to backfill {asset:?}: {e}");
                    }
                }
            }
        }
        tracing::info!("Startup data load complete");
    }

    // --- Background: Round scanner (every 5s) ---
    // Discovers active Polymarket crypto prediction rounds by scanning the API.
    // When a round disappears from the scan, it is resolved (expired) and the
    // outcome (Up/Down) is determined by comparing current spot to reference.
    // Also updates Polymarket CLOB WS subscriptions based on active token IDs.
    let scanner_rt = round_tracker.clone();
    let scanner_pf = price_feed.clone();
    let scanner_poly_subs = poly_subs.clone();
    let scanner_db = db.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        let mut prev_resolved_count = 0usize;
        loop {
            interval.tick().await;
            let rounds = crypto_scanner.scan_all().await;

            // Fetch current spot prices for resolution
            let prices = scanner_pf.fetch_all_prices().await;

            // Update poly_subs for CLOB WS adapter
            if let Ok(mut subs) = scanner_poly_subs.write() {
                subs.clear();
                for round in &rounds {
                    subs.insert(
                        round.token_id_up.clone(),
                        (round.condition_id.clone(), TokenSide::Up),
                    );
                    subs.insert(
                        round.token_id_down.clone(),
                        (round.condition_id.clone(), TokenSide::Down),
                    );
                }
            }

            // Update round tracker (detects expired rounds, resolves them)
            if let Ok(mut tracker) = scanner_rt.write() {
                tracker.update_rounds(&rounds, &prices);

                // Persist newly resolved rounds
                let all_resolved = tracker.get_resolved(None, None, 200);
                if all_resolved.len() > prev_resolved_count {
                    for r in all_resolved.iter().skip(prev_resolved_count) {
                        scanner_db.insert_resolved(r);
                    }
                    prev_resolved_count = all_resolved.len();
                }
            }

            tracing::debug!("Scanned {} active rounds", rounds.len());
        }
    });

    // --- Background: Reference recorder (every 1s) ---
    // Checks if any timeframe boundary has been crossed (e.g. a new 5-minute window
    // started) and records the spot price at that moment as the reference price.
    // Reference prices are persisted to DB every 30 seconds (not every tick) to
    // reduce write load while still surviving restarts with minimal data loss.
    let ref_pf = price_feed.clone();
    let ref_rt = round_tracker.clone();
    let ref_db = db.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        let mut tick_count = 0u64;
        loop {
            interval.tick().await;
            ref_pf.check_boundaries().await;

            // Sync reference prices into RoundTracker
            let refs = ref_pf.all_references().await;
            if let Ok(mut tracker) = ref_rt.write() {
                for r in &refs {
                    tracker.record_reference(r.asset, r.timeframe, r.price);
                }
            }

            // Persist reference prices every 30s (not every second)
            tick_count += 1;
            if tick_count % 30 == 0 {
                for r in &refs {
                    ref_db.upsert_reference(r.asset, r.timeframe, r.price);
                }
            }
        }
    });

    // --- Background: Candle builder (every 1s) ---
    // Accumulates 5-second micro-candles (from the Binance spot WS adapter) into
    // 1-minute OHLCV candles. The CandleStore then rolls 1m candles into 5m/15m/1h
    // automatically. Also persists completed candles to SQLite and prunes old ones
    // every 10 minutes.
    let cb_ds = data_state.clone();
    let cb_cs = candle_store.clone();
    let cb_db = db.clone();
    tokio::spawn(async move {
        // Track the last micro candle close_time we consumed per asset
        let mut last_consumed: HashMap<Asset, i64> = HashMap::new();
        let mut interval = tokio::time::interval(Duration::from_secs(1));

        // Accumulate micro candles into 1m candles
        let mut accum: HashMap<Asset, Vec<polybot_data::MicroCandle>> = HashMap::new();

        // Periodic DB prune counter
        let mut tick_count = 0u64;

        loop {
            interval.tick().await;
            tick_count += 1;

            let micro_snapshot: HashMap<Asset, Vec<polybot_data::MicroCandle>> = {
                let lock = match cb_ds.micro_candles.read() {
                    Ok(l) => l,
                    Err(_) => continue,
                };
                lock.iter()
                    .map(|(a, deque)| (*a, deque.iter().cloned().collect()))
                    .collect()
            };

            for asset in Asset::ALL {
                let micros = match micro_snapshot.get(&asset) {
                    Some(m) => m,
                    None => continue,
                };

                let last_ts = last_consumed.get(&asset).copied().unwrap_or(0);

                // Collect new micro candles since last_ts
                let new_micros: Vec<_> = micros
                    .iter()
                    .filter(|m| m.close_time.timestamp() > last_ts)
                    .collect();

                if new_micros.is_empty() {
                    continue;
                }

                // Update last consumed timestamp
                if let Some(latest) = new_micros.last() {
                    last_consumed.insert(asset, latest.close_time.timestamp());
                }

                let buf = accum.entry(asset).or_default();
                for mc in &new_micros {
                    buf.push((*mc).clone());
                }

                // Check if we have enough micro candles to form a 1m candle.
                // MicroCandles are 5s each, so 12 make a 1m candle.
                // We also check time alignment: accumulated span >= 55s (not 60s)
                // to allow for slight timing jitter in the micro-candle stream.
                // The 55s threshold is hardcoded because micro-candle intervals are
                // fixed at 5s by the Binance spot WS adapter.
                if buf.len() >= 12 {
                    let first_open = buf[0].open_time.timestamp();
                    let last_close = buf[buf.len() - 1].close_time.timestamp();
                    let span = last_close - first_open;

                    if span >= 55 {
                        // Build 1m candle from accumulated micro candles
                        let open = buf[0].open;
                        let high = buf.iter().fold(f64::NEG_INFINITY, |a, c| a.max(c.high));
                        let low = buf.iter().fold(f64::INFINITY, |a, c| a.min(c.low));
                        let close = buf[buf.len() - 1].close;
                        let volume: f64 = buf.iter().map(|c| c.volume).sum();

                        // Align open_time to the minute boundary (ms)
                        let open_time_ms = (first_open / 60) * 60 * 1000;
                        let close_time_ms = open_time_ms + 59_999;

                        let candle = Candle {
                            open,
                            high,
                            low,
                            close,
                            volume,
                            open_time: open_time_ms,
                            close_time: close_time_ms,
                        };

                        // Persist 1m candle to DB
                        cb_db.upsert_candle(asset, "1m", &candle);

                        if let Ok(mut store) = cb_cs.write() {
                            store.ingest_1m(asset, candle);

                            // Persist aggregated candles if they were just created
                            // Check the last entry in each higher TF deque
                            if let Some(c) = store.candles_5m.get(&asset).and_then(|d| d.back()) {
                                cb_db.upsert_candle(asset, "5m", c);
                            }
                            if let Some(c) = store.candles_15m.get(&asset).and_then(|d| d.back()) {
                                cb_db.upsert_candle(asset, "15m", c);
                            }
                            if let Some(c) = store.candles_1h.get(&asset).and_then(|d| d.back()) {
                                cb_db.upsert_candle(asset, "1h", c);
                            }
                        }

                        buf.clear();
                    }
                }
            }

            // Prune old candles from DB every 10 minutes (600 ticks at 1s interval).
            // Retention limits are hardcoded per interval to balance between
            // having enough history for strategies and keeping DB size bounded:
            // 1m: 120 candles (2h), 5m: 60 (5h), 15m: 40 (10h), 1h: 24 (1d)
            if tick_count % 600 == 0 {
                for asset in Asset::ALL {
                    cb_db.prune_candles(asset, "1m", 120);
                    cb_db.prune_candles(asset, "5m", 60);
                    cb_db.prune_candles(asset, "15m", 40);
                    cb_db.prune_candles(asset, "1h", 24);
                }
            }
        }
    });

    // --- HTTP API ---
    let api_state = api::ApiState {
        data_state,
        candle_store,
        round_tracker,
        started_at: chrono::Utc::now(),
    };

    let app = api::routes()
        .layer(CorsLayer::permissive())
        .with_state(api_state);

    let addr = format!("0.0.0.0:{port}");
    tracing::info!("HTTP API listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

//! Enriched HTTP API routes for the DataHub.
//!
//! Provides the `/cycle-data` endpoint (the primary data source for bot pipelines)
//! plus individual endpoints for health, rounds, candles, intel, and resolved data.
//! All routes are read-only GET handlers that assemble data from shared state
//! (DataState, CandleStore, RoundTracker).
//!
//! The API is mounted under `/api` in `main.rs` via `Router::nest`.

use std::sync::{Arc, RwLock};

use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::{Json, Router};
use axum::routing::get;
use chrono::Utc;
use serde::Deserialize;
use std::collections::HashMap;

use polybot_scanner::crypto::{Asset, Timeframe};
use polybot_scanner::price_feed::Candle;

use crate::candle_store::CandleStore;
use crate::round_tracker::RoundTracker;
use crate::types::*;

/// Shared state for the enriched API routes.
///
/// Mirrors the HubState in main.rs -- main.rs constructs this and passes it in.
/// All fields are `Arc`-wrapped so they can be cheaply cloned into each axum
/// handler (axum requires `Clone` on state).
///
/// # Fields
/// * `data_state` - Raw adapter data (prices, futures, order books, trade tapes)
///   from the `polybot_data` crate's DataHub. Updated by WebSocket/REST adapters.
/// * `candle_store` - Aggregated OHLCV candles at 1m/5m/15m/1h granularity.
///   Built from micro-candles by the candle builder background task.
/// * `round_tracker` - Active and resolved Polymarket prediction rounds.
///   Updated by the scanner background task every 5 seconds.
/// * `started_at` - Process start time, used for uptime calculation in `/health`.
#[derive(Clone)]
pub struct ApiState {
    pub data_state: Arc<polybot_data::DataState>,
    pub candle_store: Arc<RwLock<CandleStore>>,
    pub round_tracker: Arc<RwLock<RoundTracker>>,
    pub started_at: chrono::DateTime<Utc>,
}

/// Build the enriched API router. Mount under a prefix in main.rs:
/// ```ignore
/// let api = api::routes().with_state(api_state);
/// let app = Router::new().nest("/api", api);
/// ```
pub fn routes() -> Router<ApiState> {
    Router::new()
        .route("/health", get(health))
        .route("/cycle-data", get(cycle_data))
        .route("/rounds", get(api_rounds))
        .route("/candles/{asset}/{interval}", get(api_candles))
        .route("/intel/{asset}", get(intel))
        .route("/resolved", get(api_resolved))
}

/// GET /health -- returns adapter connectivity, round counts, and uptime.
///
/// Uses hardcoded staleness thresholds per adapter (see `build_adapter_status`).
/// This endpoint is hit by monitoring tools and the frontend health badge.
async fn health(State(state): State<ApiState>) -> Json<HealthResponse> {
    let active_rounds = state
        .round_tracker
        .read()
        .map(|rt| rt.active_rounds().len())
        .unwrap_or(0);

    let resolved_rounds = state
        .round_tracker
        .read()
        .map(|rt| rt.get_resolved(None, None, 999).len())
        .unwrap_or(0);

    let uptime = (Utc::now() - state.started_at).num_seconds().max(0) as u64;

    let adapters = build_adapter_status(&state.data_state);

    Json(HealthResponse {
        status: "ok".to_string(),
        adapters,
        active_rounds,
        resolved_rounds,
        uptime_secs: uptime,
    })
}

/// GET /cycle-data -- the primary endpoint consumed by bot pipelines each cycle.
///
/// Assembles all market data into a single fat JSON payload: active rounds,
/// spot prices, reference prices, candle histories, futures state, token prices,
/// order books, computed intel, options state, resolved rounds, trade tapes,
/// and Coinbase premiums. Each bot pipeline calls this once per cycle (~5s).
///
/// This "fat payload" design avoids N+1 request patterns -- one call gives the
/// pipeline everything it needs to run all strategies and risk checks.
async fn cycle_data(State(state): State<ApiState>) -> Json<CycleDataResponse> {
    let now = Utc::now();

    // 1. Active rounds
    let rounds = state
        .round_tracker
        .read()
        .map(|rt| rt.active_rounds())
        .unwrap_or_default();

    // 2. Latest prices from DataState
    let prices = read_latest_prices(&state.data_state);

    // 3. Reference prices from RoundTracker
    let reference_prices = build_reference_prices(&state.round_tracker);

    // 4. Candles from CandleStore for all assets and intervals
    let candles = build_all_candles(&state.candle_store);

    // 5. Futures state
    let futures = read_futures(&state.data_state);

    // 6. Token prices and order books
    let token_prices = read_token_prices(&state.data_state);
    let order_books = read_order_books(&state.data_state, &state.round_tracker);

    // 7. Compute DataDrivenIntel per asset
    let mut intel = HashMap::new();
    if let (Ok(cs), Ok(rt)) = (state.candle_store.read(), state.round_tracker.read()) {
        for asset in Asset::ALL {
            let key = asset.slug_str().to_uppercase();
            let price = prices.get(&key).copied().unwrap_or(0.0);
            if price > 0.0 {
                let i = crate::intel::compute_intel(asset, &cs, &rt, price);
                intel.insert(key, i);
            }
        }
    }

    // 8. Options state (Deribit DVOL)
    let options = read_options(&state.data_state);

    // 9. Resolved rounds
    let resolved_rounds = build_resolved_data(&state.round_tracker);

    // 10. Trade tapes (CLOB fill data)
    let trade_tapes = read_trade_tapes(&state.data_state);

    // 11. Coinbase premium
    let coinbase_premiums = read_coinbase_premiums(&state.data_state);

    Json(CycleDataResponse {
        rounds,
        prices,
        reference_prices,
        candles,
        futures,
        token_prices,
        order_books,
        intel,
        options,
        resolved_rounds,
        trade_tapes,
        coinbase_premiums,
        timestamp: now.to_rfc3339(),
    })
}

/// GET /rounds -- returns currently active Polymarket crypto prediction rounds.
///
/// Lighter than `/cycle-data` when only round metadata is needed (e.g. by
/// the frontend rounds table).
async fn api_rounds(State(state): State<ApiState>) -> impl IntoResponse {
    let rounds = state
        .round_tracker
        .read()
        .map(|rt| rt.active_rounds())
        .unwrap_or_default();
    Json(rounds)
}

/// GET /candles/{asset}/{interval} -- returns OHLCV candles for a specific
/// asset and interval (e.g. `/candles/BTC/5m`).
///
/// Returns an empty array if the asset is unrecognized or no candles are available.
/// Used by the frontend chart component for rendering candlestick series.
async fn api_candles(
    State(state): State<ApiState>,
    Path((asset_str, interval)): Path<(String, String)>,
) -> impl IntoResponse {
    let asset = match parse_asset(&asset_str) {
        Some(a) => a,
        None => return Json(Vec::<Candle>::new()),
    };
    let result = state
        .candle_store
        .read()
        .map(|cs| cs.get_candles(asset, &interval))
        .unwrap_or_default();
    Json(result)
}

/// GET /intel/{asset} -- returns computed data-driven intel for a single asset.
///
/// Calls `intel::compute_intel()` on the fly using current candle store and
/// round tracker state. Returns default (empty) intel if the asset is unrecognized
/// or the current price is zero (no data yet).
async fn intel(
    State(state): State<ApiState>,
    Path(asset_str): Path<String>,
) -> impl IntoResponse {
    let asset = match parse_asset(&asset_str) {
        Some(a) => a,
        None => return Json(DataDrivenIntel::default()),
    };

    let price = read_latest_prices(&state.data_state)
        .get(&asset_str.to_uppercase())
        .copied()
        .unwrap_or(0.0);

    if price <= 0.0 {
        return Json(DataDrivenIntel::default());
    }

    let result = match (state.candle_store.read(), state.round_tracker.read()) {
        (Ok(cs), Ok(rt)) => crate::intel::compute_intel(asset, &cs, &rt, price),
        _ => DataDrivenIntel::default(),
    };

    Json(result)
}

/// Query parameters for the `/resolved` endpoint.
/// All fields are optional -- omitting them returns all resolved rounds up to `limit`.
#[derive(Debug, Deserialize)]
struct ResolvedQuery {
    /// Filter by asset slug, e.g. "BTC" or "ETH".
    asset: Option<String>,
    /// Filter by timeframe slug, e.g. "5m" or "1h".
    tf: Option<String>,
    /// Maximum number of resolved rounds to return (default 50, hardcoded below).
    limit: Option<usize>,
}

/// GET /resolved -- returns recently settled prediction rounds.
///
/// Supports optional filtering by asset and timeframe via query parameters.
/// Used by strategies for accuracy tracking and by the frontend for historical
/// round outcome tables. Default limit is 50 rounds.
async fn api_resolved(
    State(state): State<ApiState>,
    Query(q): Query<ResolvedQuery>,
) -> impl IntoResponse {
    let asset = q.asset.as_deref().and_then(parse_asset);
    let tf = q.tf.as_deref().and_then(Timeframe::from_slug);
    let limit = q.limit.unwrap_or(50);

    let resolved = state
        .round_tracker
        .read()
        .map(|rt| rt.get_resolved(asset, tf, limit))
        .unwrap_or_default();

    // Convert to API-friendly ResolvedRoundData (string keys, ISO timestamps)
    let data: Vec<ResolvedRoundData> = resolved
        .into_iter()
        .map(|r| ResolvedRoundData {
            condition_id: r.condition_id,
            asset: format!("{:?}", r.asset),
            timeframe: r.timeframe.slug().to_string(),
            reference_price: r.reference_price,
            close_price: r.close_price,
            resolved_direction: r.resolved_direction,
            resolved_at: r.resolved_at.to_rfc3339(),
            round_start: r.round_start.to_rfc3339(),
        })
        .collect();

    Json(data)
}

// === Helpers ===
// These functions extract and transform data from shared state into API response structs.
// They are synchronous (no async) because they only read from RwLock-protected state.

/// Parse a string asset slug into an `Asset` enum value.
///
/// Hardcoded to the 4 supported assets: BTC, ETH, SOL, XRP. Returns `None`
/// for any other input, which causes the calling handler to return an empty
/// response rather than an error.
fn parse_asset(s: &str) -> Option<Asset> {
    match s.to_uppercase().as_str() {
        "BTC" => Some(Asset::BTC),
        "ETH" => Some(Asset::ETH),
        "SOL" => Some(Asset::SOL),
        "XRP" => Some(Asset::XRP),
        _ => None,
    }
}

/// Read the latest spot prices from DataState and return as a HashMap
/// keyed by uppercase asset slug (e.g. "BTC" -> 67000.0).
///
/// Returns an empty map if the lock is poisoned (should never happen in practice).
fn read_latest_prices(ds: &polybot_data::DataState) -> HashMap<String, f64> {
    let mut out = HashMap::new();
    if let Ok(prices) = ds.latest_prices.read() {
        for (asset, (price, _ts)) in prices.iter() {
            out.insert(asset.slug_str().to_uppercase(), *price);
        }
    }
    out
}

/// Build a map of reference prices (round-start spot prices) for all asset/timeframe
/// combinations, keyed as "BTC_5m", "ETH_15m", etc.
///
/// These are the "strike prices" for binary outcome determination. The bot uses
/// them to compute displacement (how far spot has moved from the round start).
fn build_reference_prices(rt: &Arc<RwLock<RoundTracker>>) -> HashMap<String, f64> {
    let mut out = HashMap::new();
    if let Ok(tracker) = rt.read() {
        // Iterate all asset/timeframe combos and collect references
        for asset in Asset::ALL {
            for tf in Timeframe::DEFAULT {
                if let Some(price) = tracker.get_reference(asset, tf) {
                    let key = format!("{}_{}", asset.slug_str().to_uppercase(), tf.slug());
                    out.insert(key, price);
                }
            }
        }
    }
    out
}

/// Build a map of all available candles for all assets and intervals.
/// Keys are formatted as "BTC_1m", "ETH_5m", etc.
///
/// Iterates the hardcoded interval list ["1m", "5m", "15m", "1h"] for each
/// of the 4 supported assets. Only includes non-empty candle arrays to keep
/// the payload compact.
fn build_all_candles(cs: &Arc<RwLock<CandleStore>>) -> HashMap<String, Vec<Candle>> {
    let mut out = HashMap::new();
    if let Ok(store) = cs.read() {
        for asset in Asset::ALL {
            let key_prefix = asset.slug_str().to_uppercase();
            for interval in &["1m", "5m", "15m", "1h"] {
                let candles = store.get_candles(asset, interval);
                if !candles.is_empty() {
                    out.insert(format!("{}_{}", key_prefix, interval), candles);
                }
            }
        }
    }
    out
}

/// Build the resolved rounds section of the cycle-data response.
///
/// Returns up to 100 most recent resolved rounds (hardcoded limit to keep
/// the cycle-data payload reasonable). Converts internal `ResolvedRound` structs
/// to API-friendly `ResolvedRoundData` with string keys and ISO timestamps.
fn build_resolved_data(rt: &Arc<RwLock<RoundTracker>>) -> Vec<ResolvedRoundData> {
    if let Ok(tracker) = rt.read() {
        tracker
            .get_resolved(None, None, 100)
            .into_iter()
            .map(|r| ResolvedRoundData {
                condition_id: r.condition_id,
                asset: format!("{:?}", r.asset),
                timeframe: r.timeframe.slug().to_string(),
                reference_price: r.reference_price,
                close_price: r.close_price,
                resolved_direction: r.resolved_direction,
                resolved_at: r.resolved_at.to_rfc3339(),
                round_start: r.round_start.to_rfc3339(),
            })
            .collect()
    } else {
        Vec::new()
    }
}

/// Read Binance futures state from DataState and convert to API response structs.
///
/// Maps each asset's internal `FuturesState` to a `FuturesSnapshot` with
/// serializable liquidation events. Keyed by uppercase asset slug.
fn read_futures(ds: &polybot_data::DataState) -> HashMap<String, FuturesSnapshot> {
    let mut out = HashMap::new();
    if let Ok(fs) = ds.futures_state.read() {
        for (asset, state) in fs.iter() {
            let liquidations: Vec<LiquidationSnapshot> = state
                .recent_liquidations
                .iter()
                .map(|e| LiquidationSnapshot {
                    side: e.side.clone(),
                    price: e.price,
                    quantity: e.quantity,
                    timestamp: e.timestamp.to_rfc3339(),
                })
                .collect();
            out.insert(
                asset.slug_str().to_uppercase(),
                FuturesSnapshot {
                    funding_rate: state.funding_rate,
                    open_interest: state.open_interest,
                    taker_buy_sell_ratio: state.taker_buy_sell_ratio,
                    oi_change_5m: state.oi_change_5m,
                    recent_liquidations: liquidations,
                },
            );
        }
    }
    out
}

/// Read Polymarket token price trajectories from DataState.
///
/// Returns a map from condition_id to a vector of TokenTickSnapshots.
/// Each snapshot captures the market-implied p_up and p_down at a point in time.
fn read_token_prices(ds: &polybot_data::DataState) -> HashMap<String, Vec<TokenTickSnapshot>> {
    let mut out = HashMap::new();
    if let Ok(ticks) = ds.token_prices.read() {
        for (round_key, deque) in ticks.iter() {
            let snapshots: Vec<TokenTickSnapshot> = deque
                .iter()
                .map(|t| TokenTickSnapshot {
                    p_up: t.p_up,
                    p_down: t.p_down,
                    timestamp: t.timestamp.to_rfc3339(),
                })
                .collect();
            out.insert(round_key.condition_id.clone(), snapshots);
        }
    }
    out
}

/// Read Polymarket CLOB order book snapshots from DataState.
///
/// Produces two keys per order book:
/// 1. Primary key: condition_id (for direct round lookup).
/// 2. Secondary key: "BTC_5m" style (for cross-scanner lookup when only
///    asset+timeframe is known, not the condition_id).
///
/// The reverse mapping from condition_id to (asset, timeframe) is built from
/// the active rounds in the RoundTracker.
fn read_order_books(ds: &polybot_data::DataState, rt: &Arc<RwLock<RoundTracker>>) -> HashMap<String, OrderBookSnapshotData> {
    let mut out = HashMap::new();
    // Build reverse map: condition_id -> (asset, timeframe) from RoundTracker
    let cid_to_at: HashMap<String, (Asset, Timeframe)> = rt.read()
        .map(|tracker| {
            tracker.active_rounds().into_iter()
                .map(|r| (r.condition_id.clone(), (r.asset, r.timeframe)))
                .collect()
        })
        .unwrap_or_default();

    if let Ok(books) = ds.order_books.read() {
        for (round_key, snap) in books.iter() {
            let snap_data = OrderBookSnapshotData {
                bids_up: snap.bids_up.clone(),
                asks_up: snap.asks_up.clone(),
                bids_down: snap.bids_down.clone(),
                asks_down: snap.asks_down.clone(),
                updated: snap.updated.to_rfc3339(),
            };
            // Primary key: condition_id
            out.insert(round_key.condition_id.clone(), snap_data.clone());
            // Secondary key: "BTC_5m" style for cross-scanner lookup
            if let Some((asset, tf)) = cid_to_at.get(&round_key.condition_id) {
                out.insert(format!("{}_{}", asset.slug_str(), tf.slug_str()), snap_data);
            }
        }
    }
    out
}

/// Read Deribit options state from DataState and convert to API response structs.
///
/// Only populated when the Deribit WS adapter is enabled (ENABLE_DERIBIT=true).
/// Keyed by uppercase asset slug (currently only "BTC" and "ETH").
fn read_options(ds: &polybot_data::DataState) -> HashMap<String, OptionsSnapshot> {
    let mut out = HashMap::new();
    if let Ok(opts) = ds.options_state.read() {
        for (asset, state) in opts.iter() {
            out.insert(
                asset.slug_str().to_uppercase(),
                OptionsSnapshot {
                    iv_atm: state.iv_atm,
                    skew: state.skew,
                    put_call_ratio: state.put_call_ratio,
                    dvol: state.dvol,
                    updated: state.updated.to_rfc3339(),
                },
            );
        }
    }
    out
}

/// Read Polymarket CLOB trade fill tapes from DataState.
///
/// Returns the last 100 fills per round (hardcoded limit to keep the cycle-data
/// payload under ~50KB). The fills are ordered chronologically (oldest first).
/// Used by the `clob_microstructure` strategy for trade flow imbalance computation.
fn read_trade_tapes(ds: &polybot_data::DataState) -> HashMap<String, Vec<TradeSnapshot>> {
    let mut out = HashMap::new();
    if let Ok(tapes) = ds.trade_tapes.read() {
        for (round_key, deque) in tapes.iter() {
            // Only send last 100 fills per round to keep payload reasonable
            let snapshots: Vec<TradeSnapshot> = deque.iter().rev().take(100).rev().map(|f| {
                TradeSnapshot {
                    side: format!("{:?}", f.side),
                    price: f.price,
                    size: f.size,
                    is_buyer_maker: f.is_buyer_maker,
                    timestamp: f.timestamp.to_rfc3339(),
                }
            }).collect();
            if !snapshots.is_empty() {
                out.insert(round_key.condition_id.clone(), snapshots);
            }
        }
    }
    out
}

/// Read Coinbase cross-exchange premiums from DataState.
///
/// The premium is (coinbase_price - binance_price) / binance_price.
/// Positive = Coinbase is more expensive (US institutional buying pressure).
/// Used by the `clob_microstructure` strategy as a directional signal.
fn read_coinbase_premiums(ds: &polybot_data::DataState) -> HashMap<String, f64> {
    let mut out = HashMap::new();
    if let Ok(premiums) = ds.coinbase_premium.read() {
        for (asset, &premium) in premiums.iter() {
            out.insert(asset.slug_str().to_uppercase(), premium);
        }
    }
    out
}

/// Build adapter connectivity status by checking data freshness.
///
/// Each adapter has a hardcoded staleness threshold:
/// - binance_spot: 30s (WS ticks every ~100ms, so >30s = definitely dead)
/// - binance_futures: 120s (REST polling every 60s, some slack for API delays)
/// - polymarket_clob: 60s (WS, but markets can be quiet for extended periods)
/// - deribit_options: 120s (only shown if any data has been received)
///
/// These thresholds are hardcoded because they are properties of the data sources
/// themselves, not configurable by the user. A disconnected adapter is not fatal
/// -- strategies that depend on that data source will simply abstain.
fn build_adapter_status(ds: &polybot_data::DataState) -> Vec<AdapterStatus> {
    let now = Utc::now();
    let mut adapters = Vec::new();

    // Check Binance spot data freshness via latest_prices
    let binance_age = if let Ok(prices) = ds.latest_prices.read() {
        prices
            .values()
            .map(|(_, ts)| (now - *ts).num_seconds())
            .min()
            .unwrap_or(999)
    } else {
        999
    };
    adapters.push(AdapterStatus {
        name: "binance_spot".to_string(),
        connected: binance_age < 30,
        last_data_secs_ago: binance_age,
    });

    // Check futures data freshness
    let futures_age = if let Ok(fs) = ds.futures_state.read() {
        fs.values()
            .map(|s| (now - s.funding_updated).num_seconds())
            .min()
            .unwrap_or(999)
    } else {
        999
    };
    adapters.push(AdapterStatus {
        name: "binance_futures".to_string(),
        connected: futures_age < 120,
        last_data_secs_ago: futures_age,
    });

    // Check polymarket token data freshness
    let poly_age = if let Ok(books) = ds.order_books.read() {
        books
            .values()
            .map(|b| (now - b.updated).num_seconds())
            .min()
            .unwrap_or(999)
    } else {
        999
    };
    adapters.push(AdapterStatus {
        name: "polymarket_clob".to_string(),
        connected: poly_age < 60,
        last_data_secs_ago: poly_age,
    });

    // Check Deribit options data freshness
    let deribit_age = if let Ok(opts) = ds.options_state.read() {
        opts.values()
            .map(|o| (now - o.updated).num_seconds())
            .min()
            .unwrap_or(999)
    } else {
        999
    };
    if deribit_age < 999 {
        adapters.push(AdapterStatus {
            name: "deribit_options".to_string(),
            connected: deribit_age < 120,
            last_data_secs_ago: deribit_age,
        });
    }

    adapters
}

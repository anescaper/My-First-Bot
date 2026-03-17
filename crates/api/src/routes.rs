//! REST API route handlers for the bot control plane and monitoring.
//!
//! Split into three sections:
//! 1. **General pipeline routes**: health, status, positions, signals, metrics.
//! 2. **Crypto pipeline routes**: crypto-specific positions, signals, rounds, prices.
//! 3. **History routes**: SQLite-backed historical data queries.
//!
//! Route handlers are thin -- they read from or write to `AppState` via RwLock
//! and return JSON. Business logic lives in the pipeline and strategy layers.

use axum::{extract::State, Json};
use axum::extract::ws::{WebSocket, WebSocketUpgrade, Message};
use std::sync::Arc;
use serde::Deserialize;
use crate::state::*;
use polybot_risk::types::RiskConfig;
use std::collections::HashMap;

/// GET /health -- simple liveness probe. Returns "ok" if the process is running.
/// Used by Docker healthcheck and load balancers.
pub async fn health() -> &'static str {
    "ok"
}

/// GET /status -- returns bot mode, version, instance name, and strategy profile.
pub async fn status(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let mode = *state.mode.read().unwrap();
    let started = state.started_at.read().unwrap().clone();
    Json(serde_json::json!({
        "mode": mode,
        "started_at": started,
        "version": env!("CARGO_PKG_VERSION"),
        "instance_name": state.instance_name,
        "strategy_profile": state.strategy_profile,
    }))
}

/// GET /positions -- returns all active positions for the general pipeline.
pub async fn positions(State(state): State<Arc<AppState>>) -> Json<Vec<Position>> {
    Json(state.positions.read().unwrap().clone())
}

/// GET /signals -- returns recent signals for the general pipeline.
pub async fn signals(State(state): State<Arc<AppState>>) -> Json<Vec<Signal>> {
    Json(state.signals.read().unwrap().clone())
}

/// GET /closed-positions -- returns all closed positions for the general pipeline.
pub async fn closed_positions(State(state): State<Arc<AppState>>) -> Json<Vec<Position>> {
    Json(state.closed_positions.read().unwrap().clone())
}

/// GET /metrics -- returns aggregate performance metrics (general pipeline).
pub async fn metrics(State(state): State<Arc<AppState>>) -> Json<BotMetrics> {
    Json(state.metrics.read().unwrap().clone())
}

/// GET /latency -- returns per-stage pipeline latency metrics.
pub async fn latency(State(state): State<Arc<AppState>>) -> Json<LatencyMetrics> {
    Json(state.latency.read().unwrap().clone())
}

/// GET /config -- returns the current risk configuration.
pub async fn get_config(State(state): State<Arc<AppState>>) -> Json<RiskConfig> {
    Json(state.config.read().unwrap().clone())
}

/// Request body for POST /config. Uses `#[serde(flatten)]` so the JSON body
/// is the RiskConfig itself (no wrapper key needed).
#[derive(Deserialize)]
pub struct ConfigUpdate {
    #[serde(flatten)]
    pub config: RiskConfig,
}

/// POST /config -- replace the entire risk configuration.
/// Protected by auth middleware. Takes effect immediately on the next cycle.
pub async fn update_config(
    State(state): State<Arc<AppState>>,
    Json(update): Json<ConfigUpdate>,
) -> Json<serde_json::Value> {
    *state.config.write().unwrap() = update.config;
    Json(serde_json::json!({"status": "ok"}))
}

/// Request body for POST /start.
#[derive(Deserialize)]
pub struct StartRequest {
    /// "paper" or "live". Any other value defaults to "paper".
    pub mode: String,
    /// Must be `true` to enable live mode. This is a safety gate to prevent
    /// accidental live trading. Defaults to false.
    #[serde(default)]
    pub confirm_live: bool,
}

/// POST /start -- start the bot in paper or live mode.
///
/// For live mode, enforces two safety gates:
/// 1. `confirm_live: true` must be explicitly set in the request body.
/// 2. Required secrets (API key, secret, private key) must be available
///    either as environment variables or Docker secret files.
///
/// This two-gate design prevents accidental live trading: you cannot enter
/// live mode by accident (must explicitly confirm) or without credentials.
pub async fn start_bot(
    State(state): State<Arc<AppState>>,
    Json(req): Json<StartRequest>,
) -> (axum::http::StatusCode, Json<serde_json::Value>) {
    let mode = match req.mode.as_str() {
        "live" => {
            // Safety gate: require explicit confirmation for live mode
            if !req.confirm_live {
                return (axum::http::StatusCode::BAD_REQUEST, Json(serde_json::json!({
                    "error": "Live mode requires confirm_live: true"
                })));
            }
            // Verify required secrets are available (env var or Docker secret file)
            let required = ["POLYMARKET_API_KEY", "POLYMARKET_API_SECRET", "POLYMARKET_PRIVATE_KEY"];
            let missing: Vec<&str> = required.iter()
                .filter(|k| {
                    std::env::var(k).is_err()
                        && std::fs::read_to_string(format!("/run/secrets/{}", k.to_lowercase())).is_err()
                })
                .copied()
                .collect();
            if !missing.is_empty() {
                return (axum::http::StatusCode::BAD_REQUEST, Json(serde_json::json!({
                    "error": "Missing required secrets for live mode",
                    "missing": missing,
                })));
            }
            tracing::warn!("LIVE MODE ACTIVATED for {}", state.instance_name);
            BotMode::Live
        }
        _ => BotMode::Paper,
    };
    *state.mode.write().unwrap() = mode;
    *state.started_at.write().unwrap() = Some(chrono::Utc::now());
    (axum::http::StatusCode::OK, Json(serde_json::json!({"status": "started", "mode": mode})))
}

/// POST /stop -- stop the bot (sets mode to Stopped, clears start time).
/// Does NOT clear positions -- they remain for review.
pub async fn stop_bot(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    *state.mode.write().unwrap() = BotMode::Stopped;
    *state.started_at.write().unwrap() = None;
    Json(serde_json::json!({"status": "stopped"}))
}

/// GET /stream -- WebSocket upgrade for real-time event streaming.
///
/// Clients receive JSON-encoded events (strategy updates, mode changes, kills)
/// via a broadcast channel. Each connected client gets its own receiver from
/// the shared broadcast::Sender in AppState.
pub async fn ws_stream(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl axum::response::IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

/// WebSocket handler: subscribes to the broadcast channel and forwards
/// all messages to the connected client. Terminates when the client disconnects
/// or the send fails (broken pipe).
async fn handle_ws(mut socket: WebSocket, state: Arc<AppState>) {
    let mut rx = state.tx.subscribe();
    while let Ok(msg) = rx.recv().await {
        if socket.send(Message::Text(msg.into())).await.is_err() {
            break;
        }
    }
}

/// GET /strategies -- returns strategy configs for the general pipeline.
pub async fn get_strategies(State(state): State<Arc<AppState>>) -> Json<Vec<StrategyConfig>> {
    Json(state.strategies.read().unwrap().clone())
}

/// POST /strategies -- replace all strategy configs and notify WS clients.
pub async fn update_strategies(
    State(state): State<Arc<AppState>>,
    Json(configs): Json<Vec<StrategyConfig>>,
) -> Json<serde_json::Value> {
    *state.strategies.write().unwrap() = configs;
    let _ = state.tx.send(r#"{"event":"strategies_updated"}"#.to_string());
    Json(serde_json::json!({"status": "ok"}))
}

/// POST /kill -- emergency stop: stops the bot AND clears all active positions.
///
/// More aggressive than /stop, which preserves positions. Used when you need
/// to immediately halt all activity and reset state (e.g. after detecting a bug).
/// Notifies WS clients with a "killed" event.
pub async fn kill_bot(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    *state.mode.write().unwrap() = BotMode::Stopped;
    *state.started_at.write().unwrap() = None;
    state.positions.write().unwrap().clear();
    state.crypto_positions.write().unwrap().clear();
    let _ = state.tx.send(r#"{"event":"killed"}"#.to_string());
    Json(serde_json::json!({"status": "killed", "positions_cleared": true}))
}

// === Crypto Routes ===
// These mirror the general pipeline routes but read from the crypto-specific
// state fields. The crypto pipeline tracks positions, signals, and metrics
// independently from the general pipeline.

/// GET /crypto/positions -- returns active crypto pipeline positions.
pub async fn crypto_positions(State(state): State<Arc<AppState>>) -> Json<Vec<Position>> {
    Json(state.crypto_positions.read().unwrap().clone())
}

/// GET /crypto/signals -- returns recent crypto pipeline signals.
pub async fn crypto_signals(State(state): State<Arc<AppState>>) -> Json<Vec<Signal>> {
    Json(state.crypto_signals.read().unwrap().clone())
}

/// GET /crypto/rounds -- returns active crypto rounds with enriched metadata.
///
/// Adds computed fields to each round: `seconds_left`, `market_up_price`, and
/// `prediction` (our estimated p_up). The prediction falls back to the market
/// price if no strategy estimate is available.
pub async fn crypto_rounds(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let rounds = state.crypto_rounds.read().unwrap();
    let enriched: Vec<serde_json::Value> = rounds.iter().map(|r| {
        let mut v = serde_json::to_value(r).unwrap_or_default();
        if let Some(obj) = v.as_object_mut() {
            obj.insert("seconds_left".into(), serde_json::json!(r.seconds_remaining()));
            obj.insert("market_up_price".into(), serde_json::json!(r.price_up));
            obj.insert("prediction".into(), serde_json::json!(r.our_p_up.unwrap_or(r.price_up)));
        }
        v
    }).collect();
    Json(serde_json::json!({
        "rounds": enriched,
        "count": enriched.len(),
    }))
}

/// GET /crypto/prices -- returns latest spot prices per asset from the rounds data.
///
/// Extracts unique asset prices from active rounds (uses the first round per asset).
/// Source is always "Binance" since that is where spot prices come from.
pub async fn crypto_prices(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let rounds = state.crypto_rounds.read().unwrap();
    let mut prices = serde_json::Map::new();
    let mut seen = std::collections::HashSet::new();
    for round in rounds.iter() {
        let key = format!("{:?}", round.asset);
        if seen.contains(&key) { continue; }
        if let Some(price) = round.current_price {
            prices.insert(key.clone(), serde_json::json!({
                "price": price,
                "source": "Binance",
            }));
            seen.insert(key);
        }
    }
    Json(serde_json::Value::Object(prices))
}

/// GET /crypto/metrics -- returns crypto pipeline performance metrics plus
/// cycle speed and nearest round expiry time.
pub async fn crypto_metrics(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let metrics = state.crypto_metrics.read().unwrap();
    let cycle_speed = *state.crypto_cycle_speed_ms.read().unwrap();
    let nearest_expiry = *state.crypto_nearest_expiry.read().unwrap();

    Json(serde_json::json!({
        "total_trades": metrics.total_trades,
        "wins": metrics.wins,
        "losses": metrics.losses,
        "win_rate": metrics.win_rate,
        "sharpe_ratio": metrics.sharpe_ratio,
        "total_pnl": metrics.total_pnl,
        "max_drawdown": metrics.max_drawdown,
        "profit_factor": metrics.profit_factor,
        "cycle_speed_ms": cycle_speed,
        "nearest_expiry_secs": nearest_expiry,
    }))
}

/// GET /ladder -- returns bankroll ladder state including progress toward
/// the current round's target.
pub async fn ladder(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let ladder = state.ladder.read().unwrap();
    let progress = if let Some(current) = ladder.rounds.last() {
        if current.target_pnl > 0.0 {
            (ladder.total_profit / current.target_pnl * 100.0).min(100.0).max(0.0)
        } else { 0.0 }
    } else { 0.0 };

    Json(serde_json::json!({
        "seed": ladder.seed,
        "aggressive_cap": ladder.aggressive_cap,
        "current_round": ladder.current_round,
        "locked": ladder.locked,
        "available": ladder.available,
        "total_profit": ladder.total_profit,
        "progress_pct": progress,
        "rounds": ladder.rounds,
    }))
}

/// GET /pipeline-mode -- returns current pipeline mode (both/crypto_hf/general_lf).
pub async fn get_pipeline_mode(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let mode = *state.pipeline_mode.read().unwrap();
    Json(serde_json::json!({"mode": mode}))
}

/// Request body for POST /pipeline-mode.
#[derive(Deserialize)]
pub struct PipelineModeUpdate {
    pub mode: PipelineMode,
}

/// POST /pipeline-mode -- change which pipeline(s) are active.
/// Notifies WS clients of the mode change.
pub async fn set_pipeline_mode(
    State(state): State<Arc<AppState>>,
    Json(update): Json<PipelineModeUpdate>,
) -> Json<serde_json::Value> {
    *state.pipeline_mode.write().unwrap() = update.mode;
    let _ = state.tx.send(serde_json::json!({"event": "pipeline_mode_changed", "mode": update.mode}).to_string());
    Json(serde_json::json!({"status": "ok", "mode": update.mode}))
}

// === Crypto Strategies ===

/// GET /crypto/strategies -- returns strategy configs for the crypto pipeline.
pub async fn get_crypto_strategies(State(state): State<Arc<AppState>>) -> Json<Vec<StrategyConfig>> {
    Json(state.crypto_strategies.read().unwrap().clone())
}

/// POST /crypto/strategies -- replace crypto strategy configs and notify WS clients.
pub async fn update_crypto_strategies(
    State(state): State<Arc<AppState>>,
    Json(configs): Json<Vec<StrategyConfig>>,
) -> Json<serde_json::Value> {
    *state.crypto_strategies.write().unwrap() = configs;
    let _ = state.tx.send(r#"{"event":"crypto_strategies_updated"}"#.to_string());
    Json(serde_json::json!({"status": "ok"}))
}

// === History Endpoints (SQLite) ===
// These endpoints query the SQLite database for historical data.
// They return errors gracefully if no database is configured (db: None).

/// GET /history/positions -- paginated query of historical positions.
///
/// Query params: `pipeline` (default "crypto"), `limit` (default 100), `offset` (default 0).
pub async fn history_positions(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let pipeline = params.get("pipeline").map(|s| s.as_str()).unwrap_or("crypto");
    let limit: i64 = params.get("limit").and_then(|s| s.parse().ok()).unwrap_or(100);
    let offset: i64 = params.get("offset").and_then(|s| s.parse().ok()).unwrap_or(0);

    match &state.db {
        Some(db) => match db.query_positions(pipeline, limit, offset) {
            Ok(positions) => {
                let count = positions.len();
                Json(serde_json::json!({ "positions": positions, "count": count }))
            }
            Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
        },
        None => Json(serde_json::json!({ "positions": [], "error": "No database configured" })),
    }
}

/// GET /history/signals -- query historical signals from SQLite.
///
/// Query params: `pipeline` (default "crypto"), `limit` (default 100).
pub async fn history_signals(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let pipeline = params.get("pipeline").map(|s| s.as_str()).unwrap_or("crypto");
    let limit: i64 = params.get("limit").and_then(|s| s.parse().ok()).unwrap_or(100);

    match &state.db {
        Some(db) => match db.query_signals(pipeline, limit) {
            Ok(signals) => {
                let count = signals.len();
                Json(serde_json::json!({ "signals": signals, "count": count }))
            }
            Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
        },
        None => Json(serde_json::json!({ "signals": [], "error": "No database configured" })),
    }
}

/// GET /history/rounds -- query historical round outcomes from SQLite.
///
/// Query params: `asset` (default "BTC"), `timeframe` (default "5m"), `limit` (default 50).
pub async fn history_rounds(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let asset = params.get("asset").map(|s| s.as_str()).unwrap_or("BTC");
    let timeframe = params.get("timeframe").map(|s| s.as_str()).unwrap_or("5m");
    let limit: i64 = params.get("limit").and_then(|s| s.parse().ok()).unwrap_or(50);

    match &state.db {
        Some(db) => match db.query_round_history(asset, timeframe, limit) {
            Ok(rounds) => {
                let count = rounds.len();
                Json(serde_json::json!({ "rounds": rounds, "count": count }))
            }
            Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
        },
        None => Json(serde_json::json!({ "rounds": [], "error": "No database configured" })),
    }
}

/// GET /history/metrics -- query historical metric snapshots from SQLite.
///
/// Query params: `pipeline` (default "crypto"), `limit` (default 100).
pub async fn history_metrics(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let pipeline = params.get("pipeline").map(|s| s.as_str()).unwrap_or("crypto");
    let limit: i64 = params.get("limit").and_then(|s| s.parse().ok()).unwrap_or(100);

    match &state.db {
        Some(db) => match db.query_metrics(pipeline, limit) {
            Ok(snapshots) => {
                let count = snapshots.len();
                Json(serde_json::json!({ "snapshots": snapshots, "count": count }))
            }
            Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
        },
        None => Json(serde_json::json!({ "snapshots": [], "error": "No database configured" })),
    }
}

// === Backtest ===

/// Request body for POST /backtest/run.
/// All fields have defaults for convenience: BTC 5m, last 100 rounds.
#[derive(Deserialize)]
pub struct BacktestRequest {
    #[serde(default = "default_bt_asset")]
    pub asset: String,
    #[serde(default = "default_bt_timeframe")]
    pub timeframe: String,
    #[serde(default = "default_bt_limit")]
    pub limit: i64,
}
fn default_bt_asset() -> String { "BTC".into() }
fn default_bt_timeframe() -> String { "5m".into() }
fn default_bt_limit() -> i64 { 100 }

/// POST /backtest/run -- run a simple backtest over historical round data.
///
/// Fetches round history from SQLite and runs the BacktestEngine over it.
/// Returns trade-by-trade results and aggregate metrics.
pub async fn run_backtest(
    State(state): State<Arc<AppState>>,
    Json(req): Json<BacktestRequest>,
) -> Json<serde_json::Value> {
    match &state.db {
        Some(db) => {
            match db.query_round_history(&req.asset, &req.timeframe, req.limit) {
                Ok(rounds) => {
                    let engine = crate::backtest::BacktestEngine::new();
                    let result = engine.run(&rounds);
                    Json(serde_json::json!(result))
                }
                Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
            }
        }
        None => Json(serde_json::json!({ "error": "No database configured" })),
    }
}

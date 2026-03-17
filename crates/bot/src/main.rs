//! Entry point for the Polymarket trading bot binary.
//!
//! Launches 4 concurrent tasks via tokio::select!:
//! 1. **API server** (axum): REST + WebSocket on configurable port (default 4200)
//! 2. **Crypto pipeline**: trades BTC/ETH/SOL/XRP Up/Down rounds (adaptive 1-5s)
//! 3. **General pipeline**: trades non-crypto event markets (300s intervals)
//! 4. **MM pipeline**: market-making mode for bilateral-mm profile only
//!
//! All tasks share `Arc<AppState>` for cross-task state and a single `DataClient`
//! for data-hub communication. The bot auto-starts in Paper mode unless AUTO_START=false.
//!
//! ## Environment Variables
//! - `BANKROLL`: initial trading capital (default $10,000)
//! - `PORT`: API server port (default 4200)
//! - `INSTANCE_NAME`: identifier for multi-instance deployments (default "default")
//! - `STRATEGY_PROFILE`: which strategy profile to use (default "baseline")
//! - `DB_PATH`: SQLite database path (default "data/polybot.db")
//! - `DATA_HUB_URL`: data-hub service URL (default "http://data-hub:4250")
//! - `AUTO_START`: whether to auto-start in Paper mode (default true)

use anyhow::Result;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;
use polybot_api::state::{AppState, BotMode, PipelineMode};

mod bilateral;
mod crypto_strategies;
mod crypto_strategies_v2;
mod data_bridge;
mod mm_pipeline;
mod pipeline;
mod quant;
mod strategies;
pub mod timeframe_intel;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    tracing::info!("Polymarket Trading Bot v{}", env!("CARGO_PKG_VERSION"));

    // Initialize SQLite database for persistence
    let db_path = std::env::var("DB_PATH").unwrap_or_else(|_| "data/polybot.db".to_string());
    if let Some(parent) = std::path::Path::new(&db_path).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let db = match polybot_api::db::Database::new(&db_path) {
        Ok(db) => {
            tracing::info!("Database opened at {}", db_path);
            Some(Arc::new(db))
        }
        Err(e) => {
            tracing::warn!("Failed to open database: {}, running without persistence", e);
            None
        }
    };

    let bankroll: f64 = std::env::var("BANKROLL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000.0);
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4200);
    let instance_name = std::env::var("INSTANCE_NAME").unwrap_or_else(|_| "default".into());
    let strategy_profile = std::env::var("STRATEGY_PROFILE").unwrap_or_else(|_| "baseline".into());

    let known_profiles = ["baseline", "garch-t", "garch-t-aggressive", "hurst-hinf", "full-quant", "bilateral-mm"];
    if !known_profiles.contains(&strategy_profile.as_str()) {
        tracing::warn!("Unknown STRATEGY_PROFILE '{}', known profiles: {:?}. Falling back to baseline behavior.",
            strategy_profile, known_profiles);
    }

    tracing::info!("Instance: {} | Profile: {} | Bankroll: ${:.2} | Port: {}",
        instance_name, strategy_profile, bankroll, port);

    let mut app_state = AppState::new_with_params(bankroll, instance_name, strategy_profile);
    app_state.db = db.clone();

    // Restore open positions from previous run (crash recovery)
    if let Some(ref db) = db {
        match db.load_open_positions("crypto") {
            Ok(positions) if !positions.is_empty() => {
                tracing::info!("Recovered {} open positions from database", positions.len());
                *app_state.crypto_positions.write().unwrap() = positions;
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("Failed to load open positions: {}", e),
        }
    }

    let state = Arc::new(app_state);

    // Auto-start in Paper mode unless AUTO_START=false
    let auto_start = std::env::var("AUTO_START")
        .map(|v| v != "false" && v != "0")
        .unwrap_or(true);
    if auto_start {
        *state.mode.write().unwrap() = BotMode::Paper;
        *state.started_at.write().unwrap() = Some(chrono::Utc::now());
        tracing::info!("Auto-started in Paper mode (AUTO_START=true)");
    }

    // Spawn API server
    let api_state = state.clone();
    let api_handle = tokio::spawn(async move {
        let app = polybot_api::create_router(api_state);
        let addr = format!("0.0.0.0:{}", port);
        tracing::info!("API server listening on {}", addr);
        let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
        axum::serve(listener, app).await.unwrap();
    });

    // Connect to shared data-hub service via DataClient
    let data_hub_url = std::env::var("DATA_HUB_URL").unwrap_or_else(|_| "http://data-hub:4250".to_string());
    let data_client = Arc::new(polybot_data::client::DataClient::new(&data_hub_url));
    tracing::info!("DataClient connected to {}", data_hub_url);

    // Spawn dual pipeline (crypto + general) — skip for bilateral-mm (uses MM pipeline instead)
    let pipeline_state = state.clone();
    let dc = data_client.clone();
    let skip_directional = state.strategy_profile == "bilateral-mm";
    let crypto_handle = tokio::spawn(async move {
        if skip_directional {
            tracing::info!("Directional pipeline disabled for bilateral-mm — MM pipeline handles trades");
            std::future::pending::<()>().await;
        } else {
            let mut pipe = pipeline::Pipeline::new(pipeline_state.clone(), bankroll, dc);
            pipe.sync_recovered_positions();
            pipe.restore_equity("crypto");
            pipe.restore_trade_returns("crypto");
            pipe.run_dual_pipeline().await;
        }
    });

    // Spawn general pipeline on separate interval (300s) — skip for bilateral-mm
    let general_state = state.clone();
    let dc2 = data_client.clone();
    let general_handle = tokio::spawn(async move {
        if skip_directional {
            std::future::pending::<()>().await;
        } else {
            let mut pipe = pipeline::Pipeline::new(general_state.clone(), bankroll, dc2);
            pipe.sync_recovered_positions();
            pipe.restore_equity("general");
            pipe.restore_trade_returns("general");
            loop {
                let mode = *general_state.mode.read().unwrap();
                let pipeline_mode = *general_state.pipeline_mode.read().unwrap();
                match mode {
                    BotMode::Stopped => {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                    BotMode::Paper | BotMode::Live => {
                        if matches!(pipeline_mode, PipelineMode::Both | PipelineMode::GeneralOnly) {
                            if let Err(e) = pipe.run_general_cycle().await {
                                tracing::error!("General pipeline error: {}", e);
                            }
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                    }
                }
            }
        }
    });

    // Spawn market-making pipeline (only active for bilateral-mm profile)
    let mm_state = state.clone();
    let dc3 = data_client.clone();
    let mm_handle = tokio::spawn(async move {
        if mm_state.strategy_profile == "bilateral-mm" {
            tracing::info!("MM pipeline active for bilateral-mm profile");
            let mut mm = mm_pipeline::MarketMakingPipeline::new(mm_state.clone(), bankroll, dc3);
            loop {
                let mode = *mm_state.mode.read().unwrap();
                match mode {
                    BotMode::Stopped => {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                    BotMode::Paper | BotMode::Live => {
                        if let Err(e) = mm.run_mm_cycle().await {
                            tracing::error!("MM pipeline error: {}", e);
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                }
            }
        } else {
            // Not an MM bot, just idle
            std::future::pending::<()>().await;
        }
    });

    tokio::select! {
        r = api_handle => { tracing::error!("API server exited: {:?}", r); }
        r = crypto_handle => { tracing::error!("Crypto pipeline exited: {:?}", r); }
        r = general_handle => { tracing::error!("General pipeline exited: {:?}", r); }
        r = mm_handle => { tracing::error!("MM pipeline exited: {:?}", r); }
    }

    Ok(())
}

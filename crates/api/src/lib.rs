//! REST + WebSocket API for frontend monitoring and control.
//!
//! This crate provides the bot's HTTP API, used by the frontend dashboard
//! and operational tools. It exposes:
//! - Read-only public routes for monitoring (no auth required)
//! - Protected control routes for state changes (auth required if API_AUTH_TOKEN is set)
//! - WebSocket endpoint for real-time event streaming
//!
//! The router is split into public and protected sections. Protected routes
//! go through the `auth::require_auth` middleware, which checks the
//! `Authorization: Bearer <token>` header against the `API_AUTH_TOKEN` env var.

pub mod auth;
pub mod backtest;
pub mod db;
pub mod routes;
pub mod state;

use axum::{Router, middleware, routing::get, routing::post};
use tower_http::cors::CorsLayer;
use std::sync::Arc;
use state::AppState;

/// Create the main application router with all routes and middleware.
///
/// Routes are organized into two groups:
/// - **Public** (GET-only): monitoring endpoints that do not mutate state.
///   No authentication required -- allows monitoring tools and the frontend
///   to read data without credentials.
/// - **Protected** (POST): control endpoints that mutate bot state (start,
///   stop, kill, config changes, strategy updates, backtest). Protected by
///   the `require_auth` middleware which checks `API_AUTH_TOKEN`.
///
/// CORS is set to permissive (`CorsLayer::permissive()`) because the frontend
/// runs on a different port/domain and needs full access to all endpoints.
///
/// # Arguments
/// * `state` - Shared application state wrapped in `Arc<AppState>`.
pub fn create_router(state: Arc<AppState>) -> Router {
    let cors = CorsLayer::permissive();

    // Public read-only routes (no auth required — monitoring)
    let public = Router::new()
        .route("/health", get(routes::health))
        .route("/status", get(routes::status))
        .route("/positions", get(routes::positions))
        .route("/closed-positions", get(routes::closed_positions))
        .route("/signals", get(routes::signals))
        .route("/metrics", get(routes::metrics))
        .route("/latency", get(routes::latency))
        .route("/config", get(routes::get_config))
        .route("/strategies", get(routes::get_strategies))
        .route("/stream", get(routes::ws_stream))
        .route("/crypto/positions", get(routes::crypto_positions))
        .route("/crypto/signals", get(routes::crypto_signals))
        .route("/crypto/rounds", get(routes::crypto_rounds))
        .route("/crypto/prices", get(routes::crypto_prices))
        .route("/crypto/metrics", get(routes::crypto_metrics))
        .route("/crypto/strategies", get(routes::get_crypto_strategies))
        .route("/ladder", get(routes::ladder))
        .route("/pipeline-mode", get(routes::get_pipeline_mode))
        .route("/history/positions", get(routes::history_positions))
        .route("/history/signals", get(routes::history_signals))
        .route("/history/rounds", get(routes::history_rounds))
        .route("/history/metrics", get(routes::history_metrics));

    // Protected control routes (require API_AUTH_TOKEN if set)
    let protected = Router::new()
        .route("/start", post(routes::start_bot))
        .route("/stop", post(routes::stop_bot))
        .route("/kill", post(routes::kill_bot))
        .route("/config", post(routes::update_config))
        .route("/strategies", post(routes::update_strategies))
        .route("/crypto/strategies", post(routes::update_crypto_strategies))
        .route("/pipeline-mode", post(routes::set_pipeline_mode))
        .route("/backtest/run", post(routes::run_backtest))
        .layer(middleware::from_fn(auth::require_auth));

    Router::new()
        .merge(public)
        .merge(protected)
        .layer(cors)
        .with_state(state)
}

//! Pluggable data adapter traits.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::sync::atomic::AtomicI64;
use std::sync::Arc;
use std::time::Duration;

use crate::state::DataState;

/// Classification of data source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceType {
    /// Underlying asset data (Binance spot, futures, Coinbase).
    UnderlyingAsset,
    /// Prediction market data (Polymarket CLOB).
    PredictionMarket,
}

/// Tick-level adapter for real-time WebSocket streams.
/// Each implementation handles one data source.
/// Not Sync — each adapter is owned by a single task.
#[async_trait]
pub trait TickAdapter: Send + 'static {
    /// Human-readable adapter name (e.g., "binance-spot-ws").
    fn name(&self) -> &str;

    /// Which data stream category this adapter belongs to.
    fn source_type(&self) -> SourceType;

    /// Connect to upstream. Called once at startup and on reconnect.
    async fn connect(&mut self) -> Result<()>;

    /// Graceful disconnect.
    async fn disconnect(&mut self);

    /// Subscribe to symbols/assets. Idempotent.
    async fn subscribe(&mut self, symbols: &[String]) -> Result<()>;

    /// Drive the adapter: read one message, write into DataState.
    /// Returns Ok(true) if data was produced, Ok(false) if keepalive/no-op.
    async fn poll_next(&mut self, state: &DataState) -> Result<bool>;

    /// Is the connection alive and producing data?
    fn is_healthy(&self) -> bool;

    /// Last time this adapter produced real data (not just PONG).
    fn last_data_at(&self) -> Option<DateTime<Utc>>;

    /// Max silence duration before watchdog triggers reconnect.
    fn inactivity_timeout(&self) -> Duration;

    /// Atomic last-data timestamp for watchdog monitoring.
    fn last_data_atomic(&self) -> Arc<AtomicI64>;
}

/// REST-based adapter for polling endpoints.
/// Not Sync — each adapter is owned by a single task.
#[async_trait]
pub trait RestAdapter: Send + 'static {
    /// Human-readable adapter name (e.g., "binance-futures-rest").
    fn name(&self) -> &str;

    /// Which data stream category this adapter belongs to.
    fn source_type(&self) -> SourceType;

    /// Fetch data and write into DataState. Called on poll_interval().
    async fn fetch(&self, state: &DataState) -> Result<()>;

    /// How often to poll.
    fn poll_interval(&self) -> Duration;

    /// Is the adapter working?
    fn is_healthy(&self) -> bool;

    /// Last successful fetch time.
    fn last_data_at(&self) -> Option<DateTime<Utc>>;

    /// Atomic last-data timestamp for watchdog monitoring.
    fn last_data_atomic(&self) -> Arc<AtomicI64>;
}

//! Executor trait — the abstraction boundary between pipeline logic and order execution.
//!
//! The pipeline never calls the CLOB directly. It always goes through an Executor,
//! which can be either `PaperExecutor` (simulated) or `ClobExecutor` (live).
//! This allows the same pipeline code to run in both paper and live modes.

use anyhow::Result;
use async_trait::async_trait;
use crate::order::{ClobOrderRequest, ClobOrderResponse, GtcOrderStatus};

/// Core trait for order execution — implemented by PaperExecutor and ClobExecutor.
///
/// All methods are async because live execution involves HTTP calls to the CLOB API.
/// The pipeline holds `Option<Box<dyn Executor>>` for the live executor (lazily created)
/// and a concrete `PaperExecutor` for paper mode.
#[async_trait]
pub trait Executor: Send + Sync {
    /// Submit an order (BUY or SELL) and return the response.
    /// For live: signs and submits to CLOB API. For paper: simulates fill with slippage.
    async fn execute_order(&self, request: &ClobOrderRequest) -> Result<ClobOrderResponse>;

    /// Cancel a GTC order by its order ID. Returns true if cancellation succeeded.
    async fn cancel_order(&self, order_id: &str) -> Result<bool>;

    /// Human-readable name for logging ("paper" or "clob-live")
    fn name(&self) -> &str;

    /// Whether this executor submits real orders to the Polymarket CLOB
    fn is_live(&self) -> bool;

    /// Query wallet USDC balance (and allowance) from the CLOB API.
    /// Returns the minimum of balance and allowance (can't trade more than approved).
    /// Default: f64::MAX (paper mode has unlimited funds).
    async fn get_balance(&self) -> Result<f64> { Ok(f64::MAX) }

    /// Check the status of a GTC order (Open, Filled, Cancelled, Unknown).
    /// Used by the pipeline's pending GTC order processing loop.
    /// Default: Unknown (paper executor doesn't track GTC orders).
    async fn get_order_status(&self, _order_id: &str) -> Result<GtcOrderStatus> {
        Ok(GtcOrderStatus::Unknown)
    }
}

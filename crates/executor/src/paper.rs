//! Paper (simulated) executor for backtesting and paper trading.
//!
//! Simulates order execution with configurable fill rate, slippage, and latency.
//! No real orders are submitted — all fills are synthetic.

use anyhow::Result;
use async_trait::async_trait;
use crate::executor::Executor;
use crate::order::{ClobOrderRequest, ClobOrderResponse};

/// Simulated executor that generates synthetic fills for paper trading.
///
/// Uses a deterministic hash of (price, size) to decide fill/no-fill,
/// making paper results reproducible for the same inputs.
pub struct PaperExecutor {
    /// Probability of a fill (0.0 to 1.0). Default 0.95 (95% fill rate).
    /// Hardcoded default reflects realistic CLOB fill rates for small orders.
    pub fill_rate: f64,
    /// Simulated slippage in basis points. Default 20 bps (0.20%).
    /// Applied as a price increase on fills (conservative: always slips against us).
    pub slippage_bps: f64,
    /// Simulated network latency in milliseconds. Default 200ms.
    /// Set to 0 in the pipeline's hot loop to avoid artificial delays.
    pub latency_ms: u64,
}

impl Default for PaperExecutor {
    fn default() -> Self {
        Self {
            fill_rate: 0.95,
            slippage_bps: 20.0,
            latency_ms: 200,
        }
    }
}

impl PaperExecutor {
    /// Convenience method that delegates to `execute_order` (Executor trait).
    /// Used directly by the pipeline for paper-mode crypto fills.
    pub async fn execute(&self, request: &ClobOrderRequest) -> Result<ClobOrderResponse> {
        self.execute_order(request).await
    }
}

#[async_trait]
impl Executor for PaperExecutor {
    async fn execute_order(&self, request: &ClobOrderRequest) -> Result<ClobOrderResponse> {
        tokio::time::sleep(std::time::Duration::from_millis(self.latency_ms)).await;

        // Deterministic "random" based on price and size
        let hash: u64 = (request.price * 1e6) as u64 ^ (request.size_usd * 1e4) as u64;
        let roll = (hash % 100) as f64 / 100.0;

        if roll > self.fill_rate {
            return Ok(ClobOrderResponse {
                success: false,
                order_id: None,
                status: Some("no_fill".into()),
                transact_hash: None,
                price: None,
                error_msg: Some("Simulated no-fill".into()),
            });
        }

        let slippage_mult = self.slippage_bps / 10_000.0;
        let fill_price = (request.price * (1.0 + slippage_mult)).clamp(0.001, 0.999);

        let ts = chrono::Utc::now().timestamp_millis();

        Ok(ClobOrderResponse {
            success: true,
            order_id: Some(format!("paper-{}", ts)),
            status: Some("filled".into()),
            transact_hash: Some(format!("poly-paper-{}", ts)),
            price: Some(fill_price),
            error_msg: None,
        })
    }

    async fn cancel_order(&self, _order_id: &str) -> Result<bool> {
        Ok(true)
    }

    fn name(&self) -> &str {
        "paper"
    }

    fn is_live(&self) -> bool {
        false
    }
}

//! Core types for the risk engine — configuration, requests, and verdicts.
//!
//! These types are used by the legacy `RiskEngine` (in `checks.rs`) which runs
//! sequential risk checks. The newer composable gate chain (`RiskGateChain`)
//! uses `GateSignal` and `GateResult` from `gate.rs` instead.
//!
//! Both systems coexist: the gate chain handles the primary trading pipeline,
//! while `RiskEngine` is used for simpler risk evaluation in backtesting and
//! API endpoints.

use serde::{Deserialize, Serialize};

/// Configuration for the legacy RiskEngine.
///
/// All monetary values are in USD. Percentage values are decimals (e.g. 0.08 = 8%).
/// These defaults are conservative baselines for paper trading.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskConfig {
    /// Minimum absolute edge (|p_model - p_market|) to enter a trade.
    /// Default: 0.04 (4%). Trades with edge below this are rejected.
    pub edge_threshold: f64,
    /// Base fraction of bankroll for position sizing. See kelly.rs for details.
    /// Default: 0.25 (25%).
    pub kelly_fraction: f64,
    /// Maximum USD exposure for any single position.
    /// Default: $1,000. Caps individual position risk.
    pub max_exposure_per_position: f64,
    /// Maximum total USD exposure across all open positions.
    /// Default: $5,000.
    pub max_total_exposure: f64,
    /// Maximum acceptable Value at Risk at 95% confidence (USD).
    /// Default: $500. Theoretical loss threshold.
    pub var_limit_95: f64,
    /// Maximum drawdown (as a decimal) before the circuit breaker trips.
    /// Default: 0.08 (8%). Lower than RiskChainConfig's 25% because the
    /// legacy engine is more conservative.
    pub mdd_limit: f64,
    /// Maximum number of concurrent open positions.
    /// Default: 10.
    pub max_positions: usize,
    /// Minimum market liquidity (USD) to consider for trading.
    /// Default: $10,000. Ensures adequate depth for execution.
    pub min_liquidity: f64,
}

impl Default for RiskConfig {
    /// Conservative defaults for paper trading. See field-level docs for rationale.
    fn default() -> Self {
        Self {
            edge_threshold: 0.04,
            kelly_fraction: 0.25,
            max_exposure_per_position: 1000.0,
            max_total_exposure: 5000.0,
            var_limit_95: 500.0,
            mdd_limit: 0.08,
            max_positions: 10,
            min_liquidity: 10_000.0,
        }
    }
}

/// A request to evaluate a potential position through the risk engine.
///
/// Contains the model's assessment of the market (p_model, edge, z_score)
/// alongside market data (p_market) and trade metadata (market_id, direction).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionRequest {
    /// Polymarket condition ID or other unique market identifier.
    pub market_id: String,
    /// Human-readable market question.
    pub question: String,
    /// Which side we want to buy (Yes or No).
    pub direction: Direction,
    /// Our model's estimated probability of the Yes outcome.
    pub p_model: f64,
    /// Current market price of the Yes token (implied market probability).
    pub p_market: f64,
    /// Edge = p_model - p_market (positive means we think Yes is underpriced).
    pub edge: f64,
    /// Z-score of the edge (how many standard deviations the edge represents).
    /// Used for confidence assessment — higher z-scores indicate more statistically
    /// significant edges.
    pub z_score: f64,
}

/// Direction of a binary market trade.
///
/// `BuyYes` = buy the Yes token (betting the event happens).
/// `BuyNo` = buy the No token (betting the event doesn't happen).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum Direction {
    BuyYes,
    BuyNo,
}

/// The risk engine's verdict on a proposed position.
///
/// If `approved`, the fields `kelly_fraction`, `position_size`, and
/// `expected_value` contain the recommended sizing. If not approved,
/// these fields are 0.0 and `reason` explains why.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskVerdict {
    /// Whether the position is approved for execution.
    pub approved: bool,
    /// Human-readable explanation (either "All risk checks passed" or rejection reason).
    pub reason: String,
    /// Recommended sizing fraction (0.0 to kelly_fraction).
    pub kelly_fraction: f64,
    /// Recommended position size in USD.
    pub position_size: f64,
    /// Expected value of the position in USD.
    pub expected_value: f64,
}

//! Composable risk gate trait and result type.

use serde::Serialize;

/// Result of a single risk gate evaluation.
#[derive(Debug, Clone, Serialize)]
pub enum GateResult {
    /// Signal passes this gate unchanged.
    Pass,
    /// Signal rejected. Reason is logged.
    Reject(String),
    /// Signal allowed but position size reduced. Multiplier in (0, 1].
    Resize {
        multiplier: f64,
        reason: String,
    },
}

/// A single risk check. Stateless evaluation.
pub trait RiskGate: Send + Sync {
    /// Human-readable gate name.
    fn name(&self) -> &str;

    /// Evaluate a trading signal against current state.
    fn evaluate(&self, signal: &GateSignal) -> GateResult;

    /// Priority order (lower = runs first). Default 100.
    fn priority(&self) -> u32 {
        100
    }

    /// Is this gate enabled? Default true.
    fn is_enabled(&self) -> bool {
        true
    }
}

/// Input to risk gates — contains the signal to evaluate.
#[derive(Debug, Clone, Serialize)]
pub struct GateSignal {
    pub market_id: String,
    pub asset: String,
    pub timeframe: String,
    pub direction: String,
    pub p_up: f64,
    pub confidence: f64,
    pub edge: f64,
    pub entry_price: f64,
    pub proposed_size_usd: f64,
    pub current_total_exposure: f64,
    pub current_drawdown_pct: f64,
    pub current_position_count: usize,
    pub bankroll: f64,
    pub current_net_exposure: f64,
    pub current_volatility: f64,
    pub avg_volatility: f64,
    /// Actual market token prices (from CLOB bid/ask), NOT model probabilities.
    /// For bilateral MM: if market_p_up + market_p_down < 1.0, spread exists.
    pub market_p_up: f64,
    pub market_p_down: f64,
    /// Number of currently open positions for the same asset.
    pub current_asset_positions: usize,
    /// Cross-timeframe agreement: 0.0 = full disagreement, 1.0 = all timeframes agree on direction
    pub timeframe_agreement: f64,
    /// Number of active positions in the same direction as this signal.
    pub same_direction_count: usize,
    /// Total USD exposure of active positions in the same direction.
    pub same_direction_exposure: f64,
}

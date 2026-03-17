//! Legacy risk engine with sequential validation checks.
//!
//! The `RiskEngine` runs a fixed sequence of 5 checks (edge, positions, drawdown,
//! sizing, exposure) and returns a binary approved/rejected verdict with sizing.
//!
//! This engine predates the composable gate chain (`RiskGateChain`) and is simpler
//! but less flexible. It's retained for backtesting, API endpoints, and as a
//! fallback. The gate chain is the primary risk system for live trading.
//!
//! State: The engine tracks portfolio state (exposure, positions, equity) mutably.
//! Call `update_position_opened/closed` to keep state in sync with actual positions.

use crate::types::{RiskConfig, PositionRequest, RiskVerdict};
use crate::kelly;

/// Stateful risk engine that tracks portfolio state and evaluates position requests.
///
/// Unlike the composable `RiskGateChain` (which is stateless and receives all context
/// via `GateSignal`), the `RiskEngine` maintains its own mutable state for exposure,
/// position count, and equity tracking.
///
/// Check order:
/// 1. Edge threshold (minimum absolute edge)
/// 2. Max positions (concurrency limit)
/// 3. Max drawdown (circuit breaker)
/// 4. Position sizing (Kelly/conviction-based)
/// 5. Total exposure limit
pub struct RiskEngine {
    /// Risk configuration (thresholds and limits).
    config: RiskConfig,
    /// Current bankroll in USD. Used as the basis for position sizing.
    bankroll: f64,
    /// Sum of all open position sizes in USD.
    current_exposure: f64,
    /// Count of currently open positions.
    open_positions: usize,
    /// Highest equity value seen since session start.
    /// Used to compute drawdown: (peak - current) / peak.
    peak_equity: f64,
    /// Current equity value. Updated via `update_position_closed`.
    current_equity: f64,
}

impl RiskEngine {
    /// Create a new risk engine with the given configuration and starting bankroll.
    /// Peak and current equity are initialized to the bankroll value.
    pub fn new(config: RiskConfig, bankroll: f64) -> Self {
        Self {
            config,
            bankroll,
            current_exposure: 0.0,
            open_positions: 0,
            peak_equity: bankroll,
            current_equity: bankroll,
        }
    }

    /// Evaluate a position request through all 5 sequential risk checks.
    ///
    /// Returns a `RiskVerdict` with approval status, recommended size, and expected value.
    /// Checks are ordered from cheapest to most expensive computation:
    /// 1. Edge threshold — simple comparison
    /// 2. Max positions — simple comparison
    /// 3. Drawdown circuit breaker — one division
    /// 4. Position sizing — calls kelly::compute_position_size
    /// 5. Exposure limit — addition + comparison
    pub fn evaluate(&self, request: &PositionRequest) -> RiskVerdict {
        // Check 1: Edge threshold
        if request.edge.abs() < self.config.edge_threshold {
            return RiskVerdict {
                approved: false,
                reason: format!("Edge {:.4} below threshold {:.4}", request.edge, self.config.edge_threshold),
                kelly_fraction: 0.0,
                position_size: 0.0,
                expected_value: 0.0,
            };
        }

        // Check 2: Max positions
        if self.open_positions >= self.config.max_positions {
            return RiskVerdict {
                approved: false,
                reason: format!("Max positions reached ({}/{})", self.open_positions, self.config.max_positions),
                kelly_fraction: 0.0,
                position_size: 0.0,
                expected_value: 0.0,
            };
        }

        // Check 3: MDD circuit breaker
        let current_dd = if self.peak_equity > 0.0 {
            (self.peak_equity - self.current_equity) / self.peak_equity
        } else { 0.0 };

        if current_dd >= self.config.mdd_limit {
            return RiskVerdict {
                approved: false,
                reason: format!("MDD {:.2}% exceeds limit {:.2}%", current_dd * 100.0, self.config.mdd_limit * 100.0),
                kelly_fraction: 0.0,
                position_size: 0.0,
                expected_value: 0.0,
            };
        }

        // Check 4: Compute Kelly position size
        let (size, ev) = kelly::compute_position_size(request, &self.config, self.bankroll);

        // Check 5: Exposure limit
        if self.current_exposure + size > self.config.max_total_exposure {
            return RiskVerdict {
                approved: false,
                reason: format!("Exposure ${:.0} + ${:.0} exceeds max ${:.0}",
                    self.current_exposure, size, self.config.max_total_exposure),
                kelly_fraction: 0.0,
                position_size: 0.0,
                expected_value: 0.0,
            };
        }

        let conviction = (request.p_model - request.p_market).abs().min(0.3);
        let size_fraction = self.config.kelly_fraction * (conviction / 0.3);

        RiskVerdict {
            approved: true,
            reason: "All risk checks passed".into(),
            kelly_fraction: size_fraction,
            position_size: size,
            expected_value: ev,
        }
    }

    /// Update state when a new position is opened.
    /// Must be called after successful order execution to keep the engine in sync.
    pub fn update_position_opened(&mut self, size: f64) {
        self.current_exposure += size;
        self.open_positions += 1;
    }

    /// Update state when a position is closed.
    /// `size`: the original position size in USD.
    /// `pnl`: profit or loss from the position (positive = profit, negative = loss).
    /// Also updates peak equity if the close pushes equity to a new high.
    pub fn update_position_closed(&mut self, size: f64, pnl: f64) {
        self.current_exposure = (self.current_exposure - size).max(0.0);
        self.open_positions = self.open_positions.saturating_sub(1);
        self.current_equity += pnl;
        if self.current_equity > self.peak_equity {
            self.peak_equity = self.current_equity;
        }
    }

    /// Reset peak/current equity to a new baseline (e.g., at session start or daily reset).
    /// This prevents historical drawdown from permanently blocking new trades after
    /// the bot restarts. Without this, a previous session's drawdown would carry over
    /// and potentially trip the circuit breaker immediately.
    pub fn reset_session_equity(&mut self, equity: f64) {
        self.peak_equity = equity;
        self.current_equity = equity;
    }

    /// Read-only access to the current risk configuration.
    pub fn config(&self) -> &RiskConfig { &self.config }

    /// Hot-swap the risk configuration (e.g., via API endpoint).
    /// Takes effect on the next `evaluate()` call.
    pub fn update_config(&mut self, config: RiskConfig) { self.config = config; }
}

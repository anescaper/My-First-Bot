//! Composable risk gate chain — evaluates a trading signal through an ordered
//! sequence of risk gates.
//!
//! The chain executes gates in ascending priority order (lower number = runs first).
//! Evaluation follows these rules:
//!
//! 1. **Reject**: First rejection immediately stops the chain and returns the rejection.
//! 2. **Resize**: Multipliers accumulate multiplicatively. If the cumulative multiplier
//!    drops below 0.1 (10%), the signal is auto-rejected as "too small to trade".
//! 3. **Pass**: Gate is satisfied, continue to next gate.
//! 4. If all gates pass/resize, the final result is either Pass (multiplier = 1.0)
//!    or Resize (with accumulated multiplier < 1.0).
//!
//! Disabled gates (`is_enabled() = false`) are skipped entirely.

use serde::Serialize;

use crate::gate::{GateResult, GateSignal, RiskGate};

/// Serializable summary of a single gate's configuration, for API introspection.
/// Returned by `RiskGateChain::summary()` to expose the active risk configuration.
#[derive(Debug, Clone, Serialize)]
pub struct GateSummary {
    /// Human-readable gate name (e.g. "min-edge", "max-exposure").
    pub name: String,
    /// Execution priority (lower = runs first).
    pub priority: u32,
    /// Whether this gate is currently active.
    pub enabled: bool,
}

/// Ordered chain of risk gates that evaluates trading signals.
///
/// Gates are stored sorted by priority. Adding a gate re-sorts the chain,
/// so insertion order doesn't matter — only the `priority()` value.
///
/// The chain is the primary interface for risk evaluation in the trading pipeline.
/// Each strategy profile builds its own chain via `build_risk_chain()`.
///
/// Thread safety: `RiskGateChain` is `Send + Sync` because all gates implement
/// `Send + Sync` (via the `RiskGate` trait bound). However, the chain itself is
/// not shared — each pipeline owns its own instance.
pub struct RiskGateChain {
    /// Gates sorted by priority (ascending). First Reject stops evaluation.
    gates: Vec<Box<dyn RiskGate>>,
}

impl RiskGateChain {
    /// Create an empty gate chain. Use `.add()` to populate it.
    pub fn new() -> Self {
        Self { gates: Vec::new() }
    }

    /// Add a gate to the chain. Re-sorts by priority after insertion.
    /// Uses builder pattern (returns `self`) for ergonomic chaining:
    /// `RiskGateChain::new().add(gate1).add(gate2).add(gate3)`
    pub fn add(mut self, gate: impl RiskGate + 'static) -> Self {
        self.gates.push(Box::new(gate));
        self.gates.sort_by_key(|g| g.priority());
        self
    }

    /// Run all enabled gates in priority order and return the final decision.
    ///
    /// - First `Reject` stops the chain immediately.
    /// - `Resize` multipliers accumulate multiplicatively. Auto-rejects if < 0.1.
    /// - If all gates pass, returns `Pass` or `Resize` with the accumulated multiplier.
    pub fn evaluate(&self, signal: &GateSignal) -> GateResult {
        let mut cumulative_multiplier = 1.0_f64;
        let mut resize_reasons = Vec::new();

        for gate in &self.gates {
            if !gate.is_enabled() {
                continue;
            }

            match gate.evaluate(signal) {
                GateResult::Pass => continue,
                GateResult::Reject(reason) => {
                    tracing::info!(gate = gate.name(), %reason, "Signal rejected");
                    return GateResult::Reject(reason);
                }
                GateResult::Resize { multiplier, reason } => {
                    cumulative_multiplier *= multiplier;
                    resize_reasons.push(format!("{}: {}", gate.name(), reason));
                    if cumulative_multiplier < 0.1 {
                        return GateResult::Reject(format!(
                            "Cumulative resize {:.0}% too small: {}",
                            cumulative_multiplier * 100.0,
                            resize_reasons.join("; "),
                        ));
                    }
                }
            }
        }

        if cumulative_multiplier < 1.0 {
            GateResult::Resize {
                multiplier: cumulative_multiplier,
                reason: resize_reasons.join("; "),
            }
        } else {
            GateResult::Pass
        }
    }

    /// List all gates with their status, for API introspection endpoints.
    /// Returns name, priority, and enabled status for each gate in chain order.
    pub fn summary(&self) -> Vec<GateSummary> {
        self.gates
            .iter()
            .map(|g| GateSummary {
                name: g.name().to_string(),
                priority: g.priority(),
                enabled: g.is_enabled(),
            })
            .collect()
    }
}

impl Default for RiskGateChain {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::{GateResult, GateSignal, RiskGate};

    struct AlwaysPassGate;
    impl RiskGate for AlwaysPassGate {
        fn name(&self) -> &str { "always-pass" }
        fn evaluate(&self, _: &GateSignal) -> GateResult { GateResult::Pass }
        fn priority(&self) -> u32 { 10 }
    }

    struct AlwaysRejectGate;
    impl RiskGate for AlwaysRejectGate {
        fn name(&self) -> &str { "always-reject" }
        fn evaluate(&self, _: &GateSignal) -> GateResult {
            GateResult::Reject("test rejection".into())
        }
        fn priority(&self) -> u32 { 20 }
    }

    struct HalfSizeGate;
    impl RiskGate for HalfSizeGate {
        fn name(&self) -> &str { "half-size" }
        fn evaluate(&self, _: &GateSignal) -> GateResult {
            GateResult::Resize { multiplier: 0.5, reason: "vol high".into() }
        }
        fn priority(&self) -> u32 { 30 }
    }

    fn test_signal() -> GateSignal {
        GateSignal {
            market_id: "test".into(),
            asset: "ETH".into(),
            timeframe: "15m".into(),
            direction: "Up".into(),
            p_up: 0.6,
            confidence: 0.3,
            edge: 0.08,
            entry_price: 0.52,
            proposed_size_usd: 50.0,
            current_total_exposure: 100.0,
            current_drawdown_pct: 0.02,
            current_position_count: 3,
            bankroll: 500.0,
            current_net_exposure: 0.0,
            current_volatility: 0.0,
            avg_volatility: 0.0,
            market_p_up: 0.52,
            market_p_down: 0.48,
            current_asset_positions: 0,
            timeframe_agreement: 1.0,
            same_direction_count: 0,
            same_direction_exposure: 0.0,
        }
    }

    #[test]
    fn test_empty_chain_passes() {
        let chain = RiskGateChain::new();
        assert!(matches!(chain.evaluate(&test_signal()), GateResult::Pass));
    }

    #[test]
    fn test_chain_rejects_on_first_reject() {
        let chain = RiskGateChain::new()
            .add(AlwaysPassGate)
            .add(AlwaysRejectGate)
            .add(HalfSizeGate);
        assert!(matches!(chain.evaluate(&test_signal()), GateResult::Reject(_)));
    }

    #[test]
    fn test_chain_accumulates_resize() {
        let chain = RiskGateChain::new()
            .add(AlwaysPassGate)
            .add(HalfSizeGate);
        match chain.evaluate(&test_signal()) {
            GateResult::Resize { multiplier, .. } => {
                assert!((multiplier - 0.5).abs() < 1e-10);
            }
            other => panic!("Expected Resize, got {:?}", other),
        }
    }

    #[test]
    fn test_chain_auto_rejects_tiny_resize() {
        // Two HalfSizeGates = 0.25, add a third = 0.125, fourth = 0.0625 < 0.1
        let chain = RiskGateChain::new()
            .add(HalfSizeGate)
            .add(HalfSizeGate)
            .add(HalfSizeGate)
            .add(HalfSizeGate);
        assert!(matches!(chain.evaluate(&test_signal()), GateResult::Reject(_)));
    }
}

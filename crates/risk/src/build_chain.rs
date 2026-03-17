//! Pre-built risk gate chains for common trading profiles.
//!
//! Each strategy profile (e.g. "garch-t-aggressive", "bilateral-mm") needs
//! a different combination of risk gates with different thresholds. This module
//! provides factory functions that build the appropriate chain for a given profile.
//!
//! Configuration is read from environment variables with sensible defaults.
//! The defaults are tuned for conservative paper-trading on Polymarket crypto
//! Up/Down rounds based on Q6/Q7/Q10/Q14/Q15 research findings.

use crate::chain::RiskGateChain;
use crate::gates::*;

/// Configuration for the risk gate chain, populated from environment variables.
///
/// All monetary values are in USD. Percentage values are decimals (e.g. 0.25 = 25%).
/// Each field maps to a specific env var, with hardcoded defaults that represent
/// conservative baseline settings for paper trading.
#[derive(Debug, Clone)]
pub struct RiskChainConfig {
    /// Minimum edge (our_p - market_p) required to enter a trade.
    /// Env: `CRYPTO_EDGE_THRESHOLD`, default: 0.12 (12%).
    /// High default because our model is uncalibrated — we need large perceived
    /// edge to compensate for model error.
    pub min_edge: f64,
    /// Maximum total portfolio exposure in USD.
    /// Computed as: bankroll * MAX_EXPOSURE_PCT (env, default 2.0).
    /// The 2x multiplier allows leveraged exposure in paper mode.
    pub max_exposure: f64,
    /// Base fraction of bankroll for position sizing (confidence-scaled Kelly).
    /// Env: `KELLY_FRACTION`, default: 0.25 (25%).
    /// This is the maximum single-position fraction at full confidence.
    pub kelly_fraction: f64,
    /// Maximum drawdown before the circuit breaker trips.
    /// Env: `MAX_DRAWDOWN_PCT`, default: 0.25 (25%).
    /// Generous in paper mode; tighten for live trading.
    pub max_drawdown_pct: f64,
    /// Maximum number of concurrent open positions.
    /// Env: `MAX_POSITIONS`, default: 10.
    pub max_positions: usize,
    /// Minimum acceptable entry price for binary tokens.
    /// Env: `MIN_ENTRY_PRICE`, default: 0.12.
    /// Below this, outcomes are too unlikely for reliable prediction.
    pub min_entry_price: f64,
    /// Maximum acceptable entry price for binary tokens.
    /// Env: `MAX_ENTRY_PRICE`, default: 0.88.
    /// Above this, payout is too small to justify fee + slippage costs.
    pub max_entry_price: f64,
}

impl RiskChainConfig {
    /// Read risk configuration from environment variables, falling back to defaults.
    /// `bankroll`: current bankroll in USD (used to compute max_exposure).
    pub fn from_env(bankroll: f64) -> Self {
        let max_exposure_pct: f64 = std::env::var("MAX_EXPOSURE_PCT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2.0);

        Self {
            min_edge: std::env::var("CRYPTO_EDGE_THRESHOLD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.12),
            max_exposure: bankroll * max_exposure_pct,
            kelly_fraction: std::env::var("KELLY_FRACTION")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.25),
            max_drawdown_pct: std::env::var("MAX_DRAWDOWN_PCT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.25),
            max_positions: std::env::var("MAX_POSITIONS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10),
            min_entry_price: std::env::var("MIN_ENTRY_PRICE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.12),
            max_entry_price: std::env::var("MAX_ENTRY_PRICE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.88),
        }
    }
}

/// Build a risk gate chain for the given strategy profile, reading config from env vars.
///
/// This is the primary entry point for pipeline initialization.
/// `profile`: strategy name (e.g. "garch-t-aggressive", "bilateral-mm", "default").
/// `bankroll`: current bankroll in USD.
pub fn build_risk_chain(profile: &str, bankroll: f64) -> RiskGateChain {
    let config = RiskChainConfig::from_env(bankroll);
    build_risk_chain_with_config(profile, &config)
}

/// Build a risk gate chain for the given strategy profile with explicit config.
///
/// Profile-specific gate configurations:
///
/// - **"garch-t-aggressive" / "aggressive"**: Looser thresholds — 70% of min_edge,
///   150% of max_exposure, 140% of Kelly fraction. For strategies with strong
///   statistical signals where we want more aggressive sizing.
///
/// - **"stochastic-vol" / "lmsr-filter"**: Default chain + VolatilityGate(2.0).
///   Adds volatility awareness for strategies that model stochastic volatility.
///
/// - **"bilateral-mm"**: Market-making profile — no MinEdgeGate (earns spread,
///   not directional edge), no KellySizeGate (p_up=0.5 would always reject),
///   adds SpreadViabilityGate, NetExposureGate(30%), AdverseSelectionGate.
///   Higher concurrent position limit (3) since MM holds both sides.
///
/// - **Default**: Standard chain with all core gates. Used by most profiles
///   including "control", "monte-carlo", "factor-model", etc.
///
/// Direction exposure limits (MAX_SAME_DIRECTION=3, MAX_SAME_DIRECTION_PCT=60%)
/// are read from env vars and applied to all non-bilateral profiles.
pub fn build_risk_chain_with_config(profile: &str, config: &RiskChainConfig) -> RiskGateChain {
    let max_same_dir: usize = std::env::var("MAX_SAME_DIRECTION")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let max_same_dir_pct: f64 = std::env::var("MAX_SAME_DIRECTION_PCT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.6);

    let default_chain = || {
        RiskGateChain::new()
            .add(MaxDrawdownGate::new(config.max_drawdown_pct))
            .add(MaxPositionsGate::new(config.max_positions))
            .add(EntryPriceGate::new(config.min_entry_price, config.max_entry_price))
            .add(MinEdgeGate::new(config.min_edge))
            .add(MaxExposureGate::new(config.max_exposure))
            .add(DirectionExposureGate::new(max_same_dir, max_same_dir_pct))
            .add(KellySizeGate::new(config.kelly_fraction))
            .add(JumpDetectionGate::new(3.0))
            .add(ConcurrentPositionGate::new(2))
    };

    match profile {
        "garch-t-aggressive" | "aggressive" => RiskGateChain::new()
            .add(MaxDrawdownGate::new(config.max_drawdown_pct))
            .add(MaxPositionsGate::new(config.max_positions))
            .add(EntryPriceGate::new(config.min_entry_price, config.max_entry_price))
            .add(MinEdgeGate::new(config.min_edge * 0.7)) // lower edge threshold
            .add(MaxExposureGate::new(config.max_exposure * 1.5))
            .add(DirectionExposureGate::new(max_same_dir, max_same_dir_pct))
            .add(KellySizeGate::new(config.kelly_fraction * 1.4))
            .add(JumpDetectionGate::new(3.0))
            .add(ConcurrentPositionGate::new(2)),
        "stochastic-vol" | "lmsr-filter" => default_chain()
            .add(VolatilityGate::new(2.0)),
        "bilateral-mm" => RiskGateChain::new()
            .add(SpreadViabilityGate::new())
            .add(NetExposureGate::new(0.3))
            .add(AdverseSelectionGate::new())
            .add(MaxDrawdownGate::new(config.max_drawdown_pct))
            .add(MaxPositionsGate::new(config.max_positions))
            .add(MaxExposureGate::new(config.max_exposure))
            // No MinEdgeGate — bilateral earns spread, not directional edge
            // No KellySizeGate — bilateral has p_up=0.5, Kelly always rejects neutral signals
            .add(ConcurrentPositionGate::new(3)),
        _ => default_chain(),
    }
}

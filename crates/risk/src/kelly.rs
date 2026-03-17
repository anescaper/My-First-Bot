//! Position sizing and risk metrics.
//!
//! Despite the module name "kelly", this module uses **flat confidence-based sizing**
//! rather than true Kelly Criterion. Kelly requires calibrated probabilities
//! (Pr[win] = p), but our model outputs are uncalibrated directional signals.
//! Feeding uncalibrated values into Kelly amplifies errors — overbet on confident
//! wrong signals, underbet on uncertain correct ones.
//!
//! The sizing formula: `kelly_fraction * conviction_scale * bankroll`, where
//! conviction_scale = min(|p_model - p_market|, 0.3) / 0.3, capped at
//! `max_exposure_per_position`.
//!
//! The 0.3 conviction cap prevents extreme sizing when the model strongly
//! disagrees with the market — a safeguard against model hallucination.

use crate::types::{RiskConfig, PositionRequest};

/// Compute position size using flat confidence-based sizing.
///
/// Returns `(size_usd, expected_value_usd)`.
///
/// `request`: The position request containing model and market probabilities.
/// `config`: Risk configuration with kelly_fraction and max_exposure_per_position.
/// `bankroll`: Current bankroll in USD.
///
/// Conviction is capped at 0.3 (hardcoded) because larger disagreements between
/// our model and the market are more likely to indicate model error than genuine
/// alpha. The 0.3 cap means maximum sizing occurs when |p_model - p_market| >= 0.3,
/// which represents a 30-cent edge — already extremely confident.
pub fn compute_position_size(
    request: &PositionRequest,
    config: &RiskConfig,
    bankroll: f64,
) -> (f64, f64) {
    // Conviction: how far p_model deviates from the market price.
    // Capped at 0.3 to prevent oversizing on extreme model disagreements.
    let conviction = (request.p_model - request.p_market).abs().min(0.3);
    // Normalized to [0, 1] range for use as a scaling factor.
    let conviction_scale = conviction / 0.3;

    let size_fraction = config.kelly_fraction * conviction_scale;
    let size = (bankroll * size_fraction).min(config.max_exposure_per_position);
    let ev = request.edge * size;

    (size, ev)
}

/// Expected value of a position: (model_probability - market_probability) * size.
///
/// Positive EV means our model thinks the position is underpriced by the market.
/// This is a simplified EV calculation that assumes linear payoff, which is
/// approximately correct for binary option markets near fair value.
pub fn expected_value(p_model: f64, p_market: f64, size: f64) -> f64 {
    (p_model - p_market) * size
}

/// Value at Risk at 95% confidence level using parametric (normal) method.
///
/// VaR_95 = mean - 1.645 * std, where 1.645 is the z-score for 5% left tail
/// of the standard normal distribution. Returns the loss threshold that is
/// exceeded only 5% of the time under normal market conditions.
///
/// `mean_return`: expected return (can be negative).
/// `std_return`: standard deviation of returns.
pub fn var_95(mean_return: f64, std_return: f64) -> f64 {
    mean_return - 1.645 * std_return
}

/// Compute maximum drawdown from a time series of equity values.
///
/// Maximum drawdown = max((peak - trough) / peak) over all time.
/// Returns 0.0 for empty equity series.
///
/// Used by the RiskEngine to track portfolio health and trigger the
/// circuit breaker when drawdown exceeds the configured limit.
pub fn max_drawdown(equity: &[f64]) -> f64 {
    if equity.is_empty() { return 0.0; }
    let mut peak = equity[0];
    let mut max_dd = 0.0;
    for &val in equity {
        if val > peak { peak = val; }
        let dd = (peak - val) / peak;
        if dd > max_dd { max_dd = dd; }
    }
    max_dd
}

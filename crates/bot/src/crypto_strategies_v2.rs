//! Quantitative crypto strategies — drop-in replacement for CryptoComposer
//!
//! Uses GARCH(1,1), Hurst exponent, Student-t CDF, and jump detection
//! from the `quant` module instead of hand-rolled approximations.

use crate::crypto_strategies::{
    ComposedCryptoPrediction, CryptoPrediction, CryptoStrategyConfig, PredictionContext,
};
use polybot_scanner::crypto::{Asset, Timeframe};
use polybot_scanner::price_feed::Candle;
use std::collections::HashMap;

/// Strategy profiles that determine which mathematical models each strategy slot uses.
///
/// Each profile swaps in different implementations for the 5 strategy slots.
/// The baseline uses all-original Rust implementations from `crypto_strategies.rs`;
/// these profiles upgrade specific slots with more sophisticated quant methods.
///
/// Profile selection is via the `STRATEGY_PROFILE` env var.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum QuantProfile {
    /// Upgrades time_decay_lock to use GARCH(1,1) volatility + Student-t CDF.
    /// Other strategies use original implementations.
    GarchT,
    /// Upgrades vol_regime to use Hurst exponent, and candle_trend to weighted
    /// linear regression. time_decay_lock stays original (normal CDF).
    HurstHinf,
    /// Full quant mode: GARCH+Student-t + Hurst + regression + jump detection gate.
    /// Rounds with detected price jumps (>3 sigma) are skipped entirely.
    FullQuant,
}

impl QuantProfile {
    /// Parse a profile name from the STRATEGY_PROFILE env var.
    /// Returns None for unrecognized profiles (which means no quant composer is created).
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "garch-t" => Some(Self::GarchT),
            "hurst-hinf" => Some(Self::HurstHinf),
            "full-quant" => Some(Self::FullQuant),
            _ => None,
        }
    }
}

/// Drop-in replacement for CryptoComposer that uses profile-dependent math.
///
/// Same interface as CryptoComposer::predict() — the pipeline can swap between them.
/// Uses function pointers instead of trait objects for strategy dispatch,
/// which avoids trait object overhead and simplifies profile-based routing.
pub struct QuantCryptoComposer {
    /// Which mathematical models to use for each strategy slot
    profile: QuantProfile,
    /// Runtime-configurable strategy weights and enabled state
    configs: Vec<CryptoStrategyConfig>,
    /// Minimum ensemble confidence to produce a signal. Hardcoded at 0.15.
    min_confidence: f64,
}

impl QuantCryptoComposer {
    pub fn new(profile: QuantProfile) -> Self {
        Self {
            profile,
            configs: CryptoStrategyConfig::defaults(),
            min_confidence: 0.15,
        }
    }

    pub fn update_configs(&mut self, new_configs: Vec<CryptoStrategyConfig>) {
        self.configs = new_configs;
    }

    /// Main prediction function — same interface as CryptoComposer::predict.
    ///
    /// For FullQuant profile, first checks for price jumps (>3 sigma) using the
    /// quant::jump module. If jumps are detected, returns None — jumping markets
    /// violate the Gaussian assumptions that all strategies rely on.
    ///
    /// Dispatches to profile-specific strategy functions via a function pointer table.
    /// Each function returns Option<(p_up, confidence)>; the composer aggregates them
    /// using the same confidence-weighted voting as CryptoComposer.
    pub fn predict(&self, ctx: &PredictionContext) -> Option<ComposedCryptoPrediction> {
        // Jump detection gate: full-quant profile bails on jumpy data
        if self.profile == QuantProfile::FullQuant {
            let returns = log_returns(&ctx.candles);
            if returns.len() >= 3 && crate::quant::jump::has_jumps(&returns, 3.0) {
                return None;
            }
        }

        // Dispatch strategies based on profile
        let strategy_fns: Vec<(&str, fn(&PredictionContext) -> Option<(f64, f64)>)> = match self.profile {
            QuantProfile::GarchT => vec![
                ("time_decay_lock", time_decay_lock_garch_t),
                ("candle_trend", candle_trend_original),
                ("vol_regime", vol_regime_original),
                ("volume_breakout", volume_breakout),
                ("cross_asset_momentum", cross_asset_momentum),
            ],
            QuantProfile::HurstHinf => vec![
                ("time_decay_lock", time_decay_lock_original),
                ("candle_trend", candle_trend_regression),
                ("vol_regime", vol_regime_hurst),
                ("volume_breakout", volume_breakout),
                ("cross_asset_momentum", cross_asset_momentum),
            ],
            QuantProfile::FullQuant => vec![
                ("time_decay_lock", time_decay_lock_garch_t),
                ("candle_trend", candle_trend_regression),
                ("vol_regime", vol_regime_hurst),
                ("volume_breakout", volume_breakout),
                ("cross_asset_momentum", cross_asset_momentum),
            ],
        };

        let mut components = Vec::new();
        let mut weighted_sum = 0.0;
        let mut total_weight = 0.0;

        for (name, strategy_fn) in &strategy_fns {
            let config = match self.configs.iter().find(|c| c.name == *name) {
                Some(c) if c.enabled => c,
                _ => continue,
            };

            if let Some((p_up, confidence)) = strategy_fn(ctx) {
                let w = config.weight * confidence;
                weighted_sum += p_up * w;
                total_weight += w;
                components.push(CryptoPrediction {
                    strategy_name: name.to_string(),
                    p_up,
                    confidence,
                    reasoning: String::new(),
                });
            }
        }

        if total_weight == 0.0 || components.is_empty() {
            return None;
        }

        let p_up = (weighted_sum / total_weight).clamp(0.01, 0.99);

        // Agreement-based confidence — identical to original CryptoComposer
        let up_count = components.iter().filter(|c| c.p_up > 0.5).count();
        let down_count = components.len() - up_count;
        let agreement = up_count.max(down_count) as f64 / components.len() as f64;

        let avg_confidence =
            components.iter().map(|c| c.confidence).sum::<f64>() / components.len() as f64;
        let mut ensemble_confidence = avg_confidence * agreement;

        // Cross-timeframe agreement scaling
        if let Some(ref intel) = ctx.timeframe_intel {
            let dir_is_up = p_up > 0.5;
            let tf_agreement = intel.direction_agreement(dir_is_up);
            // Range [0.75, 1.15]: boost on agreement, reduce on disagreement
            let tf_multiplier = 0.75 + 0.4 * tf_agreement;
            ensemble_confidence *= tf_multiplier;
        }

        // Minimum confidence gate
        if ensemble_confidence < self.min_confidence {
            return None;
        }

        let market_p_up = ctx.round.price_up;
        // Signal strength: confidence × conviction (not a true probability edge)
        let edge = ensemble_confidence * (p_up - 0.5).abs() * 2.0;
        let direction = if p_up > market_p_up { "Up" } else { "Down" };

        Some(ComposedCryptoPrediction {
            p_up,
            confidence: ensemble_confidence,
            edge,
            direction: direction.to_string(),
            components,
            hold_to_resolution: false,
            min_progress: None,
            max_progress: None,
            max_entry_price: None,
        })
    }
}

// ============================================================
// Helpers
// ============================================================

/// Compute log-returns from candle close prices
fn log_returns(candles: &[Candle]) -> Vec<f64> {
    candles
        .windows(2)
        .filter_map(|w| {
            if w[0].close <= 0.0 || w[1].close <= 0.0 {
                return None;
            }
            Some((w[1].close / w[0].close).ln())
        })
        .collect()
}

/// Abramowitz & Stegun approximation of the standard normal CDF
fn normal_cdf(x: f64) -> f64 {
    const A1: f64 = 0.254829592;
    const A2: f64 = -0.284496736;
    const A3: f64 = 1.421413741;
    const A4: f64 = -1.453152027;
    const A5: f64 = 1.061405429;
    const P: f64 = 0.3275911;

    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x = x.abs() / std::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + P * x);
    let y = 1.0 - (((((A5 * t + A4) * t) + A3) * t + A2) * t + A1) * t * (-x * x).exp();
    0.5 * (1.0 + sign * y)
}

// ============================================================
// GARCH-T profile strategies
// ============================================================

/// GARCH(1,1) volatility + Student-t CDF for p_up.
///
/// Upgrade over the original TimeDecayLock: uses GARCH(1,1) to model
/// volatility clustering (high-vol periods persist) instead of simple sample std dev.
/// Uses Student-t CDF instead of normal CDF for fatter tails (more realistic for crypto).
///
/// The GARCH forecast accounts for the current volatility regime, not just the average.
/// Student-t degrees of freedom are estimated from the return distribution's kurtosis.
fn time_decay_lock_garch_t(ctx: &PredictionContext) -> Option<(f64, f64)> {
    let progress = ctx.round.progress_pct();
    if progress < 0.5 {
        return None;
    }
    if ctx.reference_price <= 0.0 || ctx.current_price <= 0.0 {
        return None;
    }
    if ctx.candles.len() < 3 {
        return None;
    }

    let returns = log_returns(&ctx.candles);
    if returns.is_empty() {
        return None;
    }

    // Fit GARCH(1,1) and forecast 1-step vol
    let model = crate::quant::garch::Garch11::fit(&returns);
    let expected_vol_base = model
        .forecast_vol(1)
        .first()
        .copied()
        .unwrap_or(model.volatility());

    if expected_vol_base < 1e-10 {
        return None;
    }

    // Scale by sqrt(remaining_time / candle_interval) like original
    let secs_remaining = ctx.round.seconds_remaining().max(1) as f64;
    let candle_interval = ctx.round.timeframe.seconds() as f64 / 20.0;
    let expected_vol = expected_vol_base * (secs_remaining / candle_interval.max(1.0)).sqrt();
    if expected_vol < 1e-10 {
        return None;
    }

    // Price displacement as log return
    let displacement = (ctx.current_price / ctx.reference_price).ln();
    let z = displacement / expected_vol;

    // Student-t CDF instead of normal CDF
    let df = crate::quant::student_t::estimate_df(&returns);
    let p_up = crate::quant::student_t::student_t_cdf(z, df).clamp(0.05, 0.95);

    let confidence = progress.powi(2) * (z.abs() / 2.0).min(1.0);

    Some((p_up, confidence.clamp(0.05, 0.9)))
}

// ============================================================
// Hurst-HInf profile strategies
// ============================================================

/// Hurst exponent for regime classification instead of Kaufman Efficiency Ratio.
///
/// H > 0.55: trending (persistent) — follow momentum
/// H < 0.45: mean-reverting (anti-persistent) — fade the move
/// H = 0.45-0.55: ambiguous (random walk) — skip
///
/// Hurst exponent is a more theoretically grounded measure of persistence than ER.
/// Computed via rescaled range (R/S) analysis from the quant::hurst module.
///
/// Hardcoded thresholds 0.55/0.45 provide a buffer around H=0.5 (pure random walk).
fn vol_regime_hurst(ctx: &PredictionContext) -> Option<(f64, f64)> {
    if ctx.candles.len() < 10 {
        return None;
    }

    let close_prices: Vec<f64> = ctx.candles.iter().map(|c| c.close).collect();
    let h = crate::quant::hurst::hurst_exponent(&close_prices);

    // Momentum direction from last few candles
    let returns = log_returns(&ctx.candles);
    if returns.is_empty() {
        return None;
    }
    let recent_mean = returns[returns.len().saturating_sub(5)..]
        .iter()
        .sum::<f64>()
        / returns[returns.len().saturating_sub(5)..].len() as f64;
    let momentum_sign = recent_mean.signum();

    if h > 0.55 {
        // Trending: follow momentum
        let p_up = (0.5 + momentum_sign * (h - 0.5) * 0.7).clamp(0.1, 0.9);
        let confidence = ((h - 0.5) * 2.0).clamp(0.1, 0.7);
        Some((p_up, confidence))
    } else if h < 0.45 {
        // Mean-reverting: fade the move
        let p_up = (0.5 - momentum_sign * (0.5 - h) * 0.5).clamp(0.1, 0.9);
        let confidence = ((0.5 - h) * 1.5).clamp(0.05, 0.5);
        Some((p_up, confidence))
    } else {
        // Ambiguous: skip
        None
    }
}

/// Weighted linear regression on close prices instead of EMA.
///
/// Uses linearly increasing weights (w_i = i+1) so recent candles matter more.
/// The normalized slope (slope / mean_price) gives a percentage rate of change.
/// R-squared of the regression serves as confidence (goodness of fit = how clean the trend is).
///
/// More robust than EMA: a single outlier candle won't dominate the signal.
fn candle_trend_regression(ctx: &PredictionContext) -> Option<(f64, f64)> {
    if ctx.candles.len() < 5 {
        return None;
    }

    let closes: Vec<f64> = ctx.candles.iter().map(|c| c.close).collect();
    let n = closes.len();

    // Weighted linear regression: weight_i = i+1 (more weight on recent)
    let mut sum_w = 0.0;
    let mut sum_wx = 0.0;
    let mut sum_wy = 0.0;
    let mut sum_wxx = 0.0;
    let mut sum_wxy = 0.0;
    for (i, &y) in closes.iter().enumerate() {
        let x = i as f64;
        let w = (i + 1) as f64; // linearly increasing weight
        sum_w += w;
        sum_wx += w * x;
        sum_wy += w * y;
        sum_wxx += w * x * x;
        sum_wxy += w * x * y;
    }

    let denom = sum_w * sum_wxx - sum_wx * sum_wx;
    if denom.abs() < 1e-20 {
        return None;
    }

    let slope = (sum_w * sum_wxy - sum_wx * sum_wy) / denom;
    let intercept = (sum_wy - slope * sum_wx) / sum_w;

    // Normalize slope by mean price to get percentage rate of change
    let mean_price = closes.iter().sum::<f64>() / n as f64;
    if mean_price.abs() < 1e-10 {
        return None;
    }
    let normalized_slope = slope / mean_price;

    // Timeframe-dependent k-factor (same as original)
    let k = match ctx.round.timeframe {
        Timeframe::FiveMin => 8.0,
        Timeframe::FifteenMin => 6.0,
        Timeframe::OneHour => 4.0,
        Timeframe::OneDay => 2.5,
    };

    let signal = (normalized_slope * k).clamp(-1.0, 1.0);
    let p_up = (0.5 + signal * 0.4).clamp(0.05, 0.95);

    // R-squared of the regression for confidence
    let y_mean = sum_wy / sum_w;
    let mut ss_res = 0.0;
    let mut ss_tot = 0.0;
    for (i, &y) in closes.iter().enumerate() {
        let x = i as f64;
        let w = (i + 1) as f64;
        let y_hat = intercept + slope * x;
        ss_res += w * (y - y_hat).powi(2);
        ss_tot += w * (y - y_mean).powi(2);
    }

    let r_squared = if ss_tot > 1e-20 {
        (1.0 - ss_res / ss_tot).clamp(0.0, 1.0)
    } else {
        0.0
    };

    let confidence = r_squared.sqrt().clamp(0.05, 0.8);

    Some((p_up, confidence))
}

// ============================================================
// Original strategy implementations (copied as standalone fns)
// ============================================================

/// Original time_decay_lock using normal CDF — used by hurst-hinf profile
fn time_decay_lock_original(ctx: &PredictionContext) -> Option<(f64, f64)> {
    let progress = ctx.round.progress_pct();
    if progress < 0.5 {
        return None;
    }
    if ctx.reference_price <= 0.0 || ctx.current_price <= 0.0 {
        return None;
    }
    if ctx.candles.len() < 3 {
        return None;
    }

    let returns = log_returns(&ctx.candles);
    if returns.is_empty() {
        return None;
    }

    let mean = returns.iter().sum::<f64>() / returns.len() as f64;
    let variance =
        returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (returns.len() as f64).max(1.0);
    let candle_sigma = variance.sqrt();
    if candle_sigma < 1e-10 {
        return None;
    }

    let secs_remaining = ctx.round.seconds_remaining().max(1) as f64;
    let candle_interval = ctx.round.timeframe.seconds() as f64 / 20.0;
    let expected_vol = candle_sigma * (secs_remaining / candle_interval.max(1.0)).sqrt();
    if expected_vol < 1e-10 {
        return None;
    }

    let displacement = (ctx.current_price / ctx.reference_price).ln();
    let z = displacement / expected_vol;

    let p_up = normal_cdf(z).clamp(0.05, 0.95);
    let confidence = progress.powi(2) * (z.abs() / 2.0).min(1.0);

    Some((p_up, confidence.clamp(0.05, 0.9)))
}

/// Original candle_trend using EMA — used by garch-t profile
fn candle_trend_original(ctx: &PredictionContext) -> Option<(f64, f64)> {
    if ctx.candles.len() < 5 {
        return None;
    }

    let returns = log_returns(&ctx.candles);
    if returns.len() < 4 {
        return None;
    }

    let k = match ctx.round.timeframe {
        Timeframe::FiveMin => 8.0,
        Timeframe::FifteenMin => 6.0,
        Timeframe::OneHour => 4.0,
        Timeframe::OneDay => 2.5,
    };

    let alpha = 2.0 / (returns.len() as f64 + 1.0);
    let mut ema = returns[0];
    for &r in &returns[1..] {
        ema = alpha * r + (1.0 - alpha) * ema;
    }

    let recent = &ctx.candles[ctx.candles.len().saturating_sub(5)..];
    let bullish_count = recent.iter().filter(|c| c.close > c.open).count();
    let bearish_count = recent.iter().filter(|c| c.close < c.open).count();
    let consistency = (bullish_count.max(bearish_count) as f64) / recent.len() as f64;

    let mean_r = returns.iter().sum::<f64>() / returns.len() as f64;
    let hist_vol = (returns
        .iter()
        .map(|r| (r - mean_r).powi(2))
        .sum::<f64>()
        / returns.len() as f64)
        .sqrt();
    let magnitude = if hist_vol > 1e-10 {
        (ema.abs() / hist_vol).min(2.0)
    } else {
        0.0
    };

    let signal = (ema * k).clamp(-1.0, 1.0);
    let p_up = (0.5 + signal * 0.4).clamp(0.05, 0.95);
    let confidence = consistency * magnitude.min(1.0);

    Some((p_up, confidence.clamp(0.05, 0.8)))
}

/// Original vol_regime using Kaufman ER — used by garch-t profile
fn vol_regime_original(ctx: &PredictionContext) -> Option<(f64, f64)> {
    if ctx.candles.len() < 10 {
        return None;
    }

    let n = ctx.candles.len();
    let net_move = (ctx.candles[n - 1].close - ctx.candles[0].close).abs();
    let volatility_path: f64 = ctx
        .candles
        .windows(2)
        .map(|w| (w[1].close - w[0].close).abs())
        .sum();

    if volatility_path < 1e-10 {
        return None;
    }
    let er = net_move / volatility_path;

    let returns = log_returns(&ctx.candles);
    if returns.is_empty() {
        return None;
    }
    let recent_mean = returns[returns.len().saturating_sub(5)..]
        .iter()
        .sum::<f64>()
        / returns[returns.len().saturating_sub(5)..].len() as f64;
    let momentum = recent_mean.signum();

    if er > 0.5 {
        let signal = momentum * (er - 0.5) * 2.0;
        let p_up = (0.5 + signal * 0.35).clamp(0.1, 0.9);
        let confidence = er * 0.6;
        Some((p_up, confidence.clamp(0.1, 0.7)))
    } else if er < 0.3 {
        let signal = -momentum * 0.3;
        let p_up = (0.5 + signal * 0.25).clamp(0.15, 0.85);
        let confidence = (1.0 - er) * 0.3;
        Some((p_up, confidence.clamp(0.05, 0.5)))
    } else {
        None
    }
}

/// Volume spike direction + price range breakout (unchanged across profiles)
fn volume_breakout(ctx: &PredictionContext) -> Option<(f64, f64)> {
    if ctx.candles.len() < 8 {
        return None;
    }

    let volumes: Vec<f64> = ctx.candles.iter().map(|c| c.volume).collect();
    let vol_sma = volumes.iter().sum::<f64>() / volumes.len() as f64;
    if vol_sma < 1e-10 {
        return None;
    }

    let spike_threshold = vol_sma * 1.5;
    let recent = &ctx.candles[ctx.candles.len().saturating_sub(5)..];
    let spike_candle = recent.iter().rev().find(|c| c.volume > spike_threshold);

    let candle = match spike_candle {
        Some(c) => c,
        None => return None,
    };

    let direction = if candle.close > candle.open {
        1.0
    } else {
        -1.0
    };
    let spike_ratio = candle.volume / vol_sma;

    let high = ctx
        .candles
        .iter()
        .map(|c| c.high)
        .fold(f64::NEG_INFINITY, f64::max);
    let low = ctx
        .candles
        .iter()
        .map(|c| c.low)
        .fold(f64::INFINITY, f64::min);
    let range = high - low;
    let breakout_position = if range > 1e-10 {
        (ctx.current_price - low) / range
    } else {
        0.5
    };

    let breakout_signal = if direction > 0.0 {
        breakout_position
    } else {
        1.0 - breakout_position
    };

    let p_up = if direction > 0.0 {
        (0.5 + breakout_signal * 0.3).clamp(0.1, 0.9)
    } else {
        (0.5 - breakout_signal * 0.3).clamp(0.1, 0.9)
    };

    let confidence = ((spike_ratio - 1.5) / 2.0).clamp(0.1, 0.7);

    Some((p_up, confidence))
}

/// Cross-asset return agreement + BTC beta lead (unchanged across profiles)
fn cross_asset_momentum(ctx: &PredictionContext) -> Option<(f64, f64)> {
    if ctx.all_reference_prices.len() < 2 || ctx.all_prices.len() < 2 {
        return None;
    }

    let btc_beta = match ctx.round.asset {
        Asset::BTC => 1.0,
        Asset::ETH => 0.9,
        Asset::SOL => 1.3,
        Asset::XRP => 1.1,
    };

    let mut returns: HashMap<Asset, f64> = HashMap::new();
    for (&asset, &current) in &ctx.all_prices {
        if let Some(&reference) = ctx.all_reference_prices.get(&asset) {
            if reference > 0.0 && current > 0.0 {
                returns.insert(asset, (current / reference).ln());
            }
        }
    }

    if returns.len() < 2 {
        return None;
    }

    let btc_return = returns.get(&Asset::BTC).copied().unwrap_or(0.0);
    let our_return = returns.get(&ctx.round.asset).copied().unwrap_or(0.0);

    let our_direction = our_return.signum();
    let agreement_count = returns
        .values()
        .filter(|&&r| r.signum() == our_direction)
        .count();
    let agreement_ratio = agreement_count as f64 / returns.len() as f64;

    let btc_signal = if ctx.round.asset != Asset::BTC {
        btc_return * btc_beta
    } else {
        0.0
    };

    let combined = our_return.signum() * agreement_ratio * 0.5 + btc_signal * 0.5;
    let p_up = (0.5 + combined.clamp(-0.3, 0.3)).clamp(0.15, 0.85);

    let confidence = (agreement_ratio * 0.3).clamp(0.05, 0.3);

    Some((p_up, confidence))
}

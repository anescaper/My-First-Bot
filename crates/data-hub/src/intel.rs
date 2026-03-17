//! Data-driven intelligence computation from candle data and round state.
//!
//! Provides `compute_intel()`, which produces a `DataDrivenIntel` struct for a
//! single asset. This is pre-computed by the data-hub so that all 8 bot pipelines
//! receive the same technical signals without each re-deriving them from raw candles.
//!
//! The signals are intentionally simple (log-returns, realized vol, displacement)
//! rather than model-dependent. Strategies in the Python service layer are
//! responsible for applying their own models (GARCH, Kalman, etc.) on top.

use std::collections::HashMap;
use polybot_scanner::crypto::{Asset, Timeframe};
use polybot_scanner::price_feed::Candle;
use crate::candle_store::CandleStore;
use crate::round_tracker::RoundTracker;
use crate::types::DataDrivenIntel;

/// Compute data-driven intel for a single asset using candle data and round state.
///
/// Produces per-timeframe candle trends (log-returns), realized volatility,
/// displacement from reference prices, and a cross-timeframe trend agreement score.
///
/// # Arguments
/// * `asset` - Which crypto asset to compute intel for (BTC, ETH, SOL, XRP).
/// * `candle_store` - Source of aggregated OHLCV candles at multiple intervals.
/// * `round_tracker` - Source of reference prices for displacement calculation.
/// * `current_price` - Latest spot price (used for displacement computation).
///
/// # Returns
/// A `DataDrivenIntel` struct populated with signals for 5m, 15m, and 1h
/// timeframes. The `child_accuracy` field is left empty (future research).
///
/// # Hardcoded timeframes
/// Only computes for `[FiveMin, FifteenMin, OneHour]` -- these are the three
/// timeframes that Polymarket crypto rounds use. `OneDay` is excluded because
/// no Polymarket 1d rounds exist.
pub fn compute_intel(
    asset: Asset,
    candle_store: &CandleStore,
    round_tracker: &RoundTracker,
    current_price: f64,
) -> DataDrivenIntel {
    let mut candle_trend = HashMap::new();
    let mut realized_vol = HashMap::new();
    let mut displacement = HashMap::new();

    let timeframes = [Timeframe::FiveMin, Timeframe::FifteenMin, Timeframe::OneHour];

    for &tf in &timeframes {
        let slug = tf.slug();
        let candles = get_candles_for_timeframe(asset, tf, candle_store);

        if !candles.is_empty() {
            candle_trend.insert(slug.to_string(), compute_trend(&candles));
            realized_vol.insert(slug.to_string(), compute_realized_vol(&candles));
        }

        // Displacement from reference price (log-return)
        if let Some(ref_price) = round_tracker.get_reference(asset, tf) {
            if ref_price > 0.0 && current_price > 0.0 {
                let disp = (current_price / ref_price).ln();
                displacement.insert(slug.to_string(), disp);
            }
        }
    }

    let trend_agreement = compute_trend_agreement(&candle_trend);

    DataDrivenIntel {
        candle_trend,
        realized_vol,
        trend_agreement,
        displacement,
        ..Default::default()
    }
}

/// Get the appropriate candles for a timeframe from the store.
///
/// Uses a fallback strategy to maximize granularity:
/// - 5m: last 5 x 1m candles (always available after warm start)
/// - 15m: last 15 x 1m candles (preferred), fallback to last 3 x 5m candles
/// - 1h: last 12 x 5m candles (preferred), fallback to last 4 x 15m candles
/// - 1d: all available 1h candles (not currently used)
///
/// The minimum threshold for preferring higher-granularity candles is hardcoded:
/// - 15m prefers 1m if >= 10 candles available
/// - 1h prefers 5m if >= 8 candles available
///
/// These thresholds ensure the fallback only triggers on cold start when higher-TF
/// candles from DB are available but 1m candles have not accumulated yet.
fn get_candles_for_timeframe(asset: Asset, tf: Timeframe, store: &CandleStore) -> Vec<Candle> {
    match tf {
        Timeframe::FiveMin => {
            let candles = store.get_candles(asset, "1m");
            tail(&candles, 5)
        }
        Timeframe::FifteenMin => {
            // Prefer 1m candles (more granular)
            let candles_1m = store.get_candles(asset, "1m");
            if candles_1m.len() >= 10 {
                return tail(&candles_1m, 15);
            }
            // Fallback to 5m candles
            let candles_5m = store.get_candles(asset, "5m");
            tail(&candles_5m, 3)
        }
        Timeframe::OneHour => {
            // Prefer 5m candles
            let candles_5m = store.get_candles(asset, "5m");
            if candles_5m.len() >= 8 {
                return tail(&candles_5m, 12);
            }
            // Fallback to 15m candles
            let candles_15m = store.get_candles(asset, "15m");
            tail(&candles_15m, 4)
        }
        Timeframe::OneDay => {
            store.get_candles(asset, "1h")
        }
    }
}

/// Return the last `n` elements of a slice, preserving order.
fn tail(candles: &[Candle], n: usize) -> Vec<Candle> {
    let start = candles.len().saturating_sub(n);
    candles[start..].to_vec()
}

/// Compute log-return trend from candles: ln(last.close / first.open), clamped to [-1, 1].
fn compute_trend(candles: &[Candle]) -> f64 {
    if candles.len() < 2 {
        return 0.0;
    }
    let first_open = candles[0].open;
    let last_close = candles[candles.len() - 1].close;
    if first_open <= 0.0 {
        return 0.0;
    }
    (last_close / first_open).ln().clamp(-1.0, 1.0)
}

/// Realized volatility: sample std deviation of per-candle log-returns.
fn compute_realized_vol(candles: &[Candle]) -> f64 {
    if candles.len() < 3 {
        return 0.0;
    }
    let returns: Vec<f64> = candles
        .windows(2)
        .filter_map(|w| {
            if w[0].close <= 0.0 { None }
            else { Some((w[1].close / w[0].close).ln()) }
        })
        .collect();
    if returns.len() < 2 {
        return 0.0;
    }
    let n = returns.len() as f64;
    let mean = returns.iter().sum::<f64>() / n;
    let variance = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0);
    variance.sqrt()
}

/// Magnitude-weighted trend agreement across timeframes.
///
/// Each timeframe's trend contributes both direction (sign) and conviction (magnitude).
/// The result is the weighted-average direction, where stronger trends count more.
/// Range: [-1.0, +1.0], continuous (not discrete).
///
/// Example: 5m = +0.002, 15m = -0.008, 1h = -0.015
///   numerator = 0.002 - 0.008 - 0.015 = -0.021
///   denominator = 0.002 + 0.008 + 0.015 = 0.025
///   agreement = -0.021 / 0.025 = -0.840
fn compute_trend_agreement(candle_trend: &HashMap<String, f64>) -> f64 {
    if candle_trend.is_empty() {
        return 0.0;
    }
    let sum_signed: f64 = candle_trend.values().sum();
    let sum_abs: f64 = candle_trend.values().map(|v| v.abs()).sum();
    if sum_abs < 1e-12 {
        return 0.0;
    }
    (sum_signed / sum_abs).clamp(-1.0, 1.0)
}


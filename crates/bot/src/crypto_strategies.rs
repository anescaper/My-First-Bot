//! Crypto round prediction strategies for Up/Down binary markets.
//!
//! Contains 5 statistically grounded strategies that predict the probability of
//! a crypto asset going Up vs Down within a time-bounded round on Polymarket.
//!
//! Strategy weights (hardcoded in `CryptoStrategyConfig::defaults()`):
//! 1. **TimeDecayLock** (weight 3.0): Z-score displacement vs expected volatility
//! 2. **CandleTrend** (weight 2.5): EMA of log-returns + candle body consistency
//! 3. **VolRegime** (weight 2.0): Kaufman Efficiency Ratio for trend vs range detection
//! 4. **VolumeBreakout** (weight 1.5): Volume spike direction + price range breakout
//! 5. **CrossAssetMomentum** (weight 1.0): BTC beta-adjusted cross-asset agreement
//!
//! The `CryptoComposer` combines all strategies using confidence-weighted voting
//! with cross-timeframe agreement scaling. Only used for the `baseline` strategy profile;
//! other profiles use the Python strategy service instead.

use polybot_scanner::crypto::{Asset, CryptoRound, Timeframe};
use polybot_scanner::price_feed::Candle;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// === Types ===

/// All the context needed for a strategy to make a prediction about a crypto round.
///
/// Built once per round per cycle in the pipeline, then passed to each strategy.
/// Contains the round's market data, reference/current prices, candles,
/// cross-asset data, and cross-timeframe intelligence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredictionContext {
    pub round: CryptoRound,
    pub reference_price: f64,
    pub current_price: f64,
    pub candles: Vec<Candle>,
    /// Current prices for all assets (for cross-asset analysis)
    pub all_prices: HashMap<Asset, f64>,
    /// Reference prices for all assets at round start (for cross-asset returns)
    pub all_reference_prices: HashMap<Asset, f64>,
    /// Active rounds for all timeframes of same asset
    pub sibling_rounds: Vec<CryptoRound>,
    /// Cross-timeframe intelligence for this asset (None if not computed)
    pub timeframe_intel: Option<crate::timeframe_intel::TimeframeIntel>,
}

/// Output of a single strategy's prediction for a crypto round.
///
/// Each strategy independently estimates p_up (probability of Up winning)
/// and a confidence level. The composer combines these via weighted averaging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CryptoPrediction {
    /// Name of the strategy that produced this prediction
    pub strategy_name: String,
    /// Estimated probability that the Up token wins (0.0 to 1.0)
    pub p_up: f64,
    /// How confident the strategy is in its prediction (0.0 to 1.0)
    pub confidence: f64,
    /// Human-readable explanation of the prediction logic
    pub reasoning: String,
}

/// Combined prediction from all strategies after ensemble voting.
///
/// The composer weights each strategy's p_up by (weight * confidence),
/// then computes agreement-based ensemble confidence and cross-timeframe scaling.
///
/// Strategy pipeline hints (hold_to_resolution, progress window, max_entry_price)
/// are populated by the Python strategy service; Rust strategies leave them at defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposedCryptoPrediction {
    /// Ensemble probability of Up winning (confidence-weighted average of strategies)
    pub p_up: f64,
    /// Ensemble confidence: avg_confidence * agreement_ratio * timeframe_multiplier
    pub confidence: f64,
    /// Signal strength: confidence * |p_up - 0.5| * 2. NOT a probability edge.
    pub edge: f64,
    /// "Up" or "Down" — determined by comparing p_up to market price
    pub direction: String,
    /// Individual strategy predictions that contributed to this ensemble
    pub components: Vec<CryptoPrediction>,
    /// If true, hold position to on-chain resolution instead of early exit.
    /// Set by Python strategy service for high-confidence path-conditioned signals.
    #[serde(default)]
    pub hold_to_resolution: bool,
    /// Minimum round progress (0.0-1.0) before entering. Strategy timing hint.
    #[serde(default)]
    pub min_progress: Option<f64>,
    /// Maximum round progress (0.0-1.0) after which we skip entry.
    #[serde(default)]
    pub max_progress: Option<f64>,
    /// Maximum entry price (CLOB ask) above which we skip entry.
    #[serde(default)]
    pub max_entry_price: Option<f64>,
}

// === Helpers ===

/// Abramowitz & Stegun polynomial approximation of the standard normal CDF.
///
/// Accuracy: max error ~1.5e-7. Uses equation 7.1.26 from "Handbook of
/// Mathematical Functions" (1964). Coefficients A1-A5 and P are published
/// constants — not tuned, not hardcoded arbitrarily.
///
/// Used by TimeDecayLock to convert a z-score into a probability.
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

/// Compute log-returns from consecutive candle close prices.
///
/// Log-returns are used instead of simple returns because they are additive
/// over time and more symmetric for positive/negative moves. Filters out
/// zero/negative prices (malformed data).
fn log_returns(candles: &[Candle]) -> Vec<f64> {
    candles.windows(2)
        .filter_map(|w| {
            if w[0].close <= 0.0 || w[1].close <= 0.0 { return None; }
            Some((w[1].close / w[0].close).ln())
        })
        .collect()
}

// === Strategy Trait ===

/// Trait for crypto round prediction strategies.
///
/// Each strategy must be Send + Sync (stored in CryptoComposer which lives in Pipeline).
/// Returns None if the strategy has no opinion (e.g., not enough data, ambiguous regime).
pub trait CryptoStrategy: Send + Sync {
    /// Unique identifier for this strategy (e.g., "time_decay_lock")
    fn name(&self) -> &str;
    /// Produce a prediction for the given round context, or None to abstain
    fn predict(&self, ctx: &PredictionContext) -> Option<CryptoPrediction>;
}

// === Strategy 1: Time Decay Lock (weight: 3.0) ===

/// Highest-weighted strategy. Uses Brownian motion assumptions to estimate
/// the probability that the current price displacement will persist to round end.
///
/// **Core idea**: As a round progresses, the remaining time for the price to
/// revert decreases. A large displacement late in the round is much more
/// predictive than early. This is the "lock-in" effect.
///
/// **Math**:
/// 1. Compute realized volatility from candle log-returns (sample std dev)
/// 2. Scale by sqrt(remaining_time / candle_interval) for expected remaining vol
/// 3. Z-score = displacement / expected_vol
/// 4. p_up = normal_CDF(z) — probability current side persists
///
/// **Hardcoded values**:
/// - progress < 0.5: skip (first half is noise, coin-flip zone)
/// - ~20 candles per round (candle_interval = timeframe_seconds / 20)
/// - confidence = progress^2 * min(|z|/2, 1) — quadratic in time, linear in z-score
/// - p_up clamped to [0.05, 0.95] — never fully certain
/// - confidence clamped to [0.05, 0.9]
pub struct TimeDecayLock;

impl CryptoStrategy for TimeDecayLock {
    fn name(&self) -> &str { "time_decay_lock" }

    fn predict(&self, ctx: &PredictionContext) -> Option<CryptoPrediction> {
        let progress = ctx.round.progress_pct();
        // No signal in the first 50% of the round
        if progress < 0.5 { return None; }
        if ctx.reference_price <= 0.0 || ctx.current_price <= 0.0 { return None; }
        if ctx.candles.len() < 3 { return None; }

        let returns = log_returns(&ctx.candles);
        if returns.is_empty() { return None; }

        // Candle-based realized volatility (std of log returns)
        let mean = returns.iter().sum::<f64>() / returns.len() as f64;
        let variance = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>()
            / ((returns.len() - 1) as f64).max(1.0);
        let candle_sigma = variance.sqrt();
        if candle_sigma < 1e-10 { return None; }

        // Expected remaining volatility via random-walk scaling
        let secs_remaining = ctx.round.seconds_remaining().max(1) as f64;
        let candle_interval = ctx.round.timeframe.seconds() as f64 / 20.0; // ~20 candles per round
        let expected_vol = candle_sigma * (secs_remaining / candle_interval.max(1.0)).sqrt();
        if expected_vol < 1e-10 { return None; }

        // Price displacement as log return
        let displacement = (ctx.current_price / ctx.reference_price).ln();
        let z = displacement / expected_vol;

        let p_up = normal_cdf(z).clamp(0.05, 0.95);
        let confidence = progress.powi(2) * (z.abs() / 2.0).min(1.0);

        Some(CryptoPrediction {
            strategy_name: self.name().into(),
            p_up,
            confidence: confidence.clamp(0.05, 0.9),
            reasoning: format!("z={:.2}, vol={:.5}, progress={:.0}%", z, expected_vol, progress * 100.0),
        })
    }
}

// === Strategy 2: Candle Trend (weight: 2.5) ===

/// Short-term momentum strategy using EMA of log-returns and candle body consistency.
///
/// **Core idea**: If recent candles consistently close in one direction, and the
/// EMA of returns is large relative to historical volatility, the trend is likely
/// to continue within the round.
///
/// **Math**:
/// 1. EMA of log-returns with alpha = 2/(n+1)
/// 2. Candle body consistency: fraction of last 5 candles that are bullish/bearish
/// 3. Magnitude: |EMA| / historical_vol (normalized signal strength)
/// 4. Signal = (EMA * k).clamp(-1, 1), p_up = 0.5 + signal * 0.4
///
/// **Hardcoded values**:
/// - k-factor by timeframe: 5m=8, 15m=6, 1h=4, 1d=2.5
///   (shorter timeframes need more amplification for comparable signal)
/// - Requires >= 5 candles (4 returns) minimum
/// - Cross-timeframe: confidence * 1.2 if parent agrees, * 0.7 if disagrees
/// - confidence clamped to [0.05, 0.8]
pub struct CandleTrend;

impl CryptoStrategy for CandleTrend {
    fn name(&self) -> &str { "candle_trend" }

    fn predict(&self, ctx: &PredictionContext) -> Option<CryptoPrediction> {
        if ctx.candles.len() < 5 { return None; }

        let returns = log_returns(&ctx.candles);
        if returns.len() < 4 { return None; }

        // Timeframe-aware k-factor for EMA
        let k = match ctx.round.timeframe {
            Timeframe::FiveMin => 8.0,
            Timeframe::FifteenMin => 6.0,
            Timeframe::OneHour => 4.0,
            Timeframe::OneDay => 2.5,
        };

        // EMA of log-returns
        let alpha = 2.0 / (returns.len() as f64 + 1.0);
        let mut ema = returns[0];
        for &r in &returns[1..] {
            ema = alpha * r + (1.0 - alpha) * ema;
        }

        // Candle body direction consistency (last 5 candles)
        let recent = &ctx.candles[ctx.candles.len().saturating_sub(5)..];
        let bullish_count = recent.iter().filter(|c| c.close > c.open).count();
        let bearish_count = recent.iter().filter(|c| c.close < c.open).count();
        let consistency = (bullish_count.max(bearish_count) as f64) / recent.len() as f64;

        // Historical vol for magnitude normalization
        let mean_r = returns.iter().sum::<f64>() / returns.len() as f64;
        let hist_vol = (returns.iter().map(|r| (r - mean_r).powi(2)).sum::<f64>()
            / ((returns.len() - 1) as f64).max(1.0)).sqrt();
        let magnitude = if hist_vol > 1e-10 { (ema.abs() / hist_vol).min(2.0) } else { 0.0 };

        let signal = (ema * k).clamp(-1.0, 1.0);
        let p_up = (0.5 + signal * 0.4).clamp(0.05, 0.95);
        let mut confidence = consistency * magnitude.min(1.0);

        // Cross-timeframe: boost confidence when parent agrees, reduce when parent disagrees
        if let Some(ref intel) = ctx.timeframe_intel {
            let parent_p = intel.parent_bias_for(&ctx.round.timeframe);
            let our_direction_is_up = p_up > 0.5;
            let parent_agrees = (our_direction_is_up && parent_p > 0.5) || (!our_direction_is_up && parent_p < 0.5);
            if parent_agrees {
                confidence *= 1.2;
            } else {
                confidence *= 0.7;
            }
            confidence = confidence.clamp(0.0, 1.0);
        }

        Some(CryptoPrediction {
            strategy_name: self.name().into(),
            p_up,
            confidence: confidence.clamp(0.05, 0.8),
            reasoning: format!("ema={:.5}, consistency={:.0}%, mag={:.2}", ema, consistency * 100.0, magnitude),
        })
    }
}

// === Strategy 3: Vol Regime (weight: 2.0) ===

/// Regime detection strategy using the Kaufman Efficiency Ratio (ER).
///
/// **Core idea**: Markets alternate between trending and ranging regimes.
/// In a trend, follow momentum. In a range, fade the move (mean reversion).
///
/// **Math**:
/// - ER = |net_move| / sum(|individual_moves|) — ranges from 0 (choppy) to 1 (straight line)
/// - ER > 0.5: TRENDING — follow momentum with confidence proportional to ER
/// - ER < 0.3: RANGING — mild mean-reversion (0.3x momentum strength, reversed)
/// - ER 0.3-0.5: AMBIGUOUS — defer to parent timeframe if it has conviction (>0.1 from 0.5)
///
/// **Hardcoded thresholds**:
/// - Trending: ER > 0.5, signal = momentum * (ER-0.5) * 2, confidence = ER * 0.6
/// - Ranging: ER < 0.3, mean-reversion at 0.3x strength, confidence = (1-ER) * 0.3
/// - Ambiguous: 0.3-0.5, follows parent if parent_bias > 0.6 or < 0.4
/// - Requires >= 10 candles
pub struct VolRegime;

impl CryptoStrategy for VolRegime {
    fn name(&self) -> &str { "vol_regime" }

    fn predict(&self, ctx: &PredictionContext) -> Option<CryptoPrediction> {
        if ctx.candles.len() < 10 { return None; }

        let n = ctx.candles.len();
        // Kaufman Efficiency Ratio: |net direction| / sum of |individual moves|
        let net_move = (ctx.candles[n - 1].close - ctx.candles[0].close).abs();
        let volatility_path: f64 = ctx.candles.windows(2)
            .map(|w| (w[1].close - w[0].close).abs())
            .sum();

        if volatility_path < 1e-10 { return None; }
        let er = net_move / volatility_path;

        // Momentum signal: direction of recent price move
        let returns = log_returns(&ctx.candles);
        if returns.is_empty() { return None; }
        let recent_mean = returns[returns.len().saturating_sub(5)..].iter().sum::<f64>()
            / returns[returns.len().saturating_sub(5)..].len() as f64;
        let momentum = recent_mean.signum();

        if er > 0.5 {
            // Trending: full momentum
            let signal = momentum * (er - 0.5) * 2.0; // 0..1 range
            let p_up = (0.5 + signal * 0.35).clamp(0.1, 0.9);
            let confidence = er * 0.6;
            Some(CryptoPrediction {
                strategy_name: self.name().into(),
                p_up,
                confidence: confidence.clamp(0.1, 0.7),
                reasoning: format!("TRENDING er={:.2}, momentum={:.5}", er, recent_mean),
            })
        } else if er < 0.3 {
            // Ranging: mild mean-reversion (0.3x momentum strength, reversed)
            let signal = -momentum * 0.3;
            let p_up = (0.5 + signal * 0.25).clamp(0.15, 0.85);
            let confidence = (1.0 - er) * 0.3;
            Some(CryptoPrediction {
                strategy_name: self.name().into(),
                p_up,
                confidence: confidence.clamp(0.05, 0.5),
                reasoning: format!("RANGING er={:.2}, mean-revert against {:.5}", er, recent_mean),
            })
        } else {
            // Ambiguous zone: follow parent timeframe if it has conviction
            if let Some(ref intel) = ctx.timeframe_intel {
                let parent_p = intel.parent_bias_for(&ctx.round.timeframe);
                if (parent_p - 0.5).abs() > 0.1 {
                    // Parent has directional conviction - follow weakly
                    let p_up = parent_p;
                    let confidence = ((parent_p - 0.5).abs() * 0.5).clamp(0.05, 0.25);
                    return Some(CryptoPrediction {
                        strategy_name: self.name().into(),
                        p_up,
                        confidence,
                        reasoning: format!("AMBIGUOUS er={:.2}, parent_bias={:.2}", er, parent_p),
                    });
                }
            }
            None
        }
    }
}

// === Strategy 4: Volume Breakout (weight: 1.5) ===

/// Detects high-volume candles and combines their direction with price range position.
///
/// **Core idea**: A volume spike with a bullish candle near the range high is a
/// strong continuation signal. A spike near the range low is bearish.
///
/// **Math**:
/// 1. Compute volume SMA across all candles
/// 2. Find most recent candle in last 5 with volume > 1.5x SMA (hardcoded threshold)
/// 3. Spike direction: bullish if close > open
/// 4. Breakout position: (current_price - low) / (high - low), 0=at low, 1=at high
/// 5. Combine: bullish spike + near highs = strong Up; bearish + near lows = strong Down
///
/// **Hardcoded values**:
/// - Volume spike threshold: 1.5x SMA (standard breakout detection heuristic)
/// - Confidence = (spike_ratio - 1.5) / 2, capped at 0.7
/// - Requires >= 8 candles
/// - Returns None if no volume spike found (no conviction without volume confirmation)
pub struct VolumeBreakout;

impl CryptoStrategy for VolumeBreakout {
    fn name(&self) -> &str { "volume_breakout" }

    fn predict(&self, ctx: &PredictionContext) -> Option<CryptoPrediction> {
        if ctx.candles.len() < 8 { return None; }

        let volumes: Vec<f64> = ctx.candles.iter().map(|c| c.volume).collect();
        let vol_sma = volumes.iter().sum::<f64>() / volumes.len() as f64;
        if vol_sma < 1e-10 { return None; }

        // Find the most recent candle with volume > 1.5x SMA
        let spike_threshold = vol_sma * 1.5;
        let recent = &ctx.candles[ctx.candles.len().saturating_sub(5)..];
        let spike_candle = recent.iter().rev().find(|c| c.volume > spike_threshold);

        let candle = match spike_candle {
            Some(c) => c,
            None => return None, // No volume spike — no conviction
        };

        // Spike direction: bullish if close > open
        let direction = if candle.close > candle.open { 1.0 } else { -1.0 };
        let spike_ratio = candle.volume / vol_sma;

        // Price range breakout position: where is current price relative to recent range?
        let high = ctx.candles.iter().map(|c| c.high).fold(f64::NEG_INFINITY, f64::max);
        let low = ctx.candles.iter().map(|c| c.low).fold(f64::INFINITY, f64::min);
        let range = high - low;
        let breakout_position = if range > 1e-10 {
            (ctx.current_price - low) / range // 0.0 = at low, 1.0 = at high
        } else {
            0.5
        };

        // Combine spike direction with breakout position
        let breakout_signal = if direction > 0.0 {
            breakout_position // bullish spike + near highs = strong
        } else {
            1.0 - breakout_position // bearish spike + near lows = strong
        };

        let p_up = if direction > 0.0 {
            (0.5 + breakout_signal * 0.3).clamp(0.1, 0.9)
        } else {
            (0.5 - breakout_signal * 0.3).clamp(0.1, 0.9)
        };

        // Confidence scales with spike magnitude, capped at 0.7
        let confidence = ((spike_ratio - 1.5) / 2.0).clamp(0.1, 0.7);

        Some(CryptoPrediction {
            strategy_name: self.name().into(),
            p_up,
            confidence,
            reasoning: format!("vol_spike={:.1}x, dir={}, breakout={:.2}", spike_ratio, if direction > 0.0 { "bull" } else { "bear" }, breakout_position),
        })
    }
}

// === Strategy 5: Cross-Asset Momentum (weight: 1.0) ===

/// Lowest-weighted strategy. Uses cross-asset return agreement and BTC-beta lead.
///
/// **Core idea**: When most crypto assets move in the same direction, the move
/// is more likely to persist. BTC leads altcoins, so BTC's move is a leading indicator.
///
/// **Math**:
/// 1. Compute log-returns for each asset since round start
/// 2. Agreement ratio: fraction of assets moving in same direction as our asset
/// 3. BTC signal: BTC_return * asset_beta (amplified for high-beta assets)
/// 4. Combined = own_direction * agreement * 0.5 + btc_signal * 0.5
///
/// **Hardcoded BTC betas**:
/// - BTC=1.0, ETH=0.9, SOL=1.3, XRP=1.1
/// - These are approximate empirical betas, not calibrated. SOL has higher beta
///   because it's more volatile and tends to amplify BTC moves.
///
/// **Confidence always low** (max 0.3) because cross-asset correlations are
/// unreliable on the short timeframes these binary rounds operate on.
/// Requires >= 2 assets with valid price data.
pub struct CrossAssetMomentum;

impl CryptoStrategy for CrossAssetMomentum {
    fn name(&self) -> &str { "cross_asset_momentum" }

    fn predict(&self, ctx: &PredictionContext) -> Option<CryptoPrediction> {
        if ctx.all_reference_prices.len() < 2 || ctx.all_prices.len() < 2 { return None; }

        // BTC betas for altcoins
        let btc_beta = match ctx.round.asset {
            Asset::BTC => 1.0,
            Asset::ETH => 0.9,
            Asset::SOL => 1.3,
            Asset::XRP => 1.1,
        };

        // Compute returns for each asset
        let mut returns: HashMap<Asset, f64> = HashMap::new();
        for (&asset, &current) in &ctx.all_prices {
            if let Some(&reference) = ctx.all_reference_prices.get(&asset) {
                if reference > 0.0 && current > 0.0 {
                    returns.insert(asset, (current / reference).ln());
                }
            }
        }

        if returns.len() < 2 { return None; }

        // BTC return as lead signal
        let btc_return = returns.get(&Asset::BTC).copied().unwrap_or(0.0);
        let our_return = returns.get(&ctx.round.asset).copied().unwrap_or(0.0);

        // Agreement: how many assets move in the same direction?
        let our_direction = our_return.signum();
        let agreement_count = returns.values().filter(|&&r| r.signum() == our_direction).count();
        let agreement_ratio = agreement_count as f64 / returns.len() as f64;

        // BTC beta-adjusted signal for altcoins
        let btc_signal = if ctx.round.asset != Asset::BTC {
            btc_return * btc_beta
        } else {
            0.0
        };

        // Combine: own return direction + BTC lead + cross-asset agreement
        let combined = our_return.signum() * agreement_ratio * 0.5 + btc_signal * 0.5;
        let p_up = (0.5 + combined.clamp(-0.3, 0.3)).clamp(0.15, 0.85);

        // Confidence always low — cross-asset correlations are unreliable on short timeframes
        let confidence = (agreement_ratio * 0.3).clamp(0.05, 0.3);

        Some(CryptoPrediction {
            strategy_name: self.name().into(),
            p_up,
            confidence,
            reasoning: format!("agree={}/{}, btc_r={:.4}%, beta={:.1}", agreement_count, returns.len(), btc_return * 100.0, btc_beta),
        })
    }
}

// === Composer ===

/// Configuration for a single crypto strategy: enabled state and ensemble weight.
///
/// These can be updated at runtime via the API (PUT /crypto/strategies)
/// and are synced to the pipeline each cycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CryptoStrategyConfig {
    /// Strategy identifier (must match CryptoStrategy::name())
    pub name: String,
    /// Whether this strategy participates in the ensemble
    pub enabled: bool,
    /// Weight in the confidence-weighted ensemble. Higher = more influence.
    pub weight: f64,
}

impl CryptoStrategyConfig {
    /// Default strategy configurations with empirically-tuned weights.
    ///
    /// Weights reflect how much each strategy's signal is trusted in the ensemble:
    /// - time_decay_lock (3.0): most reliable — physics-based (Brownian motion)
    /// - candle_trend (2.5): good momentum detection with cross-tf confirmation
    /// - vol_regime (2.0): useful regime context, but mode detection is noisy
    /// - volume_breakout (1.5): strong when it fires, but signals are rare
    /// - cross_asset_momentum (1.0): weakest — correlations unreliable on short tf
    pub fn defaults() -> Vec<Self> {
        vec![
            Self { name: "time_decay_lock".into(), enabled: true, weight: 3.0 },
            Self { name: "candle_trend".into(), enabled: true, weight: 2.5 },
            Self { name: "vol_regime".into(), enabled: true, weight: 2.0 },
            Self { name: "volume_breakout".into(), enabled: true, weight: 1.5 },
            Self { name: "cross_asset_momentum".into(), enabled: true, weight: 1.0 },
        ]
    }
}

/// Ensemble composer that aggregates predictions from all enabled crypto strategies.
///
/// Uses confidence-weighted voting: each strategy's p_up is weighted by
/// (config.weight * strategy.confidence). Strategies with higher confidence
/// and higher config weight have proportionally more influence.
///
/// Post-aggregation, applies:
/// 1. Agreement-based confidence scaling (strategies disagreeing = lower confidence)
/// 2. Cross-timeframe agreement multiplier (parent alignment boosts/penalizes)
/// 3. Minimum confidence gate (hardcoded 0.15 — below this, no signal)
pub struct CryptoComposer {
    /// Strategy implementations paired with their runtime configuration
    strategies: Vec<(Box<dyn CryptoStrategy>, CryptoStrategyConfig)>,
}

impl CryptoComposer {
    /// Create a new composer with all 5 strategies, using the provided configs.
    ///
    /// If a config is missing for a strategy, falls back to enabled=true, weight=1.0.
    pub fn new(configs: &[CryptoStrategyConfig]) -> Self {
        let all_strategies: Vec<(Box<dyn CryptoStrategy>, &str)> = vec![
            (Box::new(TimeDecayLock), "time_decay_lock"),
            (Box::new(CandleTrend), "candle_trend"),
            (Box::new(VolRegime), "vol_regime"),
            (Box::new(VolumeBreakout), "volume_breakout"),
            (Box::new(CrossAssetMomentum), "cross_asset_momentum"),
        ];

        let strategies = all_strategies.into_iter().map(|(strategy, name)| {
            let config = configs.iter()
                .find(|c| c.name == name)
                .cloned()
                .unwrap_or(CryptoStrategyConfig {
                    name: name.to_string(),
                    enabled: true,
                    weight: 1.0,
                });
            (strategy, config)
        }).collect();

        Self { strategies }
    }

    /// Run all enabled strategies and produce an ensemble prediction.
    ///
    /// Returns None if: no strategies fire, total weight is zero, or ensemble
    /// confidence is below 0.15 (hardcoded minimum gate).
    ///
    /// Edge calculation: confidence * |p_up - 0.5| * 2
    /// This is a signal strength metric (0 to 1), not a probability edge.
    /// Direction is "Up" if our p_up > market's p_up, else "Down".
    pub fn predict(&self, ctx: &PredictionContext) -> Option<ComposedCryptoPrediction> {
        let mut components = Vec::new();
        let mut weighted_sum = 0.0;
        let mut total_weight = 0.0;

        for (strategy, config) in &self.strategies {
            if !config.enabled { continue; }

            if let Some(pred) = strategy.predict(ctx) {
                let w = config.weight * pred.confidence;
                weighted_sum += pred.p_up * w;
                total_weight += w;
                components.push(pred);
            }
        }

        if total_weight == 0.0 || components.is_empty() { return None; }

        let p_up = (weighted_sum / total_weight).clamp(0.01, 0.99);

        // Agreement-based confidence: if strategies disagree, confidence drops
        let up_count = components.iter().filter(|c| c.p_up > 0.5).count();
        let down_count = components.len() - up_count;
        let agreement = up_count.max(down_count) as f64 / components.len() as f64;

        let avg_confidence = components.iter().map(|c| c.confidence).sum::<f64>() / components.len() as f64;
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
        if ensemble_confidence < 0.15 { return None; }

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

    /// Hot-reload strategy configs (enabled/weight) without recreating strategies.
    /// Called at the start of each cycle to pick up API changes.
    pub fn update_configs(&mut self, new_configs: &[CryptoStrategyConfig]) {
        for (_, config) in &mut self.strategies {
            if let Some(new) = new_configs.iter().find(|c| c.name == config.name) {
                config.enabled = new.enabled;
                config.weight = new.weight;
            }
        }
    }
}

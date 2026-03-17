//! Base strategy types and implementations for non-crypto Polymarket event markets.
//!
//! Contains 6 strategies for general prediction markets (elections, sports, etc.):
//! 1. **MeanReversionStrategy**: pulls extreme prices toward 50%
//! 2. **ArbitrageStrategy**: exploits sum-of-outcomes != 1.0
//! 3. **LiquidityEdgeStrategy**: fades low-liquidity price inefficiencies
//! 4. **SpreadStrategy**: captures edge from wide bid-ask spreads
//! 5. **CalibrationFaderStrategy**: fades extreme prices (>90% or <10%)
//! 6. **VolumeMomentumStrategy**: momentum amplified by turnover ratio
//!
//! These strategies operate on snapshot market data (no candle history)
//! and are combined by the `StrategyComposer` using confidence-weighted voting.
//!
//! Currently always runs in paper mode — the crypto pipeline is the live focus.

use polybot_api::state::StrategyConfig;
use polybot_scanner::types::Market;
use serde::{Deserialize, Serialize};

/// A prediction from a single general-market strategy.
///
/// p_model is the strategy's estimate of the true probability of outcome[0].
/// The edge (p_model - p_market) determines whether to buy Yes or No tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyPrediction {
    /// Name of the strategy that produced this prediction
    pub strategy_name: String,
    /// Model's estimated probability of outcome[0] (the "Yes" side)
    pub p_model: f64,
    /// How confident the strategy is in its estimate (0.0 to 1.0)
    pub confidence: f64,
    /// Human-readable explanation of the prediction logic
    pub reasoning: String,
}

/// Combined prediction from all strategies after ensemble voting.
///
/// Edge = p_model - p_market. Positive edge = buy Yes, negative = buy No.
/// z_score = edge / sigma (hardcoded sigma=0.05) for statistical significance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposedPrediction {
    /// Ensemble estimated probability of outcome[0]
    pub p_model: f64,
    /// Average confidence across all contributing strategies
    pub confidence: f64,
    /// p_model - p_market: positive means model thinks Yes is underpriced
    pub edge: f64,
    /// edge / 0.05: how many standard deviations the edge represents.
    /// Sigma = 0.05 (hardcoded) is an assumed market noise level for general markets.
    pub z_score: f64,
    /// Individual strategy predictions that contributed to the ensemble
    pub components: Vec<StrategyPrediction>,
}

/// Trait for general-market prediction strategies.
///
/// These operate on Polymarket Market structs (question, outcome_prices, liquidity, etc.)
/// rather than crypto rounds with candle data.
pub trait PredictionStrategy: Send + Sync {
    /// Unique identifier for this strategy
    fn name(&self) -> &str;
    /// Produce a prediction for the given market, or None to abstain
    fn predict(&self, market: &Market) -> Option<StrategyPrediction>;
}

// === Strategy 1: Mean Reversion ===

/// Pulls prices toward 50% (the maximum uncertainty point).
///
/// The further a price is from 0.5, the stronger the pull.
/// Hardcoded: reversion_strength = 0.15 (15% of the distance to 0.5).
/// Confidence scales with distance: max 0.3 (markets near 50% have no edge).
pub struct MeanReversionStrategy;

impl PredictionStrategy for MeanReversionStrategy {
    fn name(&self) -> &str { "mean_reversion" }

    fn predict(&self, market: &Market) -> Option<StrategyPrediction> {
        if market.outcome_prices.len() < 2 { return None; }
        let p = market.outcome_prices[0];

        let distance = (p - 0.5).abs();
        let reversion = 0.5 + (0.5 - p) * 0.15;
        let confidence = (distance * 2.0).min(1.0) * 0.3;

        Some(StrategyPrediction {
            strategy_name: self.name().into(),
            p_model: reversion.clamp(0.01, 0.99),
            confidence,
            reasoning: format!("Mean reversion: market at {:.1}%, pulling toward 50%", p * 100.0),
        })
    }
}

// === Strategy 2: Arbitrage Detector ===

/// Exploits mispricing when outcome prices don't sum to 1.0.
///
/// In a perfect binary market, P(Yes) + P(No) = 1.0. When the sum deviates,
/// one side is overpriced. Hardcoded: 0.005 minimum deviation threshold (0.5% vig).
/// Confidence = deviation * 10, max 0.8 — high confidence because arbitrage is mathematical.
pub struct ArbitrageStrategy;

impl PredictionStrategy for ArbitrageStrategy {
    fn name(&self) -> &str { "arbitrage" }

    fn predict(&self, market: &Market) -> Option<StrategyPrediction> {
        if market.outcome_prices.len() < 2 { return None; }
        let sum: f64 = market.outcome_prices.iter().sum();

        let deviation = sum - 1.0;
        if deviation.abs() < 0.005 { return None; }

        let p = market.outcome_prices[0];
        let adjustment = -deviation * p;
        let p_model = (p + adjustment).clamp(0.01, 0.99);
        let confidence = (deviation.abs() * 10.0).min(1.0) * 0.8;

        Some(StrategyPrediction {
            strategy_name: self.name().into(),
            p_model,
            confidence,
            reasoning: format!("Arb: sum={:.4}, deviation={:.4}, adjustment={:.4}", sum, deviation, adjustment),
        })
    }
}

// === Strategy 3: Liquidity-Weighted Fair Value ===

/// Fades prices in low-liquidity markets toward 50%.
///
/// Low-liquidity markets have wider spreads and less price discovery, so prices
/// are more likely to be wrong. The pull strength is inversely proportional to
/// liquidity. Hardcoded: $100k liquidity normalization, 20% max pull strength.
/// Minimum 1% price adjustment to produce a signal.
pub struct LiquidityEdgeStrategy;

impl PredictionStrategy for LiquidityEdgeStrategy {
    fn name(&self) -> &str { "liquidity_edge" }

    fn predict(&self, market: &Market) -> Option<StrategyPrediction> {
        if market.outcome_prices.len() < 2 { return None; }
        let p = market.outcome_prices[0];

        let liquidity_score = (market.liquidity / 100_000.0).min(1.0);
        let pull_strength = (1.0 - liquidity_score) * 0.2;
        let p_model = p + (0.5 - p) * pull_strength;

        if (p_model - p).abs() < 0.01 { return None; }

        let confidence = (1.0 - liquidity_score) * 0.4;

        Some(StrategyPrediction {
            strategy_name: self.name().into(),
            p_model: p_model.clamp(0.01, 0.99),
            confidence,
            reasoning: format!("Liquidity edge: ${:.0} liquidity, pull={:.2}", market.liquidity, pull_strength),
        })
    }
}

// === Strategy 4: Spread Exploiter ===

/// Captures edge from wide bid-ask spreads by entering on the underpriced side.
///
/// Wide spreads indicate less efficient pricing. Trades against the dominant
/// side (sells into strength). Hardcoded: minimum 2% spread to trigger,
/// captures 30% of the spread as edge. Confidence = spread * 5, max 0.5.
pub struct SpreadStrategy;

impl PredictionStrategy for SpreadStrategy {
    fn name(&self) -> &str { "spread" }

    fn predict(&self, market: &Market) -> Option<StrategyPrediction> {
        if market.outcome_prices.len() < 2 { return None; }
        if market.spread <= 0.02 { return None; }

        let p = market.outcome_prices[0];
        let spread_edge = market.spread * 0.3;
        let direction = if p > 0.5 { -1.0 } else { 1.0 };
        let p_model = (p + direction * spread_edge).clamp(0.01, 0.99);

        let confidence = (market.spread * 5.0).min(1.0) * 0.5;

        Some(StrategyPrediction {
            strategy_name: self.name().into(),
            p_model,
            confidence,
            reasoning: format!("Spread: {:.1}% spread, capturing {:.1}% edge", market.spread * 100.0, spread_edge * 100.0),
        })
    }
}

// === Strategy 5: Extreme Price Fader ===

/// Fades extreme prices (>90% or <10%) based on calibration research.
///
/// Prediction markets systematically overprice near-certain outcomes (overconfidence bias).
/// A market at 95% is more likely to resolve at 90-95% true probability.
/// Hardcoded: only fires outside 10-90% range, fades by 50% of the excess.
/// Higher confidence (0.7) for extreme prices >95% or <5%.
pub struct CalibrationFaderStrategy;

impl PredictionStrategy for CalibrationFaderStrategy {
    fn name(&self) -> &str { "calibration_fader" }

    fn predict(&self, market: &Market) -> Option<StrategyPrediction> {
        if market.outcome_prices.len() < 2 { return None; }
        let p = market.outcome_prices[0];

        if p > 0.10 && p < 0.90 { return None; }

        let p_model = if p >= 0.90 {
            let fade = (p - 0.90) * 0.5;
            p - fade
        } else {
            let fade = (0.10 - p) * 0.5;
            p + fade
        };

        let confidence = if p >= 0.95 || p <= 0.05 { 0.7 } else { 0.4 };

        Some(StrategyPrediction {
            strategy_name: self.name().into(),
            p_model: p_model.clamp(0.01, 0.99),
            confidence,
            reasoning: format!("Calibration fade: market at {:.1}% is likely overconfident", p * 100.0),
        })
    }
}

// === Strategy 6: Volume Momentum ===

/// High-turnover momentum: amplifies price direction by trading volume.
///
/// Markets with high volume/liquidity turnover ratio are actively traded,
/// and the current price reflects stronger conviction.
/// Hardcoded: minimum 0.5x turnover to trigger, momentum scales linearly
/// with turnover (capped at 2.0x excess), 10% of price deviation.
pub struct VolumeMomentumStrategy;

impl PredictionStrategy for VolumeMomentumStrategy {
    fn name(&self) -> &str { "volume_momentum" }

    fn predict(&self, market: &Market) -> Option<StrategyPrediction> {
        if market.outcome_prices.len() < 2 { return None; }
        if market.liquidity <= 0.0 { return None; }

        let turnover = market.volume_24h / market.liquidity;
        if turnover < 0.5 { return None; }

        let p = market.outcome_prices[0];
        let momentum = (p - 0.5) * (turnover - 0.5).min(2.0) * 0.1;
        let p_model = (p + momentum).clamp(0.01, 0.99);

        if (p_model - p).abs() < 0.01 { return None; }

        let confidence = (turnover / 3.0).min(1.0) * 0.5;

        Some(StrategyPrediction {
            strategy_name: self.name().into(),
            p_model: p_model.clamp(0.01, 0.99),
            confidence,
            reasoning: format!("Volume momentum: turnover={:.2}x, signal={:.4}", turnover, momentum),
        })
    }
}

// === Composer ===

/// Ensemble composer for general-market strategies.
///
/// Same confidence-weighted voting approach as CryptoComposer:
/// p_model = sum(p_i * w_i * conf_i) / sum(w_i * conf_i)
///
/// z_score uses a hardcoded sigma=0.05 representing assumed market noise.
/// This is a rough heuristic — general markets have highly variable noise levels.
pub struct StrategyComposer {
    /// Strategy implementations paired with their runtime configuration
    strategies: Vec<(Box<dyn PredictionStrategy>, StrategyConfig)>,
}

impl StrategyComposer {
    /// Create a new composer with all 6 strategies, using the provided configs.
    /// Missing configs default to enabled=true, weight=1.0.
    pub fn new(configs: &[StrategyConfig]) -> Self {
        let all_strategies: Vec<(Box<dyn PredictionStrategy>, &str)> = vec![
            (Box::new(MeanReversionStrategy), "mean_reversion"),
            (Box::new(ArbitrageStrategy), "arbitrage"),
            (Box::new(LiquidityEdgeStrategy), "liquidity_edge"),
            (Box::new(SpreadStrategy), "spread"),
            (Box::new(CalibrationFaderStrategy), "calibration_fader"),
            (Box::new(VolumeMomentumStrategy), "volume_momentum"),
        ];

        let strategies = all_strategies.into_iter().map(|(strategy, name)| {
            let config = configs.iter()
                .find(|c| c.name == name)
                .cloned()
                .unwrap_or(StrategyConfig {
                    name: name.to_string(),
                    enabled: true,
                    weight: 1.0,
                });
            (strategy, config)
        }).collect();

        Self { strategies }
    }

    /// Run all enabled strategies on a market and produce an ensemble prediction.
    /// Returns None if no strategies produce a valid prediction.
    pub fn predict(&self, market: &Market) -> Option<ComposedPrediction> {
        let mut components = Vec::new();
        let mut weighted_sum = 0.0;
        let mut total_weight = 0.0;

        for (strategy, config) in &self.strategies {
            if !config.enabled { continue; }

            if let Some(pred) = strategy.predict(market) {
                let w = config.weight * pred.confidence;
                weighted_sum += pred.p_model * w;
                total_weight += w;
                components.push(pred);
            }
        }

        if total_weight == 0.0 || components.is_empty() { return None; }

        let p_model = (weighted_sum / total_weight).clamp(0.01, 0.99);
        let p_market = market.outcome_prices.first().copied().unwrap_or(0.5);
        let edge = p_model - p_market;
        let sigma = 0.05;
        let z_score = edge / sigma;
        let avg_confidence = components.iter().map(|c| c.confidence).sum::<f64>() / components.len() as f64;

        Some(ComposedPrediction {
            p_model,
            confidence: avg_confidence,
            edge,
            z_score,
            components,
        })
    }

    /// Hot-reload strategy configs without recreating strategy objects.
    pub fn update_configs(&mut self, new_configs: &[StrategyConfig]) {
        for (_, config) in &mut self.strategies {
            if let Some(new) = new_configs.iter().find(|c| c.name == config.name) {
                config.enabled = new.enabled;
                config.weight = new.weight;
            }
        }
    }
}

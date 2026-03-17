//! Cross-timeframe intelligence: computed once per cycle per asset.
//!
//! This module provides `TimeframeIntel`, a lightweight struct that aggregates
//! directional information across all active timeframes (5m, 15m, 1h) for a single asset.
//!
//! Used by strategies to:
//! - Boost confidence when the parent timeframe agrees with the signal direction
//! - Reduce confidence when the parent timeframe disagrees
//! - Scale ensemble confidence by cross-timeframe agreement ratio
//!
//! The data comes from either the DataClient's intel endpoint (preferred, uses candle
//! trend analysis) or falls back to building from round CLOB prices.

use std::collections::HashMap;
use polybot_scanner::crypto::{Asset, CryptoRound, Timeframe};
use serde::{Serialize, Deserialize};

/// Aggregated intelligence across timeframes for a single asset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeframeIntel {
    /// Which asset this is for
    pub asset: Asset,
    /// Market p_up per active timeframe (e.g. {FiveMin: 0.62, FifteenMin: 0.58, OneHour: 0.65})
    pub market_bias: HashMap<String, f64>,
    /// Agreement score: -1.0 (all say Down) to +1.0 (all say Up)
    pub agreement_score: f64,
}

impl TimeframeIntel {
    /// Build intel from the active rounds for this asset.
    pub fn build(asset: Asset, rounds: &[CryptoRound]) -> Self {
        let market_bias: HashMap<String, f64> = rounds.iter()
            .filter(|r| r.asset == asset)
            .map(|r| (r.timeframe.slug().to_string(), r.price_up))
            .collect();

        // Agreement score: average of (2*p_up - 1) across timeframes
        let agreement_score = if market_bias.is_empty() {
            0.0
        } else {
            let sum: f64 = market_bias.values().map(|&p| 2.0 * p - 1.0).sum();
            (sum / market_bias.len() as f64).clamp(-1.0, 1.0)
        };

        Self { asset, market_bias, agreement_score }
    }

    /// Get parent timeframe bias for a specific timeframe.
    /// Returns the p_up of the nearest parent, or 0.5 if none available.
    pub fn parent_bias_for(&self, tf: &Timeframe) -> f64 {
        for parent in tf.parents() {
            if let Some(&p_up) = self.market_bias.get(parent.slug()) {
                return p_up;
            }
        }
        0.5
    }

    /// Direction agreement: what fraction of timeframes agree with this direction?
    /// Returns 0.0 (none agree) to 1.0 (all agree).
    pub fn direction_agreement(&self, is_up: bool) -> f64 {
        if self.market_bias.is_empty() { return 0.5; }
        let agree_count = self.market_bias.values()
            .filter(|&&p| if is_up { p > 0.5 } else { p < 0.5 })
            .count();
        agree_count as f64 / self.market_bias.len() as f64
    }
}

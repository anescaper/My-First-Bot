//! Market filtering criteria for general (non-crypto) Polymarket markets.
//!
//! Applies liquidity, volume, spread, and time-to-resolution filters to remove
//! markets that are too illiquid, too wide, or too far/close to resolution for
//! profitable trading.

use crate::types::Market;

/// Configuration for filtering general Polymarket markets.
///
/// Each threshold represents a minimum or maximum acceptable value. Markets
/// failing any single criterion are excluded. All monetary values are in USD.
///
/// These defaults are tuned for Polymarket's typical market characteristics
/// where most markets have relatively low liquidity ($100-$10K) and wider
/// spreads compared to traditional financial markets.
#[derive(Debug, Clone)]
pub struct FilterConfig {
    /// Minimum total liquidity (USD). Markets below this are too thin for reliable execution.
    pub min_liquidity: f64,
    /// Minimum 24-hour trading volume (USD). Set to 0 to allow newly created markets.
    pub min_volume_24h: f64,
    /// Maximum bid-ask spread (as a decimal, e.g. 0.20 = 20 cents). Wider spreads mean
    /// higher execution cost.
    pub max_spread: f64,
    /// Minimum days until resolution. Avoids markets about to expire where entry is risky.
    pub min_days_to_resolution: f64,
    /// Maximum days until resolution. Avoids markets too far out where capital is locked up.
    pub max_days_to_resolution: f64,
}

impl Default for FilterConfig {
    /// Default filter values tuned for Polymarket's market characteristics.
    ///
    /// - `min_liquidity: 100.0` — Low bar because most Polymarket markets are $100-$10K.
    ///   Raising this to e.g. $1000 would filter out most markets.
    /// - `min_volume_24h: 0.0` — Allow zero-volume markets (many are newly created).
    /// - `max_spread: 0.20` — 20 cents. Wider spreads are common on Polymarket vs TradFi.
    ///   This is generous; tighten to 0.10 for better execution quality.
    /// - `min_days_to_resolution: 0.5` — 12 hours. Avoids last-minute entry where
    ///   the market is nearly resolved and pricing is extreme.
    /// - `max_days_to_resolution: 365.0` — 1 year. Capital efficiency ceiling.
    fn default() -> Self {
        Self {
            min_liquidity: 100.0,
            min_volume_24h: 0.0,
            max_spread: 0.20,
            min_days_to_resolution: 0.5,
            max_days_to_resolution: 365.0,
        }
    }
}

impl FilterConfig {
    /// Apply all filter criteria to a list of markets, returning only those that pass.
    ///
    /// Each market must satisfy all thresholds simultaneously (AND logic).
    /// Markets without an `end_date` skip the time-to-resolution check (pass by default),
    /// since some Polymarket markets have no defined end date.
    ///
    /// The spread check has a special case: spread of 0.0 always passes, since it means
    /// the spread hasn't been computed yet (from `ClobClient::fetch_markets`).
    pub fn filter(&self, markets: &[Market]) -> Vec<Market> {
        let now = chrono::Utc::now();
        markets.iter().filter(|m| {
            if m.liquidity < self.min_liquidity { return false; }
            if m.volume_24h < self.min_volume_24h { return false; }
            if m.spread > self.max_spread && m.spread > 0.0 { return false; }
            if let Some(end) = m.end_date {
                let days = (end - now).num_hours() as f64 / 24.0;
                if days < self.min_days_to_resolution || days > self.max_days_to_resolution {
                    return false;
                }
            }
            true
        }).cloned().collect()
    }
}

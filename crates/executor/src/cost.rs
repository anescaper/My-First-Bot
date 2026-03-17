//! Cost model for computing trading fees and determining trade profitability.
//!
//! Supports two fee modes:
//! 1. **Flat fee** (general markets): fixed bps per side (trading_fee_bps + spread_bps)
//! 2. **Polymarket crypto fee** (crypto markets): convex fee formula from Polymarket docs
//!
//! The crypto fee formula is: `fee = shares * price * 0.25 * (price * (1 - price))^2`
//! This makes fees highest at p=0.50 (~1.56%) and near-zero at extreme prices.
//! This is a critical distinction because the pipeline uses entry-only fees for
//! round resolution exits (no SELL needed) vs round-trip fees for early exits.

use serde::{Deserialize, Serialize};

/// Configurable cost model with defaults tuned for Polymarket.
///
/// The pipeline overrides `trading_fee_bps` via the `TRADING_FEE_BPS` env var.
/// Other fields use defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostModel {
    /// Trading fee in basis points per side. Default 200 (2%).
    /// This is the flat fee used for general-market cost estimation.
    /// For crypto markets, use `polymarket_crypto_fee()` instead.
    pub trading_fee_bps: f64,
    /// Estimated gas cost per transaction in USD. Default $0.01 (Polygon is cheap).
    /// Hardcoded because Polygon gas rarely exceeds a few cents.
    pub gas_cost_usd: f64,
    /// Estimated bid-ask spread cost in basis points. Default 50 (0.5%).
    /// Represents the cost of crossing the spread when taking liquidity.
    pub spread_bps: f64,
}

impl Default for CostModel {
    fn default() -> Self {
        Self {
            trading_fee_bps: 200.0, // 2% Polymarket taker fee (general markets)
            gas_cost_usd: 0.01,     // Polygon gas — typically $0.001-0.02
            spread_bps: 50.0,       // 0.5% estimated spread cost
        }
    }
}

impl CostModel {
    /// Total cost in basis points for one side (fee + spread).
    pub fn total_bps(&self) -> f64 {
        self.trading_fee_bps + self.spread_bps
    }

    /// Cost for one side (entry OR exit).
    pub fn total_cost_usd(&self, trade_size_usd: f64) -> f64 {
        let bps_cost = trade_size_usd * self.total_bps() / 10_000.0;
        bps_cost + self.gas_cost_usd
    }

    /// Round-trip cost: trading fee on both sides, spread + gas once.
    pub fn round_trip_cost_usd(&self, trade_size_usd: f64) -> f64 {
        let fee_both_sides = trade_size_usd * self.trading_fee_bps * 2.0 / 10_000.0;
        let spread_once = trade_size_usd * self.spread_bps / 10_000.0;
        fee_both_sides + spread_once + self.gas_cost_usd * 2.0
    }

    /// Polymarket crypto fee using the official convex formula.
    ///
    /// Formula: `fee = shares * price * feeRate * (price * (1 - price))^exponent`
    /// - feeRate = 0.25 (hardcoded per Polymarket docs)
    /// - exponent = 2 (hardcoded per Polymarket docs)
    /// - shares = size_usd / entry_price
    ///
    /// This produces a convex fee curve:
    /// - At p=0.50: max fee ~1.56% of notional
    /// - At p=0.10 or p=0.90: fee ~0.20%
    /// - At p=0.01 or p=0.99: fee ~0.0002%
    ///
    /// The fee is NOT the flat `fee_rate_bps` field on orders (which is 1000 bps for
    /// crypto markets and used for order signing, not fee calculation).
    ///
    /// Minimum fee: $0.0001 (Polymarket enforced minimum).
    ///
    /// See: https://docs.polymarket.com/trading/fees
    pub fn polymarket_crypto_fee(&self, size_usd: f64, entry_price: f64) -> f64 {
        let p = entry_price.clamp(0.01, 0.99);
        let shares = if p > 0.0 { size_usd / p } else { 0.0 };
        let fee_rate = 0.25;
        let exponent = 2.0_f64;
        let fee = shares * p * fee_rate * (p * (1.0 - p)).powf(exponent);
        fee.max(0.0001) // Polymarket minimum fee
    }

    /// Entry-only cost using real Polymarket crypto fee + spread + gas.
    ///
    /// Used for positions that resolve on-chain (hold-to-resolution, round ended).
    /// These positions don't need a SELL order — Polymarket settles automatically.
    /// So only the entry fee applies, making resolution exits much cheaper.
    pub fn crypto_entry_cost(&self, size_usd: f64, entry_price: f64) -> f64 {
        let fee = self.polymarket_crypto_fee(size_usd, entry_price);
        let spread = size_usd * self.spread_bps / 10_000.0;
        fee + spread + self.gas_cost_usd
    }

    /// Round-trip cost using real Polymarket crypto fee on both sides + spread + gas.
    ///
    /// Used for early exits (stop-loss, edge reversal) where we submit a SELL order.
    /// Significantly more expensive than entry-only: fee is charged on both entry and exit.
    pub fn crypto_round_trip_cost(&self, size_usd: f64, entry_price: f64, exit_price: f64) -> f64 {
        let entry_fee = self.polymarket_crypto_fee(size_usd, entry_price);
        let exit_size = size_usd; // approximate
        let exit_fee = self.polymarket_crypto_fee(exit_size, exit_price);
        let spread = size_usd * self.spread_bps / 10_000.0;
        entry_fee + exit_fee + spread + self.gas_cost_usd * 2.0
    }

    /// Check if a trade's expected profit exceeds the round-trip cost.
    /// Uses the flat fee model (not crypto formula) — for general market estimation.
    pub fn is_profitable(&self, trade_size_usd: f64, expected_profit_usd: f64) -> bool {
        expected_profit_usd > self.round_trip_cost_usd(trade_size_usd)
    }
}

//! Concrete risk gate implementations for the composable gate chain.
//!
//! Each gate implements the `RiskGate` trait and performs a single, focused risk check.
//! Gates are combined into a `RiskGateChain` which runs them in priority order.
//!
//! Gate results are one of three:
//! - `Pass`: Signal is acceptable for this check.
//! - `Reject(reason)`: Signal is blocked. First rejection stops the chain.
//! - `Resize { multiplier, reason }`: Signal is allowed but position size is scaled down.
//!
//! Priority values (lower = runs first):
//! - 10: MinEdgeGate
//! - 15: SpreadViabilityGate
//! - 20: MaxExposureGate
//! - 22: DirectionExposureGate
//! - 25: NetExposureGate
//! - 30: MaxDrawdownGate / JumpDetectionGate
//! - 35: AdverseSelectionGate
//! - 40: MaxPositionsGate
//! - 45: VolatilityGate
//! - 50: EntryPriceGate / ConcurrentPositionGate
//! - 60: KellySizeGate

use crate::gate::{GateResult, GateSignal, RiskGate};

/// Rejects signals with insufficient edge to justify entry after fees.
///
/// Edge is the difference between our estimated probability and the market price.
/// The minimum edge threshold is adjusted per-asset based on Q7/Q10 volatility
/// research, and further scaled by cross-timeframe agreement.
///
/// Design: This gate runs early (priority 10) because edge is the most fundamental
/// check — if there's no edge, no amount of other analysis matters.
pub struct MinEdgeGate {
    /// Base minimum edge threshold (as a decimal, e.g. 0.035 = 3.5%).
    min_edge: f64,
}

impl MinEdgeGate {
    /// Create a new minimum edge gate.
    /// `min_edge`: base edge threshold as a decimal (e.g. 0.035 = 3.5%).
    pub fn new(min_edge: f64) -> Self {
        Self { min_edge }
    }
}

impl RiskGate for MinEdgeGate {
    fn name(&self) -> &str {
        "min-edge"
    }

    fn evaluate(&self, signal: &GateSignal) -> GateResult {
        // Per-asset edge multiplier based on Q7/Q10 volatility + spread research:
        // SOL: noisiest (0.217% 5m stddev, kurtosis 6.17) → raise threshold
        // BTC: most predictable (0.157% stddev) → standard
        // ETH: strongest dislocations, best spread predictability → slightly lower
        // XRP: BTC-like stability → standard
        let asset_multiplier = match signal.asset.as_str() {
            "BTC" => 1.0,
            "ETH" => 0.90,
            "SOL" => 1.4,
            "XRP" => 1.0,
            _ => 1.0,
        };

        // Raise threshold when timeframes disagree: full agreement → 1.0x, zero agreement → 1.15x
        let effective_min_edge = self.min_edge
            * asset_multiplier
            * (1.0 + 0.15 * (1.0 - signal.timeframe_agreement));

        if signal.edge < effective_min_edge {
            GateResult::Reject(format!(
                "Edge {:.2}% below minimum {:.2}% (asset={}, tf_agree={:.2})",
                signal.edge * 100.0,
                effective_min_edge * 100.0,
                signal.asset,
                signal.timeframe_agreement,
            ))
        } else {
            GateResult::Pass
        }
    }

    fn priority(&self) -> u32 {
        10
    }
}

/// Rejects signals that would push total portfolio exposure above a hard cap.
///
/// Total exposure = sum of all open position sizes (USD). This is a hard limit
/// that prevents the bot from over-committing capital regardless of individual
/// signal quality.
///
/// Priority 20 — runs after edge check but before sizing gates, since there's
/// no point sizing a position that would breach the exposure cap.
pub struct MaxExposureGate {
    /// Maximum total exposure across all positions (USD).
    max_exposure: f64,
}

impl MaxExposureGate {
    /// Create a new max exposure gate.
    /// `max_exposure`: hard cap on total portfolio exposure in USD.
    pub fn new(max_exposure: f64) -> Self {
        Self { max_exposure }
    }
}

impl RiskGate for MaxExposureGate {
    fn name(&self) -> &str {
        "max-exposure"
    }

    fn evaluate(&self, signal: &GateSignal) -> GateResult {
        let new_exposure = signal.current_total_exposure + signal.proposed_size_usd;
        if new_exposure > self.max_exposure {
            GateResult::Reject(format!(
                "Exposure ${:.0} + ${:.0} = ${:.0} exceeds max ${:.0}",
                signal.current_total_exposure,
                signal.proposed_size_usd,
                new_exposure,
                self.max_exposure,
            ))
        } else {
            GateResult::Pass
        }
    }

    fn priority(&self) -> u32 {
        20
    }
}

/// Circuit breaker — stops ALL trading when portfolio drawdown exceeds a threshold.
///
/// Drawdown = (peak_equity - current_equity) / peak_equity. When this exceeds
/// the configured limit, the gate rejects all new entries until equity recovers
/// or the session is reset.
///
/// This is the most critical safety gate. Uses `>=` comparison so that
/// hitting the exact threshold triggers the breaker (fail-safe behavior).
///
/// Priority 30 — runs in the middle of the chain. Not first because we want
/// edge and exposure checks to happen first (cheaper computations, and if the
/// circuit breaker is tripped, all signals would be rejected anyway).
pub struct MaxDrawdownGate {
    /// Maximum acceptable drawdown as a decimal (e.g. 0.08 = 8%).
    max_drawdown_pct: f64,
}

impl MaxDrawdownGate {
    /// Create a new max drawdown gate.
    /// `max_drawdown_pct`: threshold as a decimal (e.g. 0.08 = 8%).
    pub fn new(max_drawdown_pct: f64) -> Self {
        Self { max_drawdown_pct }
    }
}

impl RiskGate for MaxDrawdownGate {
    fn name(&self) -> &str {
        "max-drawdown"
    }

    fn evaluate(&self, signal: &GateSignal) -> GateResult {
        if signal.current_drawdown_pct >= self.max_drawdown_pct {
            GateResult::Reject(format!(
                "Drawdown {:.1}% >= circuit breaker {:.1}%",
                signal.current_drawdown_pct * 100.0,
                self.max_drawdown_pct * 100.0,
            ))
        } else {
            GateResult::Pass
        }
    }

    fn priority(&self) -> u32 {
        30
    }
}

/// Rejects signals when the total number of open positions is at the limit.
///
/// Prevents over-diversification and capital fragmentation. Too many small
/// positions dilute edge and increase operational complexity (monitoring,
/// settlement tracking).
///
/// Priority 40 — runs after exposure and drawdown checks.
pub struct MaxPositionsGate {
    /// Maximum number of concurrent open positions across all assets.
    max_positions: usize,
}

impl MaxPositionsGate {
    /// Create a new max positions gate.
    /// `max_positions`: maximum number of concurrent open positions.
    pub fn new(max_positions: usize) -> Self {
        Self { max_positions }
    }
}

impl RiskGate for MaxPositionsGate {
    fn name(&self) -> &str {
        "max-positions"
    }

    fn evaluate(&self, signal: &GateSignal) -> GateResult {
        if signal.current_position_count >= self.max_positions {
            GateResult::Reject(format!(
                "{} positions >= max {}",
                signal.current_position_count, self.max_positions,
            ))
        } else {
            GateResult::Pass
        }
    }

    fn priority(&self) -> u32 {
        40
    }
}

/// Filters out dead-zone entry prices and resizes expensive entries.
///
/// Binary token prices on Polymarket range from 0.0 to 1.0. Extreme prices
/// indicate near-certainty in one direction, which creates poor risk/reward:
/// - Very low prices (< min_price): High payout but extremely unlikely to win.
///   Model confidence is unreliable at these extremes.
/// - Very high prices (> max_price): Very likely to win but tiny payout.
///   Fees eat most of the profit.
///
/// Between `max_price - 0.10` and `max_price`, the gate scales down position
/// size linearly using a `(1 - price) * 4.0` multiplier. This creates a smooth
/// ramp-down rather than a hard cutoff.
///
/// The multiplier formula `(1 - price) * 4.0` is designed so that at `max_price`
/// (e.g. 0.85), multiplier = 0.60, and it decreases toward 0 as price approaches 1.0.
///
/// Priority 50 — runs after most other checks, since entry price filtering is
/// about execution quality rather than portfolio-level risk.
pub struct EntryPriceGate {
    /// Minimum acceptable entry price (e.g. 0.05). Below this = "too unlikely".
    min_price: f64,
    /// Maximum acceptable entry price (e.g. 0.85). Above this = "too expensive".
    max_price: f64,
}

impl EntryPriceGate {
    /// Create a new entry price gate.
    /// `min_price`: minimum acceptable price (e.g. 0.05).
    /// `max_price`: maximum acceptable price (e.g. 0.85).
    pub fn new(min_price: f64, max_price: f64) -> Self {
        Self { min_price, max_price }
    }
}

impl RiskGate for EntryPriceGate {
    fn name(&self) -> &str {
        "entry-price"
    }

    fn evaluate(&self, signal: &GateSignal) -> GateResult {
        let price = signal.entry_price;

        if price > self.max_price {
            return GateResult::Reject(format!(
                "Entry price {:.2} > {:.2} (too expensive, low payout)",
                price, self.max_price,
            ));
        }

        if price < self.min_price {
            return GateResult::Reject(format!(
                "Entry price {:.2} < {:.2} (too unlikely)",
                price, self.min_price,
            ));
        }

        // Resize zone: within 10 cents of max price, scale down linearly
        let resize_threshold = self.max_price - 0.10;
        if price > resize_threshold {
            let multiplier = (1.0 - price) * 4.0;
            return GateResult::Resize {
                multiplier,
                reason: format!(
                    "Entry price {:.2} > {:.2}, resize to {:.0}%",
                    price, resize_threshold,
                    multiplier * 100.0,
                ),
            };
        }

        GateResult::Pass
    }

    fn priority(&self) -> u32 {
        50
    }
}

/// Confidence-based flat position sizing.
///
/// Replaces Kelly criterion because our strategy p_up values are NOT calibrated
/// probabilities — they are directional signals mapped to [0,1]. Kelly requires
/// true win probabilities; feeding it uncalibrated scores amplifies errors.
///
/// Sizing: base_fraction × confidence × bankroll, scaled by timeframe agreement.
/// Still rejects when the model has no directional edge (p_up ≈ 0.5).
pub struct KellySizeGate {
    base_fraction: f64,
}

impl KellySizeGate {
    /// Create a new Kelly size gate.
    /// `base_fraction`: maximum fraction of bankroll for a single position
    /// at full confidence and full conviction (e.g. 0.25 = 25%).
    pub fn new(base_fraction: f64) -> Self {
        Self { base_fraction }
    }
}

impl RiskGate for KellySizeGate {
    fn name(&self) -> &str {
        "kelly-size"
    }

    fn evaluate(&self, signal: &GateSignal) -> GateResult {
        // Direction-aware "conviction": how far p_up deviates from 0.5
        let p = if signal.direction == "Up" {
            signal.p_up
        } else {
            1.0 - signal.p_up
        };

        // Reject if model has no directional conviction (p ≤ 0.5 means wrong direction)
        if p <= 0.50 {
            return GateResult::Reject(format!(
                "No directional edge: p={:.4} for {} (need >0.50)",
                p, signal.direction,
            ));
        }

        // Conviction = how far from 0.5 (range: 0.0 to 0.5, capped at 0.3)
        let conviction = (p - 0.5).min(0.3);

        // Per-asset Kelly scaling from Q7 volatility profiles:
        // BTC: most predictable → full fraction (1.0x)
        // ETH: strong signal but volatile → slightly reduced (0.90x)
        // SOL: noisiest, highest kurtosis → conservative (0.75x)
        // XRP: BTC-like → slightly reduced (0.85x)
        let asset_kelly_scale = match signal.asset.as_str() {
            "BTC" => 1.0,
            "ETH" => 0.90,
            "SOL" => 0.75,
            "XRP" => 0.85,
            _ => 1.0,
        };

        // Size = base_fraction × asset_scale × confidence × conviction_scale × bankroll
        // conviction_scale: 0 at p=0.5, 1.0 at p=0.8+
        let conviction_scale = conviction / 0.3;
        let size_fraction = self.base_fraction * asset_kelly_scale * signal.confidence * conviction_scale;

        // Scale by timeframe agreement: full agreement (1.0) → 1.0x, none (0.0) → 0.5x
        let size_fraction = size_fraction * (0.5 + 0.5 * signal.timeframe_agreement);

        let target_size = size_fraction * signal.bankroll;

        if target_size < 1.0 {
            return GateResult::Reject(format!(
                "Size too small: ${:.2} (conf={:.3}, conviction={:.3})",
                target_size, signal.confidence, conviction,
            ));
        }

        if target_size < signal.proposed_size_usd {
            let multiplier = target_size / signal.proposed_size_usd;
            GateResult::Resize {
                multiplier,
                reason: format!(
                    "Confidence size ${:.2} < proposed ${:.2}, resize to {:.0}%",
                    target_size, signal.proposed_size_usd, multiplier * 100.0,
                ),
            }
        } else {
            GateResult::Pass
        }
    }

    fn priority(&self) -> u32 {
        60
    }
}

/// Rejects bilateral trades when the spread is too tight for profit.
///
/// Bilateral market-making buys both Up and Down tokens simultaneously,
/// profiting from the spread between them. When `market_p_up + market_p_down > 0.97`,
/// the effective spread is less than 3 cents — insufficient to cover fees
/// (typically 2% taker fee) and still profit.
///
/// The 0.97 threshold is hardcoded because Polymarket's minimum fee structure
/// makes spreads below ~3 cents unprofitable after fees. At 2% taker fee on
/// both sides, minimum viable spread is ~4 cents; 0.97 gives a small buffer.
///
/// Only used in the `bilateral-mm` risk chain profile.
/// Priority 15 — runs very early because if the spread is unviable, no other
/// checks matter.
pub struct SpreadViabilityGate;

impl SpreadViabilityGate {
    /// Create a new spread viability gate (no configuration — threshold is hardcoded).
    pub fn new() -> Self {
        Self
    }
}

impl RiskGate for SpreadViabilityGate {
    fn name(&self) -> &str {
        "spread-viability"
    }

    fn evaluate(&self, signal: &GateSignal) -> GateResult {
        let total = signal.market_p_up + signal.market_p_down;
        if total > 0.97 {
            GateResult::Reject(format!(
                "spread too tight for bilateral (p_up={:.3} + p_down={:.3} = {:.3})",
                signal.market_p_up, signal.market_p_down, total
            ))
        } else {
            GateResult::Pass
        }
    }

    fn priority(&self) -> u32 {
        15
    }
}

/// Prevents net directional exposure from exceeding a threshold.
///
/// Net exposure = long_exposure - short_exposure (or Up_exposure - Down_exposure).
/// Capping this prevents the portfolio from becoming too directionally biased.
/// The limit is expressed as a fraction of bankroll.
///
/// Primarily used in the `bilateral-mm` profile where the bot holds both Up and
/// Down positions and needs to stay roughly neutral.
///
/// Priority 25 — after max-exposure (20) and direction-exposure (22).
pub struct NetExposureGate {
    /// Maximum net exposure as a fraction of bankroll (e.g. 0.3 = 30%).
    max_net_exposure: f64,
}

impl NetExposureGate {
    /// Create a new net exposure gate.
    /// `max_net_exposure`: max net directional exposure as a fraction of bankroll.
    pub fn new(max_net_exposure: f64) -> Self {
        Self { max_net_exposure }
    }
}

impl RiskGate for NetExposureGate {
    fn name(&self) -> &str {
        "net-exposure"
    }

    fn evaluate(&self, signal: &GateSignal) -> GateResult {
        let new_net = signal.current_net_exposure + signal.proposed_size_usd;
        if new_net > self.max_net_exposure * signal.bankroll {
            GateResult::Reject(format!(
                "Net exposure ${:.0} + ${:.0} = ${:.0} exceeds {:.0}% of bankroll ${:.0}",
                signal.current_net_exposure,
                signal.proposed_size_usd,
                new_net,
                self.max_net_exposure * 100.0,
                signal.bankroll,
            ))
        } else {
            GateResult::Pass
        }
    }

    fn priority(&self) -> u32 {
        25
    }
}

/// Protects against adverse selection on BTC 5-minute rounds.
///
/// BTC 5m rounds have the tightest spreads and highest participation from
/// sophisticated market makers (per Q15 research). When our edge is low (<5%),
/// it's likely that the market price is more accurate than our model, and we'd
/// be the "dumb money" getting picked off by informed traders.
///
/// The 5% edge threshold and BTC+5m targeting are hardcoded based on empirical
/// observations: BTC 5m rounds showed the highest adverse selection rate in
/// backtesting (Q15), while other assets and timeframes had less informed flow.
///
/// Only triggers for BTC 5m — other combinations pass unconditionally.
/// Priority 35.
pub struct AdverseSelectionGate;

impl AdverseSelectionGate {
    /// Create a new adverse selection gate (no configuration — thresholds are hardcoded).
    pub fn new() -> Self {
        Self
    }
}

impl RiskGate for AdverseSelectionGate {
    fn name(&self) -> &str {
        "adverse-selection"
    }

    fn evaluate(&self, signal: &GateSignal) -> GateResult {
        if signal.asset == "BTC" && signal.timeframe == "5m" && signal.edge < 0.05 {
            GateResult::Reject(
                "BTC 5m rounds have high adverse selection risk, need edge > 5%".into(),
            )
        } else {
            GateResult::Pass
        }
    }

    fn priority(&self) -> u32 {
        35
    }
}

/// Halves position size when current volatility exceeds a multiple of average.
///
/// High volatility means wider price swings, which increases the probability of
/// adverse fills and makes directional predictions less reliable. Rather than
/// rejecting outright, this gate resizes to 50% — still allowing entry but with
/// reduced risk.
///
/// The 50% multiplier is hardcoded as a simple, conservative response. More
/// sophisticated approaches (e.g. continuous vol-adjusted sizing) are deferred
/// to the strategy layer.
///
/// Passes unconditionally when `avg_volatility` is 0.0 (insufficient data to
/// compute a ratio).
///
/// Only included in the `stochastic-vol` and `lmsr-filter` risk chain profiles.
/// Priority 45.
pub struct VolatilityGate {
    /// Threshold ratio: current_vol must exceed this multiple of avg_vol to trigger.
    /// E.g. 2.0 means current volatility must be 2x the average.
    vol_threshold_ratio: f64,
}

impl VolatilityGate {
    /// Create a new volatility gate.
    /// `vol_threshold_ratio`: how many times above average vol triggers resize (e.g. 2.0).
    pub fn new(vol_threshold_ratio: f64) -> Self {
        Self { vol_threshold_ratio }
    }
}

impl RiskGate for VolatilityGate {
    fn name(&self) -> &str {
        "volatility"
    }

    fn evaluate(&self, signal: &GateSignal) -> GateResult {
        if signal.avg_volatility > 0.0
            && signal.current_volatility > self.vol_threshold_ratio * signal.avg_volatility
        {
            GateResult::Resize {
                multiplier: 0.5,
                reason: format!(
                    "Current vol {:.4} > {:.1}x avg vol {:.4}, halving size",
                    signal.current_volatility,
                    self.vol_threshold_ratio,
                    signal.avg_volatility,
                ),
            }
        } else {
            GateResult::Pass
        }
    }

    fn priority(&self) -> u32 {
        45
    }
}

/// Rejects signals when volatility indicates a price jump/spike.
///
/// Unlike VolatilityGate which resizes, JumpDetectionGate fully rejects the signal.
/// This is for more extreme events (e.g. 3x average vol) where the market is moving
/// too fast for our model to be reliable. During jumps, order book depth thins out,
/// fills become unpredictable, and directional models lag the true price.
///
/// Default threshold is 3.0 (3x average volatility), compared to VolatilityGate's 2.0.
/// This higher threshold means JumpDetectionGate only fires during genuine spikes,
/// not just elevated volatility.
///
/// Passes unconditionally when either current or average volatility is 0.0
/// (insufficient data).
///
/// Priority 30.
pub struct JumpDetectionGate {
    /// Threshold ratio: current_vol / avg_vol must exceed this to reject.
    /// Hardcoded default: 3.0 (used in all risk chain profiles).
    threshold: f64,
}

impl JumpDetectionGate {
    /// Create a new jump detection gate.
    /// `threshold`: vol ratio that triggers rejection (e.g. 3.0 = 3x average).
    pub fn new(threshold: f64) -> Self {
        Self { threshold }
    }
}

impl RiskGate for JumpDetectionGate {
    fn name(&self) -> &str {
        "jump-detection"
    }

    fn evaluate(&self, signal: &GateSignal) -> GateResult {
        if signal.current_volatility > 0.0 && signal.avg_volatility > 0.0 {
            if signal.current_volatility > self.threshold * signal.avg_volatility {
                return GateResult::Reject(format!(
                    "Jump detected: current vol {:.4} > {:.1}x avg vol {:.4}",
                    signal.current_volatility,
                    self.threshold,
                    signal.avg_volatility,
                ));
            }
        }
        GateResult::Pass
    }

    fn priority(&self) -> u32 {
        30
    }
}

/// Limits correlated directional exposure.
///
/// When multiple assets are positioned in the same direction (e.g., all Up),
/// a single market move can wipe out all positions simultaneously.
/// This gate caps same-direction exposure by count and by bankroll fraction.
pub struct DirectionExposureGate {
    max_same_direction: usize,
    max_same_direction_pct: f64,
}

impl DirectionExposureGate {
    /// Create a new direction exposure gate.
    /// `max_same_direction`: hard cap on number of positions in the same direction.
    /// `max_same_direction_pct`: soft cap as a fraction of bankroll (e.g. 0.6 = 60%).
    pub fn new(max_same_direction: usize, max_same_direction_pct: f64) -> Self {
        Self { max_same_direction, max_same_direction_pct }
    }
}

impl RiskGate for DirectionExposureGate {
    fn name(&self) -> &str {
        "direction-exposure"
    }

    fn evaluate(&self, signal: &GateSignal) -> GateResult {
        // Hard cap: reject if too many positions in the same direction
        if signal.same_direction_count >= self.max_same_direction {
            return GateResult::Reject(format!(
                "{} positions already {} (max {}), blocking correlated entry",
                signal.same_direction_count, signal.direction, self.max_same_direction,
            ));
        }

        // Soft cap: resize if same-direction exposure exceeds bankroll fraction
        if signal.bankroll > 0.0 {
            let new_exposure = signal.same_direction_exposure + signal.proposed_size_usd;
            let max_usd = signal.bankroll * self.max_same_direction_pct;
            if new_exposure > max_usd {
                let remaining = (max_usd - signal.same_direction_exposure).max(0.0);
                if remaining < 1.0 {
                    return GateResult::Reject(format!(
                        "{} exposure ${:.0} + ${:.0} exceeds {:.0}% bankroll cap ${:.0}",
                        signal.direction,
                        signal.same_direction_exposure,
                        signal.proposed_size_usd,
                        self.max_same_direction_pct * 100.0,
                        max_usd,
                    ));
                }
                let multiplier = remaining / signal.proposed_size_usd;
                return GateResult::Resize {
                    multiplier,
                    reason: format!(
                        "{} exposure capped at {:.0}% bankroll, resize to {:.0}%",
                        signal.direction,
                        self.max_same_direction_pct * 100.0,
                        multiplier * 100.0,
                    ),
                };
            }
        }

        GateResult::Pass
    }

    fn priority(&self) -> u32 {
        22  // After max-exposure (20), before net-exposure (25)
    }
}

/// Limits the number of concurrent open positions per individual asset.
///
/// Unlike MaxPositionsGate (which caps total positions across all assets), this
/// gate prevents over-concentration in a single asset. For example, if max_per_asset=2,
/// we can have at most 2 BTC positions and 2 ETH positions simultaneously.
///
/// This prevents a scenario where all capital ends up in one asset's rounds,
/// which would create concentrated risk to that asset's price movements.
///
/// Default max_per_asset is 2 (one position per timeframe is typical — e.g.
/// one BTC 5m and one BTC 15m position).
///
/// Priority 50.
pub struct ConcurrentPositionGate {
    /// Maximum number of open positions for any single asset.
    max_per_asset: usize,
}

impl ConcurrentPositionGate {
    /// Create a new concurrent position gate.
    /// `max_per_asset`: maximum open positions for any single asset.
    pub fn new(max_per_asset: usize) -> Self {
        Self { max_per_asset }
    }
}

impl RiskGate for ConcurrentPositionGate {
    fn name(&self) -> &str {
        "concurrent-position"
    }

    fn evaluate(&self, signal: &GateSignal) -> GateResult {
        if signal.current_asset_positions >= self.max_per_asset {
            GateResult::Reject(format!(
                "{} positions on {} >= max {} per asset",
                signal.current_asset_positions, signal.asset, self.max_per_asset,
            ))
        } else {
            GateResult::Pass
        }
    }

    fn priority(&self) -> u32 {
        50
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::{GateResult, GateSignal};

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
    fn test_min_edge_rejects_low_edge() {
        let gate = MinEdgeGate::new(0.035);
        let mut sig = test_signal();
        sig.edge = 0.02;
        assert!(matches!(gate.evaluate(&sig), GateResult::Reject(_)));
    }

    #[test]
    fn test_min_edge_passes_good_edge() {
        let gate = MinEdgeGate::new(0.035);
        let mut sig = test_signal();
        sig.edge = 0.05;
        assert!(matches!(gate.evaluate(&sig), GateResult::Pass));
    }

    #[test]
    fn test_max_exposure_rejects_over_limit() {
        let gate = MaxExposureGate::new(5000.0);
        let mut sig = test_signal();
        sig.current_total_exposure = 4980.0;
        sig.proposed_size_usd = 50.0;
        assert!(matches!(gate.evaluate(&sig), GateResult::Reject(_)));
    }

    #[test]
    fn test_max_drawdown_circuit_breaker() {
        let gate = MaxDrawdownGate::new(0.08);
        let mut sig = test_signal();
        sig.current_drawdown_pct = 0.09;
        assert!(matches!(gate.evaluate(&sig), GateResult::Reject(_)));

        // Exactly at threshold should also reject
        sig.current_drawdown_pct = 0.08;
        assert!(matches!(gate.evaluate(&sig), GateResult::Reject(_)));

        // Below threshold passes
        sig.current_drawdown_pct = 0.07;
        assert!(matches!(gate.evaluate(&sig), GateResult::Pass));
    }

    #[test]
    fn test_entry_price_dead_zone() {
        let gate = EntryPriceGate::new(0.05, 0.85);

        // Too expensive
        let mut sig = test_signal();
        sig.entry_price = 0.90;
        assert!(matches!(gate.evaluate(&sig), GateResult::Reject(_)));

        // Too unlikely
        sig.entry_price = 0.03;
        assert!(matches!(gate.evaluate(&sig), GateResult::Reject(_)));

        // Normal price passes
        sig.entry_price = 0.50;
        assert!(matches!(gate.evaluate(&sig), GateResult::Pass));
    }

    #[test]
    fn test_entry_price_custom_bounds() {
        let gate = EntryPriceGate::new(0.12, 0.88);

        // Below custom min
        let mut sig = test_signal();
        sig.entry_price = 0.10;
        assert!(matches!(gate.evaluate(&sig), GateResult::Reject(_)));

        // Above custom max
        sig.entry_price = 0.90;
        assert!(matches!(gate.evaluate(&sig), GateResult::Reject(_)));

        // Within bounds
        sig.entry_price = 0.50;
        assert!(matches!(gate.evaluate(&sig), GateResult::Pass));
    }

    #[test]
    fn test_entry_price_resize() {
        let gate = EntryPriceGate::new(0.05, 0.85);
        let mut sig = test_signal();
        sig.entry_price = 0.80;

        match gate.evaluate(&sig) {
            GateResult::Resize { multiplier, .. } => {
                // (1.0 - 0.80) * 4.0 = 0.8
                assert!((multiplier - 0.8).abs() < 1e-10);
            }
            other => panic!("Expected Resize, got {:?}", other),
        }

        // At 0.75 boundary → resize_threshold = 0.85 - 0.10 = 0.75, > 0.75 is false, it passes
        sig.entry_price = 0.75;
        assert!(matches!(gate.evaluate(&sig), GateResult::Pass));
    }

    #[test]
    fn test_confidence_sizing_resizes_down() {
        let gate = KellySizeGate::new(0.25);
        let mut sig = test_signal();
        sig.p_up = 0.6;  // conviction = 0.1
        sig.confidence = 0.3;
        sig.entry_price = 0.50;
        sig.bankroll = 1000.0;
        sig.proposed_size_usd = 100.0;
        sig.timeframe_agreement = 1.0;

        // conviction = (0.6 - 0.5).min(0.3) = 0.1
        // conviction_scale = 0.1 / 0.3 = 0.333
        // size_fraction = 0.25 * 0.3 * 0.333 = 0.025
        // tf_scale = 0.5 + 0.5 * 1.0 = 1.0
        // target_size = 0.025 * 1000 = 25.0
        // 25 < 100 → resize, multiplier = 0.25
        match gate.evaluate(&sig) {
            GateResult::Resize { multiplier, .. } => {
                assert!((multiplier - 0.25).abs() < 0.01);
            }
            other => panic!("Expected Resize, got {:?}", other),
        }
    }

    #[test]
    fn test_confidence_sizing_rejects_no_conviction() {
        let gate = KellySizeGate::new(0.25);
        let mut sig = test_signal();
        sig.p_up = 0.3;  // direction is Up but p_up < 0.5 → no edge
        sig.entry_price = 0.50;

        assert!(matches!(gate.evaluate(&sig), GateResult::Reject(_)));
    }

    #[test]
    fn test_confidence_sizing_down_direction() {
        let gate = KellySizeGate::new(0.25);
        let mut sig = test_signal();
        sig.direction = "Down".into();
        sig.p_up = 0.264; // p_down = 0.736, conviction = 0.236
        sig.confidence = 0.35;
        sig.entry_price = 0.61;
        sig.bankroll = 1000.0;
        sig.proposed_size_usd = 50.0;
        sig.timeframe_agreement = 1.0;

        // p = 1 - 0.264 = 0.736, conviction = min(0.236, 0.3) = 0.236
        // conviction_scale = 0.236 / 0.3 = 0.787
        // size_fraction = 0.25 * 0.35 * 0.787 = 0.0689
        // target = 0.0689 * 1000 = 68.9 > 50 → Pass
        assert!(matches!(gate.evaluate(&sig), GateResult::Pass));
    }

    #[test]
    fn test_confidence_sizing_down_rejects_wrong_direction() {
        let gate = KellySizeGate::new(0.25);
        let mut sig = test_signal();
        sig.direction = "Down".into();
        sig.p_up = 0.7; // p_down = 0.3 — betting Down but model says Up
        sig.entry_price = 0.40;

        // p = 1 - 0.7 = 0.3, p <= 0.5 → reject
        assert!(matches!(gate.evaluate(&sig), GateResult::Reject(_)));
    }

    // --- SpreadViabilityGate tests ---

    #[test]
    fn test_spread_viability_rejects_tight_spread() {
        // market_p_up=0.52 + market_p_down=0.48 = 1.00 > 0.97
        let gate = SpreadViabilityGate::new();
        let sig = test_signal();
        assert!(matches!(gate.evaluate(&sig), GateResult::Reject(_)));
    }

    #[test]
    fn test_spread_viability_passes_wide_spread() {
        let gate = SpreadViabilityGate::new();
        let mut sig = test_signal();
        sig.market_p_up = 0.48;
        sig.market_p_down = 0.46; // total = 0.94 < 0.97
        assert!(matches!(gate.evaluate(&sig), GateResult::Pass));
    }

    #[test]
    fn test_spread_viability_priority() {
        let gate = SpreadViabilityGate::new();
        assert_eq!(gate.priority(), 15);
    }

    // --- NetExposureGate tests ---

    #[test]
    fn test_net_exposure_rejects_over_limit() {
        let gate = NetExposureGate::new(0.3);
        let mut sig = test_signal();
        sig.bankroll = 1000.0;
        sig.current_net_exposure = 280.0;
        sig.proposed_size_usd = 50.0;
        // 280 + 50 = 330 > 0.3 * 1000 = 300
        assert!(matches!(gate.evaluate(&sig), GateResult::Reject(_)));
    }

    #[test]
    fn test_net_exposure_passes_under_limit() {
        let gate = NetExposureGate::new(0.3);
        let mut sig = test_signal();
        sig.bankroll = 1000.0;
        sig.current_net_exposure = 200.0;
        sig.proposed_size_usd = 50.0;
        // 200 + 50 = 250 < 300
        assert!(matches!(gate.evaluate(&sig), GateResult::Pass));
    }

    #[test]
    fn test_net_exposure_priority() {
        let gate = NetExposureGate::new(0.3);
        assert_eq!(gate.priority(), 25);
    }

    // --- AdverseSelectionGate tests ---

    #[test]
    fn test_adverse_selection_rejects_btc_5m_low_edge() {
        let gate = AdverseSelectionGate::new();
        let mut sig = test_signal();
        sig.asset = "BTC".into();
        sig.timeframe = "5m".into();
        sig.edge = 0.03;
        assert!(matches!(gate.evaluate(&sig), GateResult::Reject(_)));
    }

    #[test]
    fn test_adverse_selection_passes_btc_5m_high_edge() {
        let gate = AdverseSelectionGate::new();
        let mut sig = test_signal();
        sig.asset = "BTC".into();
        sig.timeframe = "5m".into();
        sig.edge = 0.06;
        assert!(matches!(gate.evaluate(&sig), GateResult::Pass));
    }

    #[test]
    fn test_adverse_selection_passes_non_btc() {
        let gate = AdverseSelectionGate::new();
        let mut sig = test_signal();
        sig.asset = "ETH".into();
        sig.timeframe = "5m".into();
        sig.edge = 0.03;
        assert!(matches!(gate.evaluate(&sig), GateResult::Pass));
    }

    #[test]
    fn test_adverse_selection_passes_non_5m() {
        let gate = AdverseSelectionGate::new();
        let mut sig = test_signal();
        sig.asset = "BTC".into();
        sig.timeframe = "15m".into();
        sig.edge = 0.03;
        assert!(matches!(gate.evaluate(&sig), GateResult::Pass));
    }

    #[test]
    fn test_adverse_selection_priority() {
        let gate = AdverseSelectionGate::new();
        assert_eq!(gate.priority(), 35);
    }

    // --- VolatilityGate tests ---

    #[test]
    fn test_volatility_resizes_when_high() {
        let gate = VolatilityGate::new(2.0);
        let mut sig = test_signal();
        sig.current_volatility = 0.05;
        sig.avg_volatility = 0.02;
        // 0.05 > 2.0 * 0.02 = 0.04
        match gate.evaluate(&sig) {
            GateResult::Resize { multiplier, .. } => {
                assert!((multiplier - 0.5).abs() < 1e-10);
            }
            other => panic!("Expected Resize, got {:?}", other),
        }
    }

    #[test]
    fn test_volatility_passes_when_normal() {
        let gate = VolatilityGate::new(2.0);
        let mut sig = test_signal();
        sig.current_volatility = 0.03;
        sig.avg_volatility = 0.02;
        // 0.03 < 2.0 * 0.02 = 0.04
        assert!(matches!(gate.evaluate(&sig), GateResult::Pass));
    }

    #[test]
    fn test_volatility_passes_when_no_avg() {
        let gate = VolatilityGate::new(2.0);
        let mut sig = test_signal();
        sig.current_volatility = 0.05;
        sig.avg_volatility = 0.0;
        // avg_volatility is 0, gate should pass
        assert!(matches!(gate.evaluate(&sig), GateResult::Pass));
    }

    #[test]
    fn test_volatility_priority() {
        let gate = VolatilityGate::new(2.0);
        assert_eq!(gate.priority(), 45);
    }

    // --- JumpDetectionGate tests ---

    #[test]
    fn test_jump_detection_rejects_spike() {
        let gate = JumpDetectionGate::new(3.0);
        let mut sig = test_signal();
        sig.current_volatility = 0.10;
        sig.avg_volatility = 0.02;
        // 0.10 > 3.0 * 0.02 = 0.06
        assert!(matches!(gate.evaluate(&sig), GateResult::Reject(_)));
    }

    #[test]
    fn test_jump_detection_passes_normal() {
        let gate = JumpDetectionGate::new(3.0);
        let mut sig = test_signal();
        sig.current_volatility = 0.05;
        sig.avg_volatility = 0.02;
        // 0.05 < 3.0 * 0.02 = 0.06
        assert!(matches!(gate.evaluate(&sig), GateResult::Pass));
    }

    #[test]
    fn test_jump_detection_passes_no_data() {
        let gate = JumpDetectionGate::new(3.0);
        let sig = test_signal();
        // current_volatility=0 and avg_volatility=0 → insufficient data, pass
        assert!(matches!(gate.evaluate(&sig), GateResult::Pass));
    }

    #[test]
    fn test_jump_detection_priority() {
        let gate = JumpDetectionGate::new(3.0);
        assert_eq!(gate.priority(), 30);
    }

    // --- ConcurrentPositionGate tests ---

    #[test]
    fn test_concurrent_position_rejects_at_limit() {
        let gate = ConcurrentPositionGate::new(2);
        let mut sig = test_signal();
        sig.current_asset_positions = 2;
        assert!(matches!(gate.evaluate(&sig), GateResult::Reject(_)));
    }

    #[test]
    fn test_concurrent_position_rejects_over_limit() {
        let gate = ConcurrentPositionGate::new(2);
        let mut sig = test_signal();
        sig.current_asset_positions = 5;
        assert!(matches!(gate.evaluate(&sig), GateResult::Reject(_)));
    }

    #[test]
    fn test_concurrent_position_passes_under_limit() {
        let gate = ConcurrentPositionGate::new(2);
        let mut sig = test_signal();
        sig.current_asset_positions = 1;
        assert!(matches!(gate.evaluate(&sig), GateResult::Pass));
    }

    #[test]
    fn test_concurrent_position_passes_zero() {
        let gate = ConcurrentPositionGate::new(2);
        let sig = test_signal();
        // current_asset_positions = 0 (default)
        assert!(matches!(gate.evaluate(&sig), GateResult::Pass));
    }

    #[test]
    fn test_concurrent_position_priority() {
        let gate = ConcurrentPositionGate::new(2);
        assert_eq!(gate.priority(), 50);
    }

    // --- DirectionExposureGate tests ---

    #[test]
    fn test_direction_exposure_rejects_at_count_limit() {
        let gate = DirectionExposureGate::new(3, 0.6);
        let mut sig = test_signal();
        sig.same_direction_count = 3;
        assert!(matches!(gate.evaluate(&sig), GateResult::Reject(_)));
    }

    #[test]
    fn test_direction_exposure_passes_under_count() {
        let gate = DirectionExposureGate::new(3, 0.6);
        let mut sig = test_signal();
        sig.same_direction_count = 2;
        sig.same_direction_exposure = 100.0;
        sig.bankroll = 500.0;
        sig.proposed_size_usd = 50.0;
        // 100 + 50 = 150 < 0.6 * 500 = 300
        assert!(matches!(gate.evaluate(&sig), GateResult::Pass));
    }

    #[test]
    fn test_direction_exposure_resizes_over_pct() {
        let gate = DirectionExposureGate::new(3, 0.6);
        let mut sig = test_signal();
        sig.same_direction_count = 2;
        sig.same_direction_exposure = 250.0;
        sig.bankroll = 500.0;
        sig.proposed_size_usd = 100.0;
        // 250 + 100 = 350 > 0.6 * 500 = 300, remaining = 50
        match gate.evaluate(&sig) {
            GateResult::Resize { multiplier, .. } => {
                assert!((multiplier - 0.5).abs() < 0.01); // 50/100 = 0.5
            }
            other => panic!("Expected Resize, got {:?}", other),
        }
    }

    #[test]
    fn test_direction_exposure_rejects_when_full() {
        let gate = DirectionExposureGate::new(3, 0.6);
        let mut sig = test_signal();
        sig.same_direction_count = 2;
        sig.same_direction_exposure = 300.0;
        sig.bankroll = 500.0;
        sig.proposed_size_usd = 50.0;
        // 300 >= 300 cap, remaining = 0
        assert!(matches!(gate.evaluate(&sig), GateResult::Reject(_)));
    }

    #[test]
    fn test_direction_exposure_priority() {
        let gate = DirectionExposureGate::new(3, 0.6);
        assert_eq!(gate.priority(), 22);
    }
}

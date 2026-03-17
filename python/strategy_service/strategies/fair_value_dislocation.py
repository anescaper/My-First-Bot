"""Fair Value Dislocation Strategy — detects when token prices diverge from fair value.

Instead of predicting price direction, this strategy:
1. Computes fair value of the UP token from spot-vs-reference + time + volatility
2. Compares to actual market token price
3. Trades when dislocation exceeds fee threshold

Uses Q2 empirical calibration data (validated 2026-03-15 with 3,259 settled markets):
  - Market is systematically mispriced by 11.2% average
  - Tokens above 60% are underpriced (settle UP more than price implies)
  - Tokens below 40% are overpriced (settle UP less than price implies)

Also uses Q4 parent lock data:
  - When 15m is >75% UP, child 5m rounds settle UP 76.9% of the time
  - When 15m is <25% UP, child 5m rounds settle DOWN 74.5% of the time
"""

import math
from typing import Optional

from strategy_service.models import PredictionRequest, StrategyResult
from strategy_service.strategies.base import Strategy
from strategy_service.strategies.registry import register_strategy


# Q2 empirical calibration curve at 5% price resolution.
# Maps: market token price -> true settlement probability.
#
# Source: 3,259 settled crypto round markets over a 48-hour sample (2026-03-15).
# Methodology: grouped all settled markets by their mid-round token price into
# 5% buckets, then computed what fraction actually settled UP in each bucket.
#
# Key finding: the market is systematically mispriced:
# - Tokens priced 0.50 settle UP 62.2% of the time (should be 50% if well-calibrated)
# - Tokens priced 0.40 settle UP 36.7% (close to calibrated)
# - Tokens priced 0.65 settle UP 89.2% (massive underpricing of UP at high prices)
# - Tokens priced 0.20 settle UP 1.1% (market overestimates UP probability)
#
# The average mispricing is 11.2%, providing consistent edge for dislocation trading.
# These values are hardcoded because they represent a structural market inefficiency
# that changes slowly (recalibrate quarterly).
Q2_CALIBRATION = {
    0.05: 0.050, 0.10: 0.050, 0.15: 0.038, 0.20: 0.011,
    0.25: 0.034, 0.30: 0.130, 0.35: 0.223, 0.40: 0.367,
    0.45: 0.448, 0.50: 0.622, 0.55: 0.670, 0.60: 0.719,
    0.65: 0.892, 0.70: 0.958, 0.75: 0.968, 0.80: 0.978,
    0.85: 0.979, 0.90: 0.958, 0.95: 1.000,
}

# Typical intra-round volatility per timeframe.
# These are used by the parametric fair value model (_parametric_fair_value) as the
# sigma parameter for the normal CDF calculation.
#
# Derived from 7 days of Binance 1-min klines for BTC (the most liquid asset).
# These are approximate values that work well enough for fair value estimation.
# The parametric model is secondary to the Q2 calibration curve, so precision
# here is less critical. Values are expressed as fractional price moves:
# 0.0005 = 0.05% expected move over a 5-minute window.
TYPICAL_VOL = {
    "5m": 0.0005,    # ~0.05% per 5min window
    "15m": 0.001,    # ~0.10% per 15min window
    "1h": 0.002,     # ~0.20% per 1h window
}

# Max entry progress per timeframe (from issue #55 research).
# The strategy only enters during the early portion of a round because:
# 1. Late entries have less time for the dislocation to resolve.
# 2. Token prices converge toward settlement probability as expiry approaches.
# 3. The parametric model becomes less useful as uncertainty shrinks.
#
# Shorter timeframes allow later entry because the total round is short --
# missing the first 60% of a 5m round still leaves 2 minutes for the trade.
# Longer timeframes need earlier entry (30% of 1h = 18 min) because the
# market has more time to correct the dislocation before we enter.
MAX_ENTRY_PROGRESS = {
    "5m": 0.60,   # First 3 minutes of 5min round
    "15m": 0.50,  # First 7.5 minutes of 15min round
    "1h": 0.30,   # First 18 minutes of 1h round
}


def _interpolate_calibration(market_price: float) -> float:
    """Linearly interpolate the Q2 calibration curve.

    Clamps the input to [0.05, 0.95] to avoid extrapolation at extremes
    where sample sizes are smallest and the calibration is least reliable.

    Args:
        market_price: Current market price of the UP token (0.0 to 1.0).

    Returns:
        Interpolated true settlement probability from the Q2 calibration table.
    """
    market_price = max(0.05, min(0.95, market_price))
    keys = sorted(Q2_CALIBRATION.keys())
    for i in range(len(keys) - 1):
        if keys[i] <= market_price <= keys[i + 1]:
            t = (market_price - keys[i]) / (keys[i + 1] - keys[i])
            return Q2_CALIBRATION[keys[i]] * (1 - t) + Q2_CALIBRATION[keys[i + 1]] * t
    return Q2_CALIBRATION[keys[-1]]


def _parametric_fair_value(
    spot: float, ref: float, vol: float, time_remaining_frac: float
) -> float:
    """Compute P(spot > ref at expiry) using a normal CDF model.

    Assumes log-normal returns (standard GBM assumption). The displacement
    (spot - ref) / ref gives the current drift, and vol * sqrt(time_remaining)
    gives the remaining uncertainty. The ratio forms a z-score.

    This is less accurate than the Q2 calibration but provides real-time
    directional information when spot has moved significantly from ref.
    When spot ~= ref (flat), this returns ~0.5 (uninformative).

    Args:
        spot: Current spot price.
        ref: Reference (strike) price at round start.
        vol: Expected volatility over the remaining time (fractional, not %).
        time_remaining_frac: Fraction of round remaining (0.0 to 1.0).

    Returns:
        Probability that spot will be above ref at expiry (0.0 to 1.0).
    """
    if ref <= 0 or spot <= 0 or vol <= 0:
        return 0.5
    if time_remaining_frac <= 0.01:
        return 1.0 if spot > ref else 0.0

    displacement = (spot - ref) / ref
    uncertainty = vol * math.sqrt(time_remaining_frac)
    if uncertainty < 1e-8:
        return 1.0 if displacement > 0 else 0.0

    z = displacement / uncertainty
    # Standard normal CDF approximation
    return 0.5 * (1.0 + math.erf(z / math.sqrt(2.0)))


@register_strategy
class FairValueDislocationStrategy(Strategy):
    """Detects when Polymarket token prices diverge from computed fair value.

    Unlike directional strategies (GARCH, Monte Carlo) which predict price movement,
    this strategy computes fair value of the UP token and trades when the market
    price is significantly different (dislocated) from fair value.

    Two fair value models are combined:
    1. Q2 Empirical Calibration: historical settlement probability given market price.
    2. Parametric Model: P(spot > ref at expiry) using normal CDF of displacement.

    The weighting between models shifts based on displacement strength: when spot
    is flat (near ref), the parametric model is uninformative and Q2 dominates.
    When spot has moved significantly, parametric provides directional info.
    """

    def name(self) -> str:
        return "fair_value_dislocation"

    def required_data(self) -> list[str]:
        return []  # Only needs round info + prices (no candles)

    def weight(self) -> float:
        return 5.0  # High weight -- this is the primary dislocation strategy

    def min_data_points(self) -> int:
        return 0  # Does not need candle history

    def predict(self, ctx: PredictionRequest) -> Optional[StrategyResult]:
        """Compute fair value and trade dislocations.

        Abstains when: too early or too late in round, market price at extremes
        (already decided), or dislocation is smaller than fee threshold (0.04).

        The 0.04 minimum dislocation threshold covers:
        - 2% Polymarket entry fee
        - ~1% bid-ask spread
        - ~1% buffer for price movement during execution

        Args:
            ctx: Prediction context with round info, spot/ref prices.

        Returns:
            StrategyResult with p_up set to the computed fair value and
            hold_to_resolution=True (positions held until round settlement).
        """
        meta = {}

        # --- Round timing gate ---
        # Compute progress from seconds_remaining (more reliable than progress_pct
        # which may use market creation time instead of round start time)
        tf = ctx.round.timeframe
        tf_secs = max(ctx.round.timeframe_seconds, 1)
        progress = 1.0 - (ctx.round.seconds_remaining / tf_secs)
        progress = max(0.0, min(1.0, progress))

        max_progress = MAX_ENTRY_PROGRESS.get(tf, 0.50)
        if progress > max_progress:
            return None  # Too late in the round

        # Don't enter in the first 10% either (let price establish)
        if progress < 0.10:
            return None

        market_price_up = ctx.round.price_up
        if market_price_up <= 0.02 or market_price_up >= 0.98:
            return None  # Already decided, no edge

        # --- Fair Value Model 1: Q2 Empirical Calibration ---
        q2_true_prob = _interpolate_calibration(market_price_up)
        q2_dislocation = market_price_up - q2_true_prob
        meta["q2_true_prob"] = round(q2_true_prob, 4)
        meta["q2_dislocation"] = round(q2_dislocation, 4)

        # --- Fair Value Model 2: Parametric (spot vs reference) ---
        spot = ctx.current_price
        ref = ctx.reference_price
        vol = TYPICAL_VOL.get(tf, 0.001)
        time_remaining_frac = ctx.round.seconds_remaining / tf_secs

        parametric_fv = _parametric_fair_value(spot, ref, vol, time_remaining_frac)
        parametric_dislocation = market_price_up - parametric_fv
        meta["parametric_fv"] = round(parametric_fv, 4)
        meta["parametric_dislocation"] = round(parametric_dislocation, 4)

        # --- Combined fair value (weighted average) ---
        # When spot ≈ ref (flat price), the parametric model is uninformative (≈0.5).
        # In that case, shift weight toward 50/50 split instead of letting Q2 dominate,
        # since Q2 is a static calibration that doesn't reflect real-time conditions.
        abs_displacement = abs(spot - ref) / ref if ref > 0 else 0.0
        # Parametric weight scales with displacement: 0.4 at full, 0.7 at flat
        # This means at flat: fair_value ≈ 0.3*Q2 + 0.7*0.5 ≈ closer to 0.5
        displacement_strength = min(abs_displacement / 0.003, 1.0)  # saturates at 0.3% move
        q2_weight = 0.35 + 0.25 * displacement_strength  # 0.35 → 0.60
        parametric_weight = 1.0 - q2_weight                # 0.65 → 0.40
        fair_value = q2_weight * q2_true_prob + parametric_weight * parametric_fv
        dislocation = market_price_up - fair_value
        meta["fair_value"] = round(fair_value, 4)
        meta["dislocation"] = round(dislocation, 4)
        meta["displacement_strength"] = round(displacement_strength, 4)

        # --- Parent lock boost (Q4 validated: 75% accuracy) ---
        parent_boost = 0.0
        if ctx.timeframe_intel:
            if ctx.timeframe_intel.agreement_score > 0.5:
                parent_boost = 0.15  # Parent locked UP
                meta["parent_lock"] = "UP"
            elif ctx.timeframe_intel.agreement_score < -0.5:
                parent_boost = 0.15  # Parent locked DOWN
                meta["parent_lock"] = "DOWN"

        # --- Direction and confidence ---
        # Negative dislocation = UP token is cheap → buy UP
        # Positive dislocation = UP token is expensive → buy DOWN
        abs_dislocation = abs(dislocation)

        # Minimum dislocation to cover fees (2% entry + ~1% spread)
        min_dislocation = 0.04
        if abs_dislocation < min_dislocation:
            return None  # Not enough edge to cover fees

        # Direction: buy the underpriced side
        if dislocation < 0:
            # UP token is cheap relative to fair value → buy UP
            direction_up = True
            p_up = fair_value  # Our estimate of true probability
        else:
            # UP token is expensive → buy DOWN
            direction_up = False
            p_up = fair_value

        # Confidence scales with dislocation magnitude
        base_confidence = min(abs_dislocation / 0.30, 1.0)  # Cap at 30¢ dislocation
        # Dampen confidence when price is flat — parametric model provides no directional info
        # displacement_strength = 0 (flat) → multiply by 0.3, at full move → 1.0
        flat_damper = 0.3 + 0.7 * displacement_strength
        base_confidence *= flat_damper
        confidence = min(base_confidence + parent_boost, 0.95)
        meta["flat_damper"] = round(flat_damper, 4)

        # Check parent lock agreement
        if ctx.timeframe_intel and parent_boost > 0:
            parent_agrees = (
                (ctx.timeframe_intel.agreement_score > 0.5 and direction_up) or
                (ctx.timeframe_intel.agreement_score < -0.5 and not direction_up)
            )
            if not parent_agrees:
                confidence *= 0.5  # Halve confidence if parent disagrees
                meta["parent_conflict"] = True

        meta["direction"] = "Up" if direction_up else "Down"
        meta["abs_dislocation"] = round(abs_dislocation, 4)
        meta["base_confidence"] = round(base_confidence, 4)
        meta["confidence"] = round(confidence, 4)
        meta["spot"] = round(spot, 2)
        meta["ref"] = round(ref, 2)
        meta["market_price_up"] = round(market_price_up, 4)
        meta["progress_pct"] = round(progress, 3)

        return StrategyResult(
            p_up=p_up,
            confidence=confidence,
            hold_to_resolution=True,
            max_entry_price=0.85,  # Wide range — we trade at extremes
            meta=meta,
        )

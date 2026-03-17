"""Hierarchical Cascade Strategy — multi-timeframe agreement signal.

Uses cross-timeframe alignment from Q15 research (11,348 markets):
  1h+15m agree UP  → 77.8% P(UP)  for 5m child
  1h+15m agree DOWN → 77.8% P(DOWN) for 5m child
  Conflict: child (15m) overrides parent (1h)
  Both neutral: skip

This strategy fires on ANY round regardless of child_index, providing
a baseline signal when Brownian Bridge has no pattern (child_index == 0
or parent direction unknown).

Per-asset edge thresholds from Q7/Q10:
  BTC: most predictable, standard threshold
  ETH: strongest vol clustering (1.68x), develops dislocations fastest
  SOL: noisiest, highest threshold to filter noise
  XRP: BTC-like stability, standard threshold
"""

from typing import Optional

from strategy_service.models import PredictionRequest, StrategyResult
from strategy_service.strategies.base import Strategy
from strategy_service.strategies.registry import register_strategy


# Per-asset minimum edge threshold (from Q7 volatility profiles).
# SOL requires higher edge (0.05) because its intra-round vol is 2-3x BTC's,
# making it harder to distinguish real cascade signals from vol noise.
# BTC, ETH, XRP use the standard 0.03 threshold (covers 2% fee + 1% spread).
ASSET_EDGE_THRESHOLD = {
    "BTC": 0.03,
    "ETH": 0.03,
    "SOL": 0.05,
    "XRP": 0.03,
}

# Per-asset confidence scaling (from Q10 spread-vs-settlement prediction accuracy).
# ETH (0.90) has the highest predictability because its vol clustering ratio (1.68x)
# means cascade signals develop faster and more reliably.
# SOL (0.75) has the lowest because its spread signals are noisiest.
# These values multiply the base confidence computed from parent agreement strength.
ASSET_CONFIDENCE_SCALE = {
    "BTC": 0.85,
    "ETH": 0.90,
    "SOL": 0.75,
    "XRP": 0.80,
}


@register_strategy
class HierarchicalCascadeStrategy(Strategy):
    """Multi-timeframe cascade agreement signal.

    Uses cross-timeframe alignment from the Rust pipeline's TimeframeIntel:
    when parent timeframes (1h, 15m) agree on direction, the child round
    (5m) follows with ~78% probability (from Q15 research, n=108).

    This strategy fires on ANY round regardless of child_index, providing a
    baseline directional signal when Brownian Bridge has no pattern (child_index=0
    or parent direction unknown). It is the secondary strategy in the 'fair-value'
    profile.

    Hardcoded probability values:
    - 0.778: two parents agree (Q15 empirical: 77.8%)
    - 0.75: single parent agrees (weaker signal)
    - 0.65: conflict between parents (child overrides parent at 65%)
    - 0.60/0.40: threshold for parent direction classification (neutral zone)
    """

    def name(self) -> str:
        return "hierarchical_cascade"

    def required_data(self) -> list[str]:
        return []  # Only needs timeframe_intel from the Rust pipeline

    def weight(self) -> float:
        return 4.0  # Below Brownian Bridge (8.0), above weak z-score signals

    def min_data_points(self) -> int:
        return 0  # No candle data needed

    def predict(self, ctx: PredictionRequest) -> Optional[StrategyResult]:
        """Predict based on cross-timeframe parent agreement.

        Abstains when: no timeframe_intel, round is 1h (no parents), all parents
        neutral, or insufficient edge to cover fees.

        Args:
            ctx: Prediction context with timeframe_intel populated by Rust.

        Returns:
            StrategyResult or None.
        """
        ti = ctx.timeframe_intel
        if ti is None:
            return None

        asset = ctx.round.asset
        tf = ctx.round.timeframe
        meta = {"asset": asset, "timeframe": tf}

        # Get parent timeframe biases from market_bias map
        # For 5m: parents are 15m and 1h
        # For 15m: parent is 1h
        # For 1h: no parents → skip
        parent_tfs = _get_parent_timeframes(tf)
        if not parent_tfs:
            return None

        # Collect parent directions from market_bias
        parent_signals = {}
        for ptf in parent_tfs:
            bias = ti.market_bias.get(ptf, 0.5)
            if bias > 0.60:
                parent_signals[ptf] = ("Up", bias)
            elif bias < 0.40:
                parent_signals[ptf] = ("Down", 1.0 - bias)
            else:
                parent_signals[ptf] = ("Neutral", 0.5)

        meta["parent_signals"] = {
            k: {"direction": v[0], "confidence": round(v[1], 3)}
            for k, v in parent_signals.items()
        }

        # Determine cascade signal
        directions = [v[0] for v in parent_signals.values()]
        confidences = [v[1] for v in parent_signals.values()]

        if all(d == "Up" for d in directions) and len(directions) >= 1:
            # All parents agree UP
            p_up = 0.778 if len(directions) >= 2 else 0.75
            signal = "cascade_up"
        elif all(d == "Down" for d in directions) and len(directions) >= 1:
            # All parents agree DOWN
            p_up = 0.222 if len(directions) >= 2 else 0.25
            signal = "cascade_down"
        elif "Up" in directions and "Down" in directions:
            # Conflict: child (closer timeframe) overrides parent
            # Child = first parent_tf (15m for 5m rounds)
            child_dir, child_conf = parent_signals[parent_tfs[0]]
            if child_dir == "Up":
                p_up = 0.65
                signal = "cascade_conflict_child_up"
            else:
                p_up = 0.35
                signal = "cascade_conflict_child_down"
            confidences = [child_conf]
        else:
            # At least one neutral → weak signal
            non_neutral = [(d, c) for d, c in zip(directions, confidences) if d != "Neutral"]
            if not non_neutral:
                return None  # Both neutral → skip (Q15: coin flip at 54.6%)
            # One parent has direction, other neutral
            d, c = non_neutral[0]
            if d == "Up":
                p_up = 0.65
                signal = "cascade_partial_up"
            else:
                p_up = 0.35
                signal = "cascade_partial_down"
            confidences = [c * 0.7]

        meta["signal"] = signal

        # Confidence from parent agreement strength
        avg_conf = sum(confidences) / len(confidences)
        # Scale: parent at 0.60 threshold → 0.29 base, parent at 0.85+ → 1.0 base
        base_confidence = min((avg_conf - 0.5) / (0.85 - 0.5), 1.0)
        base_confidence = max(base_confidence, 0.1)

        # Per-asset scaling
        asset_scale = ASSET_CONFIDENCE_SCALE.get(asset, 0.80)
        confidence = base_confidence * asset_scale

        # Edge check: is the dislocation big enough?
        market_price_up = ctx.round.price_up
        edge = abs(p_up - market_price_up) * confidence
        min_edge = ASSET_EDGE_THRESHOLD.get(asset, 0.03)

        if edge < min_edge:
            meta["skip_reason"] = f"edge {edge:.4f} < threshold {min_edge}"
            return None

        meta["confidence"] = round(confidence, 3)
        meta["edge"] = round(edge, 4)
        meta["avg_parent_confidence"] = round(avg_conf, 3)

        return StrategyResult(
            p_up=p_up,
            confidence=confidence,
            hold_to_resolution=True,
            meta=meta,
        )


def _get_parent_timeframes(tf: str) -> list[str]:
    """Return parent timeframe slugs for a given timeframe.

    The hierarchy is: 5m -> [15m, 1h], 15m -> [1h], 1h -> [] (no parents).
    This mirrors Polymarket's round structure where 15m contains 3x5m children
    and 1h contains 4x15m children.

    Args:
        tf: Timeframe slug ('5m', '15m', '1h').

    Returns:
        List of parent timeframe slugs, ordered from nearest to farthest.
        Empty list for 1h (top of hierarchy).
    """
    if tf == "5m":
        return ["15m", "1h"]
    elif tf == "15m":
        return ["1h"]
    else:
        return []

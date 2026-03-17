"""Spread-based Liquidity Filter — pre-entry gate based on order book depth.

Assesses whether a market is liquid enough to trade profitably:
- Tight spread + high depth = liquid = safe to trade
- Wide spread = illiquid = skip
"""

from typing import Optional

from strategy_service.models import PredictionRequest, StrategyResult
from strategy_service.strategies.base import Strategy
from strategy_service.strategies.registry import register_strategy

# Minimum liquidity score for a market to be considered tradeable.
# Score = (1 / normalized_spread) * depth_factor. A score of 50 corresponds
# to roughly a 2% spread with moderate depth -- below this, spread costs
# eat too much of the expected edge.
_MIN_LIQUIDITY = 50.0

# Standard trade size in shares for cost estimation (not currently used in
# the main logic, retained for future slippage modeling).
_STANDARD_SIZE = 100.0

# Expected edge threshold: if the half-spread execution cost exceeds this
# fraction, the market is too expensive to trade profitably.
# 3.5% = 2% Polymarket fee + 1% half-spread + 0.5% slippage buffer.
_EDGE_THRESHOLD = 0.035


def _assess_liquidity(order_book: dict) -> Optional[dict]:
    """Assess market liquidity from order book spread and depth.

    Computes a liquidity score based on the inverse of normalized spread,
    scaled by the depth at the top of book. Tighter spread + more depth =
    higher score = more liquid market.

    Args:
        order_book: Dict with 'bids_up'/'asks_up' (or 'bids'/'asks') arrays.
            Each level is either [price, size] or {'price': ..., 'size': ...}.

    Returns:
        Dict with liquidity_score, spread, normalized_spread, mid, depth,
        and cost_per_share. None if the book is empty or crossed.
    """
    bids = order_book.get("bids_up", []) or order_book.get("bids", [])
    asks = order_book.get("asks_up", []) or order_book.get("asks", [])

    if not bids or not asks:
        return None

    def _parse_level(level) -> tuple[float, float]:
        if isinstance(level, (list, tuple)):
            return float(level[0]), float(level[1])
        if isinstance(level, dict):
            return float(level.get("price", 0)), float(level.get("size", 0))
        return 0.0, 0.0

    best_bid_price, best_bid_size = _parse_level(bids[0])
    best_ask_price, best_ask_size = _parse_level(asks[0])

    if best_bid_price <= 0 or best_ask_price <= 0:
        return None
    if best_ask_price <= best_bid_price:
        return None

    spread = best_ask_price - best_bid_price
    if spread <= 0:
        return None

    mid = (best_bid_price + best_ask_price) / 2.0
    normalized_spread = spread / max(mid, 0.01)
    if normalized_spread <= 0:
        return None

    # Liquidity score: inverse of normalized spread (tighter = higher)
    liquidity_score = 1.0 / normalized_spread

    # Depth factor: more size at top of book = more effective liquidity
    depth_factor = min((best_bid_size + best_ask_size) / 200.0, 3.0)
    liquidity_score *= max(depth_factor, 0.5)

    # Estimated cost to cross the spread for a standard trade
    cost_per_share = normalized_spread / 2.0  # half-spread as execution cost

    return {
        "liquidity_score": liquidity_score,
        "spread": spread,
        "normalized_spread": normalized_spread,
        "mid": mid,
        "depth": best_bid_size + best_ask_size,
        "cost_per_share": cost_per_share,
    }


@register_strategy
class LmsrLiquidityFilterStrategy(Strategy):
    """Spread-based liquidity gate. Blocks trades in illiquid markets."""

    def name(self) -> str:
        return "lmsr_liquidity_filter"

    def required_data(self) -> list[str]:
        return ["order_book"]

    def weight(self) -> float:
        return 1.5

    def min_data_points(self) -> int:
        return 1

    def predict(self, ctx: PredictionRequest) -> Optional[StrategyResult]:
        if ctx.order_book is None:
            return None

        assessment = _assess_liquidity(ctx.order_book)
        if assessment is None:
            return None

        liquidity_score = assessment["liquidity_score"]
        cost_per_share = assessment["cost_per_share"]

        # Gate 1: minimum liquidity
        if liquidity_score < _MIN_LIQUIDITY:
            return StrategyResult(
                p_up=0.5,
                confidence=0.0,
                meta={
                    "liquidity_score": liquidity_score,
                    "gate": "illiquid",
                    "reason": f"score={liquidity_score:.1f} < {_MIN_LIQUIDITY}",
                },
            )

        # Gate 2: spread cost check
        if cost_per_share > _EDGE_THRESHOLD:
            return StrategyResult(
                p_up=0.5,
                confidence=0.0,
                meta={
                    "liquidity_score": liquidity_score,
                    "gate": "cost_too_high",
                    "cost_per_share": cost_per_share,
                    "threshold": _EDGE_THRESHOLD,
                },
            )

        # Market is liquid enough — pass through with confidence based on depth
        confidence_score = min(liquidity_score / 500.0, 1.0)  # saturates at 500
        confidence = 0.1 + 0.15 * confidence_score  # range [0.1, 0.25]

        # Cross-timeframe consistency: if spread-implied direction matches
        # the timeframe consensus, boost confidence; if it contradicts, reduce
        p_up = ctx.round.price_up
        tf_agreement = None
        if ctx.timeframe_intel:
            agreement = ctx.timeframe_intel.agreement_score
            says_up = p_up > 0.5 if p_up is not None else None
            tf_says_up = agreement > 0

            if says_up is not None:
                if says_up == tf_says_up:
                    confidence *= 1.25
                    tf_agreement = "agree"
                else:
                    confidence *= 0.7
                    tf_agreement = "disagree"

        return StrategyResult(
            p_up=0.5,  # neutral — this is a gate, not a directional signal
            confidence=confidence,
            meta={
                "liquidity_score": liquidity_score,
                "gate": "pass",
                "cost_per_share": cost_per_share,
                "spread": assessment["normalized_spread"],
                "depth": assessment["depth"],
                "tf_agreement": tf_agreement,
            },
        )

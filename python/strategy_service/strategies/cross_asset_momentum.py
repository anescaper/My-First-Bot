"""BTC beta-adjusted cross-asset momentum strategy.

Uses timeframe_intel.agreement_score as a proxy for cross-asset momentum
direction, and market_bias profiles to compare asset behavior with BTC leader.
"""

from typing import Optional

import numpy as np

from strategy_service.models import PredictionRequest, StrategyResult
from strategy_service.strategies.base import Strategy
from strategy_service.strategies.registry import register_strategy

# Approximate beta (sensitivity to BTC) per asset.
# Derived from 30-day rolling correlation analysis of Binance spot returns.
# BTC is the market leader (beta=1.0 by definition).
# SOL (1.3) amplifies BTC moves; ETH (0.9) slightly lags.
# These are approximate and stable enough to hardcode (recalibrate quarterly).
BETA = {"ETH": 0.9, "SOL": 1.3, "XRP": 1.1, "BTC": 1.0}


@register_strategy
class CrossAssetMomentumStrategy(Strategy):
    """BTC-leader momentum and mean-reversion strategy for non-BTC assets.

    When BTC (proxied by agreement_score) is strongly directional:
    - If the target asset follows BTC -> momentum continuation signal.
    - If the target asset diverges from BTC -> mean-reversion (catch-up) signal.

    Only works for non-BTC assets (BTC cannot use itself as a reference).
    """
    def name(self) -> str:
        return "cross_asset"

    def required_data(self) -> list[str]:
        return ["micro_candles", "all_prices"]

    def weight(self) -> float:
        return 1.0

    def min_data_points(self) -> int:
        return 5

    def predict(self, ctx: PredictionRequest) -> Optional[StrategyResult]:
        asset = ctx.round.asset.upper()

        # BTC can't use itself as reference
        if asset == "BTC":
            return None

        candles = ctx.micro_candles
        if len(candles) < self.min_data_points():
            return None

        # Require timeframe intel for cross-asset signal
        if not ctx.timeframe_intel:
            return None

        tf_intel = ctx.timeframe_intel
        beta = BETA.get(asset, 1.0)

        # Asset return from candles
        closes = np.array([c.c for c in candles])
        asset_return = (closes[-1] - closes[0]) / closes[0]

        # Use agreement_score as BTC-leader directional proxy
        # agreement_score > 0 means most timeframes say Up (BTC leading up)
        # Scale by beta to get expected co-movement
        agreement = tf_intel.agreement_score
        if abs(agreement) < 0.05:
            # No clear cross-asset direction — skip
            return None

        # Market bias profile: how each timeframe views this asset
        bias_values = list(tf_intel.market_bias.values())
        if bias_values:
            avg_bias = float(np.mean(bias_values))
            bias_spread = float(np.std(bias_values))
        else:
            avg_bias = 0.5
            bias_spread = 0.0

        # Leader direction (from agreement_score, proxy for BTC)
        leader_up = agreement > 0
        leader_strength = abs(agreement)

        # Asset direction (from candle returns)
        asset_up = asset_return > 0

        # Cross-asset momentum: if BTC leader is strongly directional
        # and this asset follows, it's a momentum signal
        if leader_up == asset_up:
            # Asset follows leader — momentum continuation
            if leader_up:
                p_up = 0.5 + min(leader_strength * beta * 0.08, 0.12)
            else:
                p_up = 0.5 - min(leader_strength * beta * 0.08, 0.12)
            signal_type = "momentum"
        else:
            # Asset diverges from leader — mean reversion toward leader
            if leader_up:
                # Leader says up but asset went down — expect catch-up
                p_up = 0.5 + min(leader_strength * 0.06, 0.08)
            else:
                # Leader says down but asset went up — expect pullback
                p_up = 0.5 - min(leader_strength * 0.06, 0.08)
            signal_type = "mean_revert"

        # Confidence based on leader strength and timeframe consistency
        confidence = min(leader_strength * 0.4, 0.3)
        # Reduce confidence if bias spread is high (timeframes disagree)
        if bias_spread > 0.1:
            confidence *= max(0.5, 1.0 - bias_spread)

        if confidence < 0.05:
            return None

        return StrategyResult(
            p_up=p_up,
            confidence=confidence,
            meta={
                "beta": beta,
                "asset_return": float(asset_return),
                "agreement_score": float(agreement),
                "avg_bias": avg_bias,
                "bias_spread": bias_spread,
                "leader_strength": float(leader_strength),
                "signal_type": signal_type,
            },
        )

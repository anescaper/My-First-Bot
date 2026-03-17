"""Futures data strategy — funding rate, taker ratio, OI change, liquidation clustering."""

from typing import Optional

from strategy_service.models import PredictionRequest, StrategyResult
from strategy_service.strategies.base import Strategy
from strategy_service.strategies.registry import register_strategy

# Funding rate extreme threshold (0.03%).
# Binance BTC funding typically ranges from -0.01% to +0.03%. Values above 0.03%
# indicate crowded long positions and historically precede squeezes (contrarian signal).
# Source: empirical analysis of Binance perpetual funding vs 5-min price changes.
FUNDING_EXTREME = 0.0003

# OI change threshold (2% in 5 minutes).
# A 2% change in open interest within 5 minutes indicates significant new position
# entry or exit, which confirms directional conviction when combined with taker ratio.
OI_CHANGE_THRESHOLD = 0.02

# Minimum liquidation notional for squeeze detection ($50k).
# Below $50k, liquidations are noise (small retail accounts). Above $50k with
# 2:1 directional skew, it indicates a genuine squeeze cascade.
LIQUIDATION_NOTIONAL_THRESHOLD = 50_000


@register_strategy
class FuturesPositioningStrategy(Strategy):
    """Futures-based positioning analysis using funding, OI, taker ratio, and liquidations.

    Combines four independent signals from Binance perpetual futures:
    1. Funding rate extreme: contrarian (crowded side gets squeezed).
    2. Taker buy/sell ratio: momentum (aggressive buyers/sellers predict direction).
    3. OI change: confirms directional conviction when combined with taker ratio.
    4. Liquidation clustering: squeeze detection (cascading liquidations accelerate moves).

    The combined signal is averaged across active signals and mapped to p_up with
    a conservative range (0.40-0.60) because futures signals are noisy on 5-min horizons.
    """
    def name(self) -> str:
        return "futures_positioning"

    def required_data(self) -> list[str]:
        return ["futures"]

    def weight(self) -> float:
        return 2.0

    def min_data_points(self) -> int:
        return 1

    def predict(self, ctx: PredictionRequest) -> Optional[StrategyResult]:
        if ctx.futures is None:
            return None

        signals = []
        meta = {}

        # 1. Funding rate extreme → contrarian (overleveraged crowd gets squeezed)
        fr = ctx.futures.funding_rate
        meta["funding_rate"] = fr
        if fr > FUNDING_EXTREME:
            signals.append(("funding", -1.0))  # too many longs → expect down
        elif fr < -FUNDING_EXTREME:
            signals.append(("funding", 1.0))   # too many shorts → expect up

        # 2. Taker buy/sell ratio → momentum
        ratio = ctx.futures.taker_buy_sell_ratio
        meta["taker_ratio"] = ratio
        if ratio > 1.3:
            signals.append(("taker", 1.0))   # aggressive buyers
        elif ratio < 0.7:
            signals.append(("taker", -1.0))  # aggressive sellers

        # 3. OI change → confirms directional conviction
        oi_chg = ctx.futures.oi_change_5m
        meta["oi_change_5m"] = oi_chg
        if abs(oi_chg) > OI_CHANGE_THRESHOLD:
            # Rising OI = new positions entering (confirms current direction)
            # Use taker ratio to determine direction of new positions
            if oi_chg > 0 and ratio > 1.0:
                signals.append(("oi_inflow", 0.5))
            elif oi_chg > 0 and ratio < 1.0:
                signals.append(("oi_inflow", -0.5))
            # Falling OI = positions closing (mean reversion)
            elif oi_chg < -OI_CHANGE_THRESHOLD:
                if ratio > 1.0:
                    signals.append(("oi_outflow", -0.3))  # longs closing
                else:
                    signals.append(("oi_outflow", 0.3))   # shorts closing

        # 4. Liquidation clustering → squeeze signal
        liqs = ctx.futures.liquidations
        if liqs:
            buy_notional = sum(l.notional for l in liqs if l.side == "BUY")
            sell_notional = sum(l.notional for l in liqs if l.side == "SELL")
            meta["liq_buy_notional"] = buy_notional
            meta["liq_sell_notional"] = sell_notional
            meta["liq_count"] = len(liqs)

            # Large sell liquidations (shorts getting squeezed) → bullish
            if sell_notional > LIQUIDATION_NOTIONAL_THRESHOLD and sell_notional > buy_notional * 2:
                signals.append(("liq_squeeze", 0.8))
            # Large buy liquidations (longs getting squeezed) → bearish
            elif buy_notional > LIQUIDATION_NOTIONAL_THRESHOLD and buy_notional > sell_notional * 2:
                signals.append(("liq_squeeze", -0.8))

        if not signals:
            return None

        combined = sum(s for _, s in signals) / len(signals)
        p_up = 0.5 + combined * 0.05
        p_up = max(0.40, min(0.60, p_up))

        confidence = min(abs(combined) * 0.25, 0.30)

        meta["combined_signal"] = combined
        meta["n_signals"] = len(signals)
        meta["signal_names"] = [name for name, _ in signals]

        return StrategyResult(
            p_up=p_up,
            confidence=confidence,
            hold_to_resolution=True,
            max_entry_price=0.65,
            meta=meta,
        )

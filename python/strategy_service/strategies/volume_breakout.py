"""Volume spike confirmed displacement z-score strategy.

This strategy requires a volume spike (>1.5x the 20-period SMA) as a prerequisite
for trading. Volume spikes confirm that the current price displacement is backed
by real trading activity rather than a low-liquidity drift.

The volume boost scales with spike magnitude:
- 1.5x SMA -> 15% confidence boost
- 2.0x SMA -> 30% confidence boost
- 2.5x+ SMA -> 45% confidence boost (capped)

Hardcoded values:
- volume_ratio >= 1.5: minimum spike to trade. 1.5x was chosen because lower
  thresholds (1.2x) produced too many false positives from normal volume variation.
- 20-period SMA: standard lookback for volume baseline. Matches the lookback used
  for realized vol estimation in the z-score computation.
"""

from typing import Optional

import numpy as np
from scipy.stats import norm

from strategy_service.models import PredictionRequest, StrategyResult
from strategy_service.strategies.base import Strategy
from strategy_service.strategies.registry import register_strategy


@register_strategy
class VolumeBreakoutStrategy(Strategy):
    def name(self) -> str:
        return "volume_breakout"

    def required_data(self) -> list[str]:
        return ["micro_candles"]

    def weight(self) -> float:
        return 1.5

    def min_data_points(self) -> int:
        return 25

    def predict(self, ctx: PredictionRequest) -> Optional[StrategyResult]:
        if ctx.round.progress_pct < 0.5:
            return None

        candles = ctx.micro_candles
        if len(candles) < self.min_data_points():
            return None

        volumes = np.array([c.v for c in candles])
        closes = np.array([c.c for c in candles])

        log_returns = np.diff(np.log(closes))
        realized_vol = float(np.std(log_returns[-20:]))
        if realized_vol < 1e-8:
            return None

        # 20-period volume SMA
        vol_sma = volumes[-20:].mean()
        latest_vol = volumes[-1]

        if vol_sma < 1e-12:
            return None

        volume_ratio = latest_vol / vol_sma

        # Require volume spike > 1.5x SMA as confirmation
        if volume_ratio < 1.5:
            return None

        # Displacement z-score (core directional signal)
        log_disp = np.log(ctx.current_price / ctx.reference_price)
        if ctx.round.seconds_remaining <= 0:
            return None

        # realized_vol is per-candle. Scale to remaining candles.
        if len(candles) >= 2:
            ci = (candles[-1].t - candles[0].t) / (len(candles) - 1)
        else:
            ci = 60.0
        n_rem = ctx.round.seconds_remaining / max(ci, 1.0)
        z_raw = log_disp / max(realized_vol * np.sqrt(max(n_rem, 1.0)), 1e-6)
        z = np.clip(z_raw, -3.0, 3.0)

        # Probability via normal CDF
        p_up = float(norm.cdf(z))

        # Bell-shaped confidence: peaks at |z|~2, decays at extremes
        abs_z = abs(z_raw)
        base_conf = min(abs_z * 0.12, 0.30) if abs_z <= 2.0 else 0.30 * np.exp(-0.5 * (abs_z - 2.0))
        # Volume spike confirms conviction — scale boost by how big the spike is
        vol_boost = min((volume_ratio - 1.0) * 0.3, 0.5)  # 1.5x -> 0.15, 2.0x -> 0.30
        base_conf *= (1.0 + vol_boost)
        base_conf = min(base_conf, 0.35)

        if base_conf < 0.05:
            return None

        return StrategyResult(
            p_up=p_up,
            confidence=base_conf,
            meta={
                "z": round(z, 4),
                "log_disp": round(log_disp, 6),
                "volume_ratio": round(float(volume_ratio), 3),
                "direction": "bullish" if z > 0 else "bearish",
            },
        )

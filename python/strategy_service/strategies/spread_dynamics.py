"""Bilateral Spread Dynamics -- displacement z-score with volume/ATR conviction modifiers.

This strategy augments the standard displacement z-score with two conviction modifiers:
1. Volume ratio (recent vs baseline): volume spikes (>1.3x) boost confidence by 20%
   because they indicate institutional participation confirming the directional move.
2. ATR (Average True Range): computed as an informational metric in the output meta.
   High ATR relative to price indicates volatile conditions (wider spreads needed).

Hardcoded values:
- volume_ratio > 1.3 threshold for boost: empirically, volume spikes above 1.3x
  the 20-candle SMA correlated with directional continuation in 5-min crypto rounds.
- Bell-shaped confidence with max 0.35: same pattern as other z-score strategies.
"""

from typing import Optional

import numpy as np
from scipy.stats import norm

from strategy_service.models import PredictionRequest, StrategyResult
from strategy_service.strategies.base import Strategy
from strategy_service.strategies.registry import register_strategy


@register_strategy
class SpreadDynamicsStrategy(Strategy):
    def name(self) -> str:
        return "spread_dynamics"

    def required_data(self) -> list[str]:
        return ["micro_candles", "token_trajectory"]

    def weight(self) -> float:
        return 3.0

    def min_data_points(self) -> int:
        return 20

    def predict(self, ctx: PredictionRequest) -> Optional[StrategyResult]:
        if ctx.round.progress_pct < 0.5:
            return None

        candles = ctx.micro_candles
        if len(candles) < self.min_data_points():
            return None

        closes = np.array([c.c for c in candles])
        highs = np.array([c.h for c in candles])
        lows = np.array([c.l for c in candles])
        volumes = np.array([c.v for c in candles])

        log_returns = np.diff(np.log(closes))
        if len(log_returns) < 10:
            return None

        # Realized volatility from recent candles
        realized_vol = float(np.std(log_returns[-20:]))
        if realized_vol < 1e-8:
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

        # Volume ratio as conviction modifier
        vol_recent = float(np.mean(volumes[-5:])) if len(volumes) >= 5 else 0
        vol_baseline = float(np.mean(volumes[-20:])) if len(volumes) >= 20 else vol_recent
        vol_ratio = vol_recent / max(vol_baseline, 1e-10)

        # ATR as spread proxy (informational)
        tr = np.maximum(highs[1:] - lows[1:], np.abs(highs[1:] - closes[:-1]))
        atr = float(np.mean(tr[-10:]))
        atr_pct = atr / closes[-1] if closes[-1] > 0 else 0

        # Bell-shaped confidence: peaks at |z|~2, decays at extremes
        abs_z = abs(z_raw)
        base_conf = min(abs_z * 0.15, 0.30) if abs_z <= 2.0 else 0.30 * np.exp(-0.5 * (abs_z - 2.0))
        if vol_ratio > 1.3:
            base_conf *= 1.2
        base_conf = min(base_conf, 0.35)

        if base_conf < 0.05:
            return None

        return StrategyResult(
            p_up=p_up,
            confidence=base_conf,
            hold_to_resolution=True,
            max_entry_price=0.65,
            meta={
                "z": round(z, 4),
                "log_disp": round(log_disp, 6),
                "realized_vol": round(realized_vol, 6),
                "n_remaining": round(float(n_rem), 2),
                "atr_pct": round(atr_pct, 6),
                "vol_ratio": round(vol_ratio, 3),
            },
        )

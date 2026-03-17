"""EMA-confirmed displacement z-score trend strategy.

Computes a displacement z-score (log(current/reference) / expected_vol) and
confirms it with an exponential moving average (EMA) of log-returns. When the
EMA direction agrees with the displacement direction, confidence is boosted 1.3x;
when they disagree, it is reduced to 0.7x.

Also uses candle body consistency (ratio of real body to full range) as a
conviction modifier -- consistent candle bodies suggest directional conviction,
while doji-like candles (small body, large range) suggest indecision.

Hardcoded values:
- TIMEFRAME_K: per-timeframe confidence scaling. Shorter timeframes (5m: 1.0) get
  higher confidence because displacement z-scores are more reliable at shorter
  horizons. Longer timeframes (1h: 0.6, 1d: 0.4) are dampened because price has
  more time to mean-revert.
- EMA span=10: chosen as a balance between responsiveness and smoothness for
  5-second candle intervals (~50 second lookback).
- Bell-shaped confidence: peaks at |z|~2 (0.30 max), decays exponentially beyond.
  This prevents overconfidence on extreme z-scores which often mean-revert.
"""

from typing import Optional

import numpy as np
from scipy.stats import norm

from strategy_service.models import PredictionRequest, StrategyResult
from strategy_service.strategies.base import Strategy
from strategy_service.strategies.registry import register_strategy

# Per-timeframe confidence scaling factor.
# Shorter timeframes are more predictable (less time for mean-reversion).
# 1d is included for completeness but no 1d rounds exist on Polymarket.
TIMEFRAME_K = {"5m": 1.0, "15m": 0.8, "1h": 0.6, "1d": 0.4}


@register_strategy
class CandleTrendStrategy(Strategy):
    """EMA-confirmed displacement z-score with body consistency modifier."""
    def name(self) -> str:
        return "candle_trend"

    def required_data(self) -> list[str]:
        return ["micro_candles"]

    def weight(self) -> float:
        return 2.5

    def min_data_points(self) -> int:
        return 15

    def predict(self, ctx: PredictionRequest) -> Optional[StrategyResult]:
        if ctx.round.progress_pct < 0.5:
            return None

        candles = ctx.micro_candles
        if len(candles) < self.min_data_points():
            return None

        closes = np.array([c.c for c in candles])
        log_returns = np.diff(np.log(closes))

        realized_vol = float(np.std(log_returns[-20:]))
        if realized_vol < 1e-8:
            return None

        # Displacement z-score (core directional signal)
        log_disp = np.log(ctx.current_price / ctx.reference_price)
        if ctx.round.seconds_remaining <= 0:
            return None

        # realized_vol is per-candle. Scale to remaining candles, not fraction.
        if len(candles) >= 2:
            ci = (candles[-1].t - candles[0].t) / (len(candles) - 1)
        else:
            ci = 60.0
        n_rem = ctx.round.seconds_remaining / max(ci, 1.0)
        z_raw = log_disp / max(realized_vol * np.sqrt(max(n_rem, 1.0)), 1e-6)
        z = np.clip(z_raw, -3.0, 3.0)

        # Probability via normal CDF
        p_up = float(norm.cdf(z))

        # EMA of log-returns as confirmation signal
        span = 10
        alpha = 2.0 / (span + 1)
        weights = (1 - alpha) ** np.arange(len(log_returns))[::-1]
        weights /= weights.sum()
        ema_return = float(np.dot(log_returns, weights))

        # Body consistency: ratio of real body to full range
        recent = candles[-10:]
        bodies = np.array([abs(c.c - c.o) for c in recent])
        ranges = np.array([c.h - c.l for c in recent])
        valid = ranges > 1e-12
        if valid.sum() < 3:
            return None
        consistency = float(np.mean(bodies[valid] / ranges[valid]))

        # EMA agrees with displacement direction = boost, contradicts = reduce
        ema_agrees = (ema_return > 0 and z > 0) or (ema_return < 0 and z < 0)
        agreement_mult = 1.3 if ema_agrees else 0.7

        # Bell-shaped confidence: peaks at |z|~2, decays at extremes
        k = TIMEFRAME_K.get(ctx.round.timeframe, 0.7)
        abs_z = abs(z_raw)
        base_conf = min(abs_z * 0.12, 0.30) if abs_z <= 2.0 else 0.30 * np.exp(-0.5 * (abs_z - 2.0))
        base_conf *= consistency * agreement_mult * k
        base_conf = min(base_conf, 0.35)

        if base_conf < 0.05:
            return None

        return StrategyResult(
            p_up=p_up,
            confidence=base_conf,
            meta={
                "z": round(z, 4),
                "ema_return": float(ema_return),
                "consistency": round(consistency, 3),
                "ema_agrees": bool(ema_agrees),
                "k": k,
            },
        )

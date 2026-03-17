"""Momentum GARCH — GARCH(1,1) vol + Rust-style momentum confidence + progress² weighting.

Combines:
  - GARCH(1,1) conditional volatility estimate (superior to realized vol)
  - Student-t CDF for p_up with z clamped to [-3, 3] (no saturation)
  - Monotonic confidence: stronger displacement = more conviction (momentum-following)
  - progress² time weighting: stronger signal late in round (from Rust baseline)
"""

import warnings
from typing import Optional

import numpy as np

from strategy_service.models import PredictionRequest, StrategyResult
from strategy_service.strategies.base import Strategy
from strategy_service.strategies.registry import register_strategy


@register_strategy
class MomentumGarchStrategy(Strategy):
    """GARCH(1,1) with momentum-style confidence curve (progress^2 weighting).

    Differs from vanilla garch_t in the confidence formula:
    - garch_t: bell-shaped confidence (peaks at |z|~2, decays at extremes)
    - momentum_garch: monotonic confidence = progress^2 * min(|z|/2, 1.0)

    The progress^2 weighting means this strategy is silent early in a round
    and increasingly confident as the round approaches expiry -- matching the
    Rust pipeline's time_decay_lock behavior where later signals are more reliable
    because the price has had more time to establish a direction.
    """

    def name(self) -> str:
        return "momentum_garch"

    def required_data(self) -> list[str]:
        return ["micro_candles"]

    def weight(self) -> float:
        return 4.0

    def min_data_points(self) -> int:
        return 30

    def predict(self, ctx: PredictionRequest) -> Optional[StrategyResult]:
        from arch import arch_model
        from scipy.stats import t as student_t

        closes = np.array([c.c for c in ctx.micro_candles])
        if len(closes) < self.min_data_points():
            return None

        # Only fire after 50% round progress
        progress = ctx.round.progress_pct
        if progress < 0.5:
            return None

        returns = np.diff(np.log(closes)) * 100  # percentage log-returns

        try:
            with warnings.catch_warnings():
                warnings.simplefilter("ignore")
                model = arch_model(returns, vol="GARCH", p=1, q=1, dist="StudentsT")
                res = model.fit(disp="off", show_warning=False)
        except Exception:
            return None

        # Forecast 1-step vol
        forecast = res.forecast(horizon=1)
        sigma = np.sqrt(forecast.variance.iloc[-1, 0]) / 100  # back to decimal

        # Z-score with CORRECT scaling: sigma per-candle → scale to remaining candles
        log_disp = np.log(ctx.current_price / ctx.reference_price)
        if ctx.round.seconds_remaining <= 0:
            return None

        candles = ctx.micro_candles
        if len(candles) >= 2:
            candle_interval_s = (candles[-1].t - candles[0].t) / (len(candles) - 1)
        else:
            candle_interval_s = 60.0
        candle_interval_s = max(candle_interval_s, 1.0)
        n_remaining = ctx.round.seconds_remaining / candle_interval_s
        expected_move = sigma * np.sqrt(max(n_remaining, 1.0))

        if expected_move < 1e-10:
            return None

        z_raw = log_disp / expected_move
        z = np.clip(z_raw, -3.0, 3.0)

        # Student-t CDF with estimated df
        df = res.params.get("nu", 5.0)
        df = max(2.1, min(df, 30.0))
        p_up = float(student_t.cdf(z, df))

        # Momentum-friendly confidence (Rust baseline style):
        # progress² × min(|z|/2, 1.0) — monotonically increasing with displacement
        confidence = progress ** 2 * min(abs(z) / 2.0, 1.0)
        confidence = min(confidence, 0.50)

        if confidence < 0.05:
            return None

        return StrategyResult(
            p_up=p_up,
            confidence=confidence,
            meta={
                "garch_vol": float(sigma),
                "df": float(df),
                "z": float(z),
                "z_raw": round(float(z_raw), 4),
                "progress": round(progress, 3),
                "n_remaining": round(float(n_remaining), 1),
            },
        )

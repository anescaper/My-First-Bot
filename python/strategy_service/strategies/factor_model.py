"""Constant-Vol Normal CDF -- rolling volatility estimate + displacement signal.

This is the simplest displacement z-score strategy: uses a rolling window stddev
of log-returns as the vol estimate (instead of GARCH) and the normal CDF
(instead of Student-t) for probability conversion.

Despite the 'factor_model' name (historical), this is a pure displacement z-score
strategy. It serves as a simpler, more robust alternative to GARCH-t for cases
where GARCH fitting may fail or overfit on short candle histories.

Hardcoded values:
- Rolling window: min(50, len(returns)) candles for vol estimation.
- max_entry_price=0.65: prevents buying tokens above 65c.
- Bell-shaped confidence: same as candle_trend (peaks at |z|~2, max 0.35).
"""

from typing import Optional

import numpy as np

from strategy_service.models import PredictionRequest, StrategyResult
from strategy_service.strategies.base import Strategy
from strategy_service.strategies.registry import register_strategy


@register_strategy
class FactorModelStrategy(Strategy):
    def name(self) -> str:
        return "factor_model"

    def required_data(self) -> list[str]:
        return ["micro_candles"]

    def weight(self) -> float:
        return 4.0

    def min_data_points(self) -> int:
        return 30

    def predict(self, ctx: PredictionRequest) -> Optional[StrategyResult]:
        from scipy.stats import norm

        candles = ctx.micro_candles
        if len(candles) < self.min_data_points():
            return None

        # Only fire after 50% round progress
        if ctx.round.progress_pct < 0.5:
            return None

        closes = np.array([c.c for c in candles])
        log_returns = np.diff(np.log(closes))
        if len(log_returns) < 10:
            return None

        # Rolling vol estimate (simpler than GARCH — that's the identity)
        window = min(50, len(log_returns))
        sigma = float(np.std(log_returns[-window:]))
        if sigma < 1e-10:
            return None

        # Displacement signal: log(current / reference) normalized by remaining vol
        log_disp = np.log(ctx.current_price / ctx.reference_price)
        if ctx.round.seconds_remaining <= 0:
            return None

        # sigma is per-candle. Scale to remaining candles.
        if len(candles) >= 2:
            ci = (candles[-1].t - candles[0].t) / (len(candles) - 1)
        else:
            ci = 60.0
        n_rem = ctx.round.seconds_remaining / max(ci, 1.0)
        expected_move = sigma * np.sqrt(max(n_rem, 1.0))
        if expected_move < 1e-10:
            return None

        z_raw = log_disp / expected_move
        z = np.clip(z_raw, -3.0, 3.0)

        # Normal CDF for p_up (constant-vol normal distribution)
        p_up = float(norm.cdf(z))
        abs_z = abs(z_raw)
        confidence = min(abs_z * 0.15, 0.30) if abs_z <= 2.0 else 0.30 * np.exp(-0.5 * (abs_z - 2.0))
        confidence = min(confidence, 0.35)

        if confidence < 0.05:
            return None

        return StrategyResult(
            p_up=p_up,
            confidence=confidence,
            hold_to_resolution=True,
            max_entry_price=0.65,
            meta={
                "sigma": round(sigma, 8),
                "z": round(float(z), 4),
                "log_disp": round(float(log_disp), 8),
                "n_remaining": round(float(n_rem), 4),
                "expected_move": round(float(expected_move), 8),
            },
        )

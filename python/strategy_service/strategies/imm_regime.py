"""Interacting Multiple Model (IMM) regime detection via 3 Kalman filter bandwidths.

Runs three parallel Kalman filters with different process noise levels to detect
the current market regime (trending, ranging, or volatile). The filter whose
innovations best explain the observed price changes gets the highest probability.

In trending regime: displacement z-score follows momentum (standard signal).
In ranging regime: z-score is INVERTED (mean-reversion: displaced up -> expect down).
The volatile branch informs vol estimation but does not change signal direction.

Hardcoded values:
- Q_pos levels: 0.001 (trending/slow), 0.1 (ranging/fast), 0.05 (volatile/medium).
  These were tuned on 7 days of BTC 1-minute data to produce meaningful regime
  probabilities that switch on ~5-15 candle timescales.
- R=0.001: measurement noise, kept small because candle closes are precise.
- Cache TTL=600s (10 min): filter state is cached per (condition_id, profile) to
  maintain continuity across prediction calls within the same round.
"""

import time
from typing import Optional

import numpy as np
from scipy.stats import norm

from strategy_service.models import PredictionRequest, StrategyResult
from strategy_service.strategies.base import Strategy
from strategy_service.strategies.registry import register_strategy

# Module-level cache: (condition_id, profile) -> (timestamp, filter_states)
_imm_cache: dict[tuple[str, str], tuple[float, dict]] = {}
_CACHE_TTL = 600  # 10 minutes


def _cleanup_cache():
    now = time.time()
    expired = [k for k, (ts, _) in _imm_cache.items() if now - ts > _CACHE_TTL]
    for k in expired:
        del _imm_cache[k]


@register_strategy
class ImmRegimeStrategy(Strategy):
    def name(self) -> str:
        return "imm_regime"

    def required_data(self) -> list[str]:
        return ["micro_candles"]

    def weight(self) -> float:
        return 3.0

    def min_data_points(self) -> int:
        return 20

    def predict(self, ctx: PredictionRequest) -> Optional[StrategyResult]:
        candles = ctx.micro_candles
        if len(candles) < self.min_data_points():
            return None

        # Timing gate: only fire after 50% round progress
        if ctx.round.progress_pct < 0.5:
            return None

        closes = np.array([c.c for c in candles])
        log_returns = np.diff(np.log(closes))
        R = 0.001  # measurement noise

        # 3 bandwidth levels: Q_pos controls how fast the Kalman tracks price
        # Low Q_pos = slow/smooth (trending), High Q_pos = fast/noisy (ranging)
        branches = [
            {"name": "trending", "Q_pos": 0.001},
            {"name": "ranging",  "Q_pos": 0.1},
            {"name": "volatile", "Q_pos": 0.05},
        ]

        cache_key = (ctx.round.condition_id, ctx.strategy_profile)
        _cleanup_cache()

        # Restore or initialize filter states
        if cache_key in _imm_cache:
            _, states = _imm_cache[cache_key]
            mus = np.array(states["mus"])
            x_hats = np.array(states["x_hats"])
            P_vals = np.array(states["P_vals"])
            processed = states["processed"]
        else:
            mus = np.ones(3) / 3.0
            x_hats = np.zeros(3)
            P_vals = np.ones(3) * 0.01
            processed = 0

        # Process new observations
        if processed >= len(log_returns):
            return None  # no new data since last call
        new_obs = log_returns[processed:]
        if len(new_obs) == 0:
            return None

        for y in new_obs:
            likelihoods = np.zeros(3)
            for i, br in enumerate(branches):
                # Predict
                x_pred = x_hats[i]
                P_pred = P_vals[i] + br["Q_pos"]
                # Innovation
                innov = y - x_pred
                S = P_pred + R
                # Likelihood: N(y | x_pred, S)
                likelihoods[i] = np.exp(-0.5 * innov**2 / S) / np.sqrt(2 * np.pi * S)
                # Update
                K = P_pred / S
                x_hats[i] = x_pred + K * innov
                P_vals[i] = (1 - K) * P_pred

            # Mix probabilities
            weighted = mus * likelihoods
            total = weighted.sum()
            if total > 0:
                mus = weighted / total
            else:
                mus = np.ones(3) / 3.0

        # Save state
        _imm_cache[cache_key] = (time.time(), {
            "mus": mus.tolist(),
            "x_hats": x_hats.tolist(),
            "P_vals": P_vals.tolist(),
            "processed": len(log_returns),
        })

        p_trend, p_range, p_vol = mus

        # Use the two directional regimes: trending vs ranging
        # Volatile branch informs vol estimate but doesn't cause abstention
        dominant_idx = 0 if p_trend >= p_range else 1  # trending or ranging

        # Use rolling realized vol (not Kalman P_vals which are in filter-space,
        # not return-space — P_vals ~0.1 vs actual 1m vol ~0.002)
        realized_vol = float(np.std(log_returns[-20:]))
        if realized_vol < 1e-8:
            return None

        # Displacement signal: log(current / reference)
        log_disp = np.log(ctx.current_price / ctx.reference_price)
        t_remain = ctx.round.seconds_remaining / ctx.round.timeframe_seconds
        if t_remain <= 0:
            return None

        # realized_vol is per-candle. Scale to remaining candles.
        if len(candles) >= 2:
            candle_interval_s = (candles[-1].t - candles[0].t) / (len(candles) - 1)
        else:
            candle_interval_s = 60.0
        candle_interval_s = max(candle_interval_s, 1.0)
        n_remaining = ctx.round.seconds_remaining / candle_interval_s
        expected_move = realized_vol * np.sqrt(max(n_remaining, 1.0))
        if expected_move < 1e-10:
            return None

        z_raw = log_disp / expected_move

        # In ranging regime, mean-revert: flip the z-score
        if p_range > p_trend:
            z_raw = -z_raw

        z = np.clip(z_raw, -3.0, 3.0)

        # Normal CDF for probability (regime doesn't estimate df)
        p_up = float(norm.cdf(z))

        # Confidence: peaks at |z|~2, decays at extremes
        abs_z = abs(z_raw)
        confidence = min(abs_z * 0.15, 0.30) if abs_z <= 2.0 else 0.30 * np.exp(-0.5 * (abs_z - 2.0))
        confidence = min(confidence, 0.35)

        regime_name = branches[dominant_idx]["name"]

        return StrategyResult(
            p_up=p_up,
            confidence=confidence,
            hold_to_resolution=True,
            max_entry_price=0.65,
            meta={
                "p_trending": float(p_trend),
                "p_ranging": float(p_range),
                "p_volatile": float(p_vol),
                "regime": regime_name,
                "z": float(z),
                "log_disp": float(log_disp),
                "expected_move": float(expected_move),
                "realized_vol": float(realized_vol),
            },
        )

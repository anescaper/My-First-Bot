"""Stochastic Volatility Filter -- dual Kalman filter tracking price + log-vol.

Runs two coupled Kalman filters:
1. Price filter: tracks the return process with volatility from the vol filter.
2. Vol filter: tracks log-volatility as a random walk, updated via squared innovations.

The coupling: the price filter's process noise variance = exp(vol filter estimate).
This means the price filter automatically adapts its tracking speed to the current
volatility regime -- fast in high-vol, slow in low-vol.

Hardcoded values:
- Q_vol=0.01: vol random walk process noise. Small value gives high persistence
  (vol estimates change slowly), appropriate for crypto where vol regimes last
  minutes to hours.
- R_meas=0.001: price measurement noise. Small because candle closes are precise.
- R_vol=2.0: log-chi-squared distribution has inherently high variance, so the
  vol measurement noise is set high to prevent overreaction to single innovations.
- Cache TTL=600s: filter state persists across calls within the same round.
- High-vol penalty: if estimated vol > 2x median vol, confidence is halved because
  high-vol regimes are less predictable (wider confidence intervals on z-scores).
"""

import time
from typing import Optional

import numpy as np

from strategy_service.models import PredictionRequest, StrategyResult
from strategy_service.strategies.base import Strategy
from strategy_service.strategies.registry import register_strategy

# Module-level cache: (condition_id, profile) -> (timestamp, filter_state)
_sv_cache: dict[tuple[str, str], tuple[float, dict]] = {}
_CACHE_TTL = 600  # 10 minutes


def _cleanup_cache():
    now = time.time()
    expired = [k for k, (ts, _) in _sv_cache.items() if now - ts > _CACHE_TTL]
    for k in expired:
        del _sv_cache[k]


@register_strategy
class StochVolFilterStrategy(Strategy):
    def name(self) -> str:
        return "stoch_vol_filter"

    def required_data(self) -> list[str]:
        return ["micro_candles"]

    def weight(self) -> float:
        return 3.0

    def min_data_points(self) -> int:
        return 20

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
        if len(log_returns) == 0:
            return None

        cache_key = (ctx.round.condition_id, ctx.strategy_profile)
        _cleanup_cache()

        # Restore or initialize filter state
        if cache_key in _sv_cache:
            _, state = _sv_cache[cache_key]
            x_price = state["x_price"]
            P_price = state["P_price"]
            x_vol = state["x_vol"]
            P_vol = state["P_vol"]
            processed = state["processed"]
            vol_history = state["vol_history"]
        else:
            x_price = float(log_returns[0])
            P_price = 0.01
            x_vol = np.log(np.var(log_returns[:5]) + 1e-10)
            P_vol = 1.0
            processed = 0
            vol_history = []

        # Process noise for vol random walk (high persistence)
        Q_vol = 0.01
        R_meas = 0.001  # measurement noise

        if processed >= len(log_returns):
            return None  # no new data since last call
        new_obs = log_returns[processed:]
        if len(new_obs) == 0:
            return None

        innovations = []
        for y in new_obs:
            # --- Vol filter: predict ---
            x_vol_pred = x_vol  # random walk
            P_vol_pred = P_vol + Q_vol

            # --- Price filter: predict ---
            # Process noise variance is exp(x_vol) — couples the two filters
            Q_price = np.exp(x_vol_pred)
            x_price_pred = x_price
            P_price_pred = P_price + Q_price

            # --- Price filter: update ---
            innov = y - x_price_pred
            S_price = P_price_pred + R_meas
            K_price = P_price_pred / S_price
            x_price = x_price_pred + K_price * innov
            P_price = (1 - K_price) * P_price_pred
            innovations.append(innov)

            # --- Vol filter: update using squared innovation as observation ---
            # Observation of log-vol: log(innov^2 + eps) is a noisy measurement
            vol_obs = np.log(innov**2 + 1e-10)
            R_vol = 2.0  # log-chi-squared noise is high variance
            S_vol = P_vol_pred + R_vol
            K_vol = P_vol_pred / S_vol
            x_vol = x_vol_pred + K_vol * (vol_obs - x_vol_pred)
            P_vol = (1 - K_vol) * P_vol_pred

            vol_history.append(float(np.exp(x_vol)))

        # Save state
        _sv_cache[cache_key] = (time.time(), {
            "x_price": float(x_price),
            "P_price": float(P_price),
            "x_vol": float(x_vol),
            "P_vol": float(P_vol),
            "processed": len(log_returns),
            "vol_history": vol_history[-100:],  # keep last 100
        })

        current_vol = np.exp(x_vol)
        median_vol = float(np.median(vol_history)) if len(vol_history) >= 5 else current_vol

        # Displacement signal: log(current / reference) normalized by remaining vol
        log_disp = np.log(ctx.current_price / ctx.reference_price)
        t_remain = ctx.round.seconds_remaining / ctx.round.timeframe_seconds
        if t_remain <= 0:
            return None

        # current_vol is per-candle variance. Scale to remaining candles.
        if len(candles) >= 2:
            candle_interval_s = (candles[-1].t - candles[0].t) / (len(candles) - 1)
        else:
            candle_interval_s = 60.0
        candle_interval_s = max(candle_interval_s, 1.0)
        n_remaining = ctx.round.seconds_remaining / candle_interval_s
        z_raw = log_disp / max(np.sqrt(current_vol) * np.sqrt(max(n_remaining, 1.0)), 1e-6)
        z = np.clip(z_raw, -3.0, 3.0)

        # Normal CDF for p_up (stochastic vol, normal distribution)
        p_up = float(norm.cdf(z))
        abs_z = abs(z_raw)
        confidence = min(abs_z * 0.15, 0.30) if abs_z <= 2.0 else 0.30 * np.exp(-0.5 * (abs_z - 2.0))
        confidence = min(confidence, 0.35)

        # High-vol penalty: if estimated vol > 2x median, halve confidence
        if len(vol_history) >= 5 and current_vol > 2.0 * median_vol:
            confidence *= 0.5

        # Parent timeframe anchoring: if parent has strong conviction and
        # current vol is low, the quiet period in a trending parent = continuation
        if ctx.timeframe_intel and ctx.round.timeframe in ("5m", "15m"):
            parent_bias = ctx.timeframe_intel.parent_bias
            parent_conviction = abs(parent_bias - 0.5)
            if parent_conviction > 0.15:
                confidence *= (1.0 + parent_conviction * 0.5)
                confidence = min(confidence, 0.40)

        if confidence < 0.05:
            return None

        return StrategyResult(
            p_up=p_up,
            confidence=confidence,
            hold_to_resolution=True,
            max_entry_price=0.65,
            meta={
                "current_vol": float(current_vol),
                "median_vol": float(median_vol),
                "high_vol": bool(len(vol_history) >= 5 and current_vol > 2.0 * median_vol),
                "z": float(z),
                "log_disp": float(log_disp),
                "t_remain": float(t_remain),
            },
        )

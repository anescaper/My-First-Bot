"""Options flow strategy — uses Deribit DVOL as forward volatility estimate.

Logic:
  Uses DVOL (Deribit Volatility Index) instead of realized vol for the
  expected-move calculation. DVOL is market-implied forward vol, which
  prices in upcoming events (FOMC, CPI, etc.) that realized vol misses.

  - High DVOL regime (>50): market expects big moves → dampen displacement signal
    toward 0.5 (mean-reversion bias, since large moves are already priced in).
  - Low DVOL regime (<30): complacency → trust displacement more.

  This strategy requires options data (Deribit DVOL). Without it, abstains.
  Skew/put-call ratio are NOT yet populated by the adapter — only DVOL is live.
"""

from typing import Optional

import numpy as np
from scipy.stats import norm

from strategy_service.models import PredictionRequest, StrategyResult
from strategy_service.strategies.base import Strategy
from strategy_service.strategies.registry import register_strategy


@register_strategy
class OptionsFlowStrategy(Strategy):
    """Uses Deribit DVOL as a forward-looking volatility estimate for z-score computation.

    Hardcoded values:
    - dvol_mean=40.0: typical BTC DVOL center based on 2024-2026 data. DVOL oscillates
      around 35-50 in normal conditions, spikes to 80+ during crashes.
    - dvol_z normalization by 15.0: one 'standard deviation' of DVOL is ~15 points,
      so dvol_z > 1 means elevated fear regime.
    - Dampening: high DVOL pulls p_up toward 0.5 because large expected moves are
      already priced in, making displacement signals less informative.
    - 525,600 = minutes per year (used for annualized vol to per-timeframe conversion).
    """

    def name(self) -> str:
        return "options_flow"

    def required_data(self) -> list[str]:
        return ["micro_candles", "options"]

    def weight(self) -> float:
        return 2.5

    def min_data_points(self) -> int:
        return 30

    def predict(self, ctx: PredictionRequest) -> Optional[StrategyResult]:
        # Require options data with valid DVOL
        if ctx.options is None or ctx.options.dvol <= 0:
            return None

        closes = np.array([c.c for c in ctx.micro_candles])
        if len(closes) < self.min_data_points():
            return None

        # Only fire after 50% round progress (aligned with garch_t)
        if ctx.round.progress_pct < 0.5:
            return None

        # --- Displacement z-score using DVOL as forward vol ---
        log_disp = np.log(ctx.current_price / ctx.reference_price)
        t_remain = ctx.round.seconds_remaining / ctx.round.timeframe_seconds
        if t_remain <= 0:
            return None

        # DVOL = annualized vol in %, e.g. 45 means 45%
        # Per-timeframe vol = DVOL/100 * sqrt(tf_minutes * t_remain / 525600)
        dvol_decimal = ctx.options.dvol / 100.0
        tf_minutes = ctx.round.timeframe_seconds / 60.0
        expected_move = dvol_decimal * np.sqrt(tf_minutes * t_remain / 525600.0)

        if expected_move < 1e-10:
            return None

        z_raw = log_disp / expected_move
        z = np.clip(z_raw, -3.0, 3.0)

        # Base p_up from normal CDF of z-score
        p_up = float(norm.cdf(z))

        # --- DVOL regime modulation ---
        # High DVOL (>50) = fear regime → dampen toward 0.5 (mean-reversion)
        # Low DVOL (<30) = complacency → trust displacement
        dvol = ctx.options.dvol
        dvol_mean = 40.0  # typical BTC DVOL center
        dvol_z = (dvol - dvol_mean) / 15.0

        if dvol_z > 0:
            # High vol: pull p_up toward 0.5
            dampening = min(dvol_z * 0.1, 0.3)
            p_up = p_up + (0.5 - p_up) * dampening

        p_up = float(np.clip(p_up, 0.05, 0.95))

        # Bell-shaped confidence: peaks at |z|~2, decays at extremes
        abs_z = abs(z_raw)
        confidence = min(abs_z * 0.12, 0.30) if abs_z <= 2.0 else 0.30 * np.exp(-0.5 * (abs_z - 2.0))
        confidence = min(confidence, 0.35)

        # Gate: don't return low-confidence results that poison the ensemble
        if confidence < 0.10:
            return None

        return StrategyResult(
            p_up=p_up,
            confidence=confidence,
            meta={
                "dvol": float(dvol),
                "dvol_z": float(dvol_z),
                "z_displacement": float(z_raw),
                "expected_move_dvol": float(expected_move),
            },
        )

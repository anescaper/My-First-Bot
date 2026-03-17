"""Token flow divergence -- directional signal from Polymarket token price trajectory.

Uses the velocity (dp_up/dt) and acceleration (d^2p_up/dt^2) of the CLOB token
price to detect informed flow. When token momentum agrees with the underlying
price direction, it confirms the move; when they diverge, it reduces confidence.

This strategy uses the raw CLOB token price as its base prediction (current p_up)
rather than computing its own fair value. Its primary role is as a confidence
modifier -- the velocity magnitude determines confidence, and agreement with
price direction determines the boost/reduction multiplier.

Hardcoded values:
- min trajectory length: 5 ticks (reduced from earlier iterations).
- velocity normalization: * 500 to convert per-second dp/dt (~0.0001-0.001)
  to a 0-0.3 confidence range.
- Agreement boost: 1.3x when token flow agrees with price direction;
  0.7x when they diverge.
"""

from typing import Optional

import numpy as np

from strategy_service.models import PredictionRequest, StrategyResult
from strategy_service.strategies.base import Strategy
from strategy_service.strategies.registry import register_strategy


@register_strategy
class TokenFlowDivergenceStrategy(Strategy):
    def name(self) -> str:
        return "token_flow_divergence"

    def required_data(self) -> list[str]:
        return ["token_trajectory"]

    def weight(self) -> float:
        return 1.5

    def min_data_points(self) -> int:
        return 5

    def predict(self, ctx: PredictionRequest) -> Optional[StrategyResult]:
        traj = ctx.token_trajectory
        if len(traj) < 5:
            return None

        # Extract p_up and timestamps from recent trajectory
        p_ups = np.array([tk.p_up for tk in traj])
        times = np.array([tk.t for tk in traj], dtype=float)

        # Normalize time to seconds from start
        dt = np.diff(times)
        if np.any(dt <= 0):
            return None

        # Token velocity: dp/dt from last few ticks
        dp = np.diff(p_ups)
        velocity = dp / dt  # dp_up/dt per tick gap

        # Token acceleration: d²p/dt²
        if len(velocity) >= 2:
            dt2 = (dt[:-1] + dt[1:]) / 2
            accel = np.diff(velocity) / dt2
            mean_accel = float(np.mean(accel[-3:]))
        else:
            mean_accel = 0.0

        mean_velocity = float(np.mean(velocity[-5:]))

        # Overall trajectory direction
        fill_direction = 1.0 if mean_velocity > 0 else -1.0

        # Base prediction from trajectory: token market's implied direction
        # Use current token p_up as the base
        current_p_up = float(p_ups[-1])

        # Determine if our momentum agrees with price direction
        price_direction = 1.0 if ctx.current_price > ctx.reference_price else -1.0

        # Base confidence from velocity magnitude
        base_confidence = min(abs(mean_velocity) * 500, 0.3)

        if fill_direction * price_direction > 0:
            # Token flow agrees with price movement → boost
            confidence = min(base_confidence * 1.3, 0.35)
        else:
            # Token flow disagrees → reduce
            confidence = base_confidence * 0.7

        return StrategyResult(
            p_up=current_p_up,
            confidence=confidence,
            meta={
                "velocity": mean_velocity,
                "acceleration": mean_accel,
                "fill_direction": fill_direction,
                "agreement": fill_direction * price_direction > 0,
            },
        )

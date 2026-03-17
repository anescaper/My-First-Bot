"""Kaufman Efficiency Ratio (ER) regime detection with displacement z-score.

The Kaufman ER measures how 'efficient' recent price movement has been:
- ER = |net change| / sum(|individual changes|)
- ER near 1.0 = trending (price moved in one direction)
- ER near 0.0 = ranging (price oscillated with no net change)

The strategy uses ER to switch between two modes:
- ER > 0.6 (trending): standard displacement z-score (momentum-following).
- ER < 0.3 (ranging): INVERTED z-score (mean-reversion: displaced up -> expect down).
- 0.3 <= ER <= 0.6 (indeterminate): abstain (no clear regime -> no trade).

Hardcoded values:
- ER window=10 candles: short window to detect regime changes quickly.
  Longer windows (20+) were too slow to react to regime transitions.
- ER thresholds 0.6/0.3: chosen from empirical testing on BTC 5-min data.
  0.6 reliably identifies trending periods; 0.3 reliably identifies ranging.
  The 0.3-0.6 dead zone avoids trading in ambiguous regimes.
"""

from typing import Optional

import numpy as np
from scipy.stats import norm

from strategy_service.models import PredictionRequest, StrategyResult
from strategy_service.strategies.base import Strategy
from strategy_service.strategies.registry import register_strategy


@register_strategy
class VolRegimeStrategy(Strategy):
    def name(self) -> str:
        return "vol_regime"

    def required_data(self) -> list[str]:
        return ["micro_candles"]

    def weight(self) -> float:
        return 2.0

    def min_data_points(self) -> int:
        return 12

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

        window = 10
        tail = closes[-window - 1:]

        # Kaufman Efficiency Ratio
        net_change = tail[-1] - tail[0]
        individual_changes = np.abs(np.diff(tail))
        sum_changes = individual_changes.sum()

        if sum_changes < 1e-12:
            return None

        er = abs(net_change) / sum_changes

        # Displacement z-score
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

        # Regime classification determines how we use z
        if er > 0.6:
            # Trending: follow displacement direction
            regime = "trending"
            p_up = float(norm.cdf(z))
        elif er < 0.3:
            # Ranging: mean-reversion (invert z — displaced up = expect revert down)
            regime = "ranging"
            p_up = float(norm.cdf(-z))
        else:
            # Indeterminate: abstain
            return None

        # Bell-shaped confidence: peaks at |z|~2, decays at extremes
        er_strength = abs(er - 0.45) / 0.45  # 0 at center, ~1 at extremes
        abs_z = abs(z_raw)
        base_conf = min(abs_z * 0.12, 0.30) if abs_z <= 2.0 else 0.30 * np.exp(-0.5 * (abs_z - 2.0))
        base_conf *= (0.5 + er_strength * 0.5)
        base_conf = min(base_conf, 0.35)

        if base_conf < 0.05:
            return None

        return StrategyResult(
            p_up=p_up,
            confidence=base_conf,
            meta={
                "z": round(z, 4),
                "er": round(float(er), 4),
                "regime": regime,
                "log_disp": round(log_disp, 6),
            },
        )

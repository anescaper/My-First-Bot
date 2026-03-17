"""GARCH(1,1) + Student-t CDF strategy.

Uses the ARCH library to fit a GARCH(1,1) model with Student-t innovations
to micro-candle log-returns. The fitted conditional volatility is used to
normalize the displacement (log(current/reference)) into a z-score, which is
then converted to a probability via the Student-t CDF.

Why GARCH(1,1) instead of realized vol:
- GARCH captures volatility clustering (high vol follows high vol) which is
  empirically strong in crypto intra-round price action.
- The Student-t distribution has heavier tails than normal, better matching
  the fat-tailed return distribution of 5-second crypto candles.

Why Student-t CDF instead of normal CDF:
- Crypto returns have excess kurtosis (fat tails). Using normal CDF would
  overestimate probabilities at moderate z-scores and underestimate at extremes.
- The estimated degrees-of-freedom (df) parameter adapts to the current tail
  heaviness, typically landing between 3-8 for crypto.

Hardcoded values:
- min_data_points=30: GARCH needs at least 30 observations to estimate 4 parameters
  (omega, alpha, beta, nu) without severe overfitting.
- progress_pct < 0.5 gate: only fires in the second half of the round because
  early candles have too little vol history for reliable GARCH estimation.
- confidence = min(|z| * 0.15, 0.4): bell-shaped confidence that caps at 0.4
  because GARCH predictions beyond 2-sigma are unreliable for 5-min horizons.
- max_entry_price=0.65: avoids buying tokens above 65c where edge is compressed.
- df clamped to [2.1, 30.0]: below 2.1 the t-distribution has infinite variance;
  above 30 it converges to normal (no benefit).
"""

import warnings
from typing import Optional

import numpy as np

from strategy_service.models import PredictionRequest, StrategyResult
from strategy_service.strategies.base import Strategy
from strategy_service.strategies.registry import register_strategy


@register_strategy
class GarchTStrategy(Strategy):
    """GARCH(1,1) conditional volatility + Student-t probability estimator."""

    def name(self) -> str:
        return "garch_t"

    def required_data(self) -> list[str]:
        return ["micro_candles"]

    def weight(self) -> float:
        return 3.0

    def min_data_points(self) -> int:
        return 30  # GARCH needs sufficient observations for parameter estimation

    def predict(self, ctx: PredictionRequest) -> Optional[StrategyResult]:
        """Fit GARCH(1,1) to candle returns and compute Student-t CDF probability.

        Args:
            ctx: Prediction context with micro_candles, reference_price, current_price.

        Returns:
            StrategyResult with p_up from Student-t CDF and bell-shaped confidence.
            None if insufficient data, too early in round, or GARCH fitting fails.
        """
        from arch import arch_model
        from scipy.stats import t as student_t

        closes = np.array([c.c for c in ctx.micro_candles])
        if len(closes) < self.min_data_points():
            return None

        # Only fire after 50% round progress (time_decay_lock behavior)
        if ctx.round.progress_pct < 0.5:
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

        # Z-score: log displacement vs expected remaining vol
        log_disp = np.log(ctx.current_price / ctx.reference_price)
        t_remain = ctx.round.seconds_remaining / ctx.round.timeframe_seconds
        if t_remain <= 0:
            return None
        expected_move = sigma * np.sqrt(t_remain)

        if expected_move < 1e-10:
            return None

        z = log_disp / expected_move

        # Student-t CDF with estimated df
        df = res.params.get("nu", 5.0)
        df = max(2.1, min(df, 30.0))  # bound df to reasonable range
        p_up = float(student_t.cdf(z, df))
        confidence = min(abs(z) * 0.15, 0.4)

        return StrategyResult(
            p_up=p_up,
            confidence=confidence,
            meta={"garch_vol": float(sigma), "df": float(df), "z": float(z)},
            hold_to_resolution=True,
            max_entry_price=0.65,
        )

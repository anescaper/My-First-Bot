"""Base strategy class. All strategies inherit from this.

Strategies follow the template method pattern: subclasses implement the abstract
methods (name, required_data, predict) and optionally override weight() and
min_data_points() for ensemble tuning.

A strategy can return None from predict() to abstain -- this is NOT an error.
Strategies should abstain when they lack sufficient data, when round timing
is outside their operating window, or when their signal is too weak to be useful.
"""

from abc import ABC, abstractmethod
from typing import Optional

from strategy_service.models import PredictionRequest, StrategyResult


class Strategy(ABC):
    """Base class for all prediction strategies.

    Each strategy produces an independent probability estimate (p_up) for a
    binary outcome market. Strategies are registered via the @register_strategy
    decorator and looked up by name in PROFILE_STRATEGIES (registry.py).

    Design principles:
    - Strategies are stateless between calls (except for cached Kalman/GARCH filter
      state, stored in module-level dicts with TTL cleanup).
    - Each strategy must be able to abstain (return None) gracefully.
    - Confidence must reflect actual conviction, not just signal strength.
    """

    @abstractmethod
    def name(self) -> str:
        """Unique strategy identifier. Must match the name in PROFILE_STRATEGIES.

        Returns:
            A string like 'garch_t', 'brownian_bridge', etc.
        """
        ...

    @abstractmethod
    def required_data(self) -> list[str]:
        """Data fields this strategy needs from PredictionRequest.

        Used for documentation and strategy listing. Not enforced at runtime.
        Valid field names: 'micro_candles', 'futures', 'options',
        'token_trajectory', 'order_book', 'all_prices', 'trade_tapes'.

        Returns:
            List of field name strings.
        """
        ...

    @abstractmethod
    def predict(self, ctx: PredictionRequest) -> Optional[StrategyResult]:
        """Produce a prediction for the given market context.

        Args:
            ctx: Full prediction context including round info, prices, candles, etc.

        Returns:
            StrategyResult with p_up, confidence, and optional pipeline hints.
            None to abstain (insufficient data, outside operating window, etc.).
        """
        ...

    def weight(self) -> float:
        """Ensemble weight for this strategy. Higher = more influence.

        Override per strategy. Default 1.0. The weight is used by the ensemble
        combiner to scale this strategy's contribution relative to others.

        Returns:
            Weight as a float (typically 1.0 to 8.0).
        """
        return 1.0

    def min_data_points(self) -> int:
        """Minimum number of micro_candles needed for this strategy to operate.

        Checked by the strategy itself in predict(). This method is informational
        and used by the /strategies listing endpoint.

        Returns:
            Minimum candle count (default 10).
        """
        return 10

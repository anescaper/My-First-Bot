"""Strategy registry with self-registration via decorator.

The registry uses a class-level dict to store all strategy instances. Strategies
are registered at module import time via the @register_strategy decorator (see
bottom of this file). Import order is controlled by strategies/__init__.py.

This design avoids manual strategy wiring -- adding a new strategy only requires:
1. Create the file with @register_strategy decorator.
2. Import it in strategies/__init__.py.
3. Add it to the appropriate profile in PROFILE_STRATEGIES.
"""

from typing import Optional

from strategy_service.strategies.base import Strategy


class StrategyRegistry:
    """Global registry of all strategy instances. Strategies register themselves on import."""

    _strategies: dict[str, Strategy] = {}

    @classmethod
    def register(cls, strategy: Strategy):
        """Register a strategy instance. Raises ValueError on duplicate names.

        Args:
            strategy: An instantiated Strategy subclass.
        """
        if strategy.name() in cls._strategies:
            raise ValueError(f"Duplicate strategy name: {strategy.name()}")
        cls._strategies[strategy.name()] = strategy

    @classmethod
    def get(cls, name: str) -> Optional[Strategy]:
        """Look up a strategy by name. Returns None if not found."""
        return cls._strategies.get(name)

    @classmethod
    def all_names(cls) -> list[str]:
        """Return all registered strategy names."""
        return list(cls._strategies.keys())

    @classmethod
    def for_profile(cls, profile: str) -> list[Strategy]:
        """Return the list of strategy instances enabled for a given bot profile.

        Looks up the profile in PROFILE_STRATEGIES and resolves each name to
        its registered instance. Silently skips names that are not registered
        (defensive against typos in PROFILE_STRATEGIES).

        Args:
            profile: Bot profile name (e.g. 'garch-t', 'fair-value').

        Returns:
            List of Strategy instances for this profile. Empty if profile is unknown.
        """
        mapping = PROFILE_STRATEGIES.get(profile, [])
        return [cls._strategies[n] for n in mapping if n in cls._strategies]


# Profile -> strategy list mapping.
#
# Design philosophy: each bot profile runs a SMALL, focused set of strategies.
# Most profiles run a SINGLE strategy (solo mode) for truly independent decisions.
# The "control" and "momentum-combo" profiles use ensemble mode with multiple strategies.
#
# Data source diversity across profiles:
#   micro_candles (Binance spot): garch_t, imm_regime, monte_carlo_gbm, stoch_vol_filter
#   futures (Binance Futures):    futures_positioning
#   token_trajectory (Poly CLOB): token_flow_divergence
#   order_book + tokens (Poly):   spread_dynamics, clob_microstructure
#   multi-asset candles:          factor_model
#   parent-child rounds:          brownian_bridge, hierarchical_cascade
#
# "baseline" profile: not listed here -- falls through to Rust-only strategies.
PROFILE_STRATEGIES: dict[str, list[str]] = {
    "control": [
        "candle_trend", "vol_regime", "volume_breakout", "cross_asset",
    ],
    "garch-t": ["garch_t"],
    "garch-t-aggressive": ["garch_t"],
    "imm-adaptive": ["imm_regime"],
    "monte-carlo": ["monte_carlo_gbm"],
    "stochastic-vol": ["stoch_vol_filter"],
    "lmsr-filter": ["candle_trend", "lmsr_liquidity_filter"],
    "bilateral-mm": ["spread_dynamics"],
    "factor-model": ["factor_model"],
    "garch-t-options": ["garch_t", "options_flow"],
    "momentum-combo": [
        "momentum_garch", "candle_trend", "vol_regime",
        "volume_breakout", "cross_asset",
    ],
    "clob-microstructure": ["clob_microstructure"],
    "fair-value": ["brownian_bridge", "hierarchical_cascade"],
}


def register_strategy(cls):
    """Decorator for auto-registration. Apply to Strategy subclass.

    Instantiates the class and registers the instance in StrategyRegistry.
    Must be applied at module level so registration happens on import.

    Usage:
        @register_strategy
        class MyStrategy(Strategy):
            def name(self) -> str: return "my_strategy"
            ...

    Args:
        cls: A Strategy subclass (not an instance).

    Returns:
        The class unchanged (so it can still be instantiated manually in tests).
    """
    StrategyRegistry.register(cls())
    return cls

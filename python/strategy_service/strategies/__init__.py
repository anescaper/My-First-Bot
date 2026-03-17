# Strategy module imports -- triggers @register_strategy decorators on import.
#
# Each import causes the strategy class to be instantiated and registered in
# StrategyRegistry. The order does not matter (registration is by name, not order).
# To add a new strategy: create the file, apply @register_strategy, import here.
from . import garch_t, candle_trend, vol_regime, volume_breakout, cross_asset_momentum
from . import futures_positioning, token_flow, imm_regime, monte_carlo
from . import stoch_vol, lmsr_filter
from . import spread_dynamics, factor_model
from . import options_flow
from . import momentum_garch
from . import clob_microstructure
from . import fair_value_dislocation
from . import brownian_bridge

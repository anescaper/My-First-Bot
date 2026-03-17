"""Pydantic models for Rust <-> Python communication.

These models define the JSON serialization contract between the Rust bot pipeline
and the Python strategy service. The Rust side serializes to JSON, sends over HTTP,
and the Python side deserializes into these Pydantic models.

Field names use short forms (o, h, l, c, v, t) for MicroCandleData to minimize
JSON payload size, since each request may contain 60+ candles.
"""

from pydantic import BaseModel
from typing import Optional


class RoundInfo(BaseModel):
    """Metadata about the Polymarket prediction round being evaluated.

    Sent with every prediction request. Contains the market state (token prices),
    timing info (seconds remaining, progress), and the round's identity.
    """

    condition_id: str
    """Polymarket condition_id uniquely identifying this round."""
    asset: str
    """Asset slug: 'BTC', 'ETH', 'SOL', or 'XRP'."""
    timeframe: str
    """Timeframe slug: '5m', '15m', or '1h'."""
    price_up: float
    """Current market price of the UP outcome token (0.0 to 1.0)."""
    price_down: float
    """Current market price of the DOWN outcome token (0.0 to 1.0)."""
    seconds_remaining: int
    """Seconds until this round expires and settles."""
    progress_pct: float
    """Fraction of the round that has elapsed (0.0 to 1.0)."""
    timeframe_seconds: int
    """Total duration of this round in seconds (e.g. 300 for 5m)."""


class MicroCandleData(BaseModel):
    """A single 5-second OHLCV candle from Binance spot WebSocket.

    Uses single-character field names to minimize JSON payload size.
    Each prediction request typically contains 30-60 micro candles
    covering the elapsed portion of the round.
    """

    o: float  # open price
    h: float  # high price
    l: float  # low price
    c: float  # close price
    v: float  # volume in base asset units
    t: int    # unix timestamp (seconds)


class LiquidationEvent(BaseModel):
    """A single forced liquidation event from Binance Futures.

    Used by the futures_positioning strategy to detect squeeze cascades.
    """

    side: str = ""
    """'BUY' or 'SELL' -- the side that was forcibly closed."""
    price: float = 0.0
    """Liquidation execution price."""
    quantity: float = 0.0
    """Liquidation size in base asset units."""
    notional: float = 0.0
    """Liquidation size in USD notional (price * quantity)."""
    timestamp: str = ""
    """ISO 8601 timestamp of the liquidation."""


class FuturesData(BaseModel):
    """Binance perpetual futures market state for a single asset.

    Provides key futures microstructure signals: funding rate (crowding),
    taker buy/sell ratio (aggression), OI change (conviction), and
    recent liquidations (cascade/squeeze detection).
    """

    funding_rate: float = 0.0
    """Current 8-hour funding rate. Positive = longs pay shorts."""
    open_interest: float = 0.0
    """Total open interest in USD notional."""
    taker_buy_sell_ratio: float = 1.0
    """Ratio of taker buy to taker sell volume. >1 = net buying."""
    oi_change_5m: float = 0.0
    """OI percentage change over last 5 minutes."""
    liquidations: list[LiquidationEvent] = []
    """Recent forced liquidation events."""


class OptionsData(BaseModel):
    """Deribit options market state (BTC/ETH only).

    DVOL is the primary field used by the options_flow strategy.
    skew and put_call_ratio are defined but not yet populated by the adapter.
    """

    iv_atm: float = 0.0
    """At-the-money implied volatility (annualized, decimal)."""
    skew: float = 0.0
    """25-delta put-call skew. Positive = puts more expensive."""
    put_call_ratio: float = 1.0
    """Put/call open interest ratio."""
    dvol: float = 0.0
    """Deribit Volatility Index (annualized %, e.g. 45 = 45%)."""


class TokenTickData(BaseModel):
    """A single tick from the Polymarket CLOB token price trajectory.

    Captures the market-implied probability of UP/DOWN at a point in time.
    Used by the token_flow_divergence strategy for velocity/acceleration signals.
    """

    p_up: float
    """Market-implied probability of UP outcome (0.0 to 1.0)."""
    p_down: float
    """Market-implied probability of DOWN outcome."""
    t: int
    """Unix timestamp (seconds)."""


class TimeframeIntel(BaseModel):
    """Cross-timeframe intelligence from the Rust pipeline."""
    agreement_score: float = 0.0        # -1.0 (all Down) to +1.0 (all Up)
    market_bias: dict[str, float] = {}   # timeframe slug -> market p_up
    parent_bias: float = 0.5            # nearest parent timeframe p_up
    direction_agreement_up: float = 0.5  # fraction of TFs that say Up
    direction_agreement_down: float = 0.5  # fraction of TFs that say Down


class PolyFillData(BaseModel):
    """A single fill (trade) from the Polymarket CLOB.

    Used by clob_microstructure for trade flow imbalance computation.
    When is_buyer_maker is False, the buyer was the aggressive taker --
    this is the 'informed flow' signal.
    """

    side: str = ""
    """Which outcome token was traded: 'Up' or 'Down'."""
    price: float = 0.0
    """Execution price (0.0 to 1.0 for binary outcome tokens)."""
    size: float = 0.0
    """Fill size in token units (shares)."""
    is_buyer_maker: bool = False
    """True if buyer was passive (maker). False = buyer was aggressive taker."""
    t: int = 0
    """Unix timestamp (seconds)."""


class ChildOutcome(BaseModel):
    """Outcome of a prior child round within the same parent window."""
    child_index: int          # 0, 1, or 2
    direction: str            # "Up" or "Down"
    timeframe: str            # e.g. "5m"


class ParentChildContext(BaseModel):
    """Brownian Bridge context: parent round + prior child outcomes."""
    child_index: int = -1                    # Which child is this? (0, 1, 2). -1 = unknown
    parent_direction: str = "Unknown"        # Estimated parent direction: "Up"/"Down"/"Unknown"
    parent_confidence: float = 0.5           # How confident in parent direction (from CLOB price)
    prior_children: list[ChildOutcome] = []  # Resolved prior children in this parent window


class PredictionRequest(BaseModel):
    """The full prediction request sent from the Rust pipeline to the strategy service.

    Contains all market data needed by any strategy: round metadata, spot prices,
    candle history, futures/options state, CLOB data, and cross-timeframe intelligence.
    Not every strategy uses every field -- each declares its required_data() to
    indicate which fields it needs (used for documentation, not enforcement).
    """

    round: RoundInfo
    """Metadata about the Polymarket round being evaluated."""
    reference_price: float
    """Spot price at round start (the binary option strike price)."""
    current_price: float
    """Latest Binance spot price for this asset."""
    micro_candles: list[MicroCandleData] = []
    futures: Optional[FuturesData] = None
    options: Optional[OptionsData] = None
    token_trajectory: list[TokenTickData] = []
    order_book: Optional[dict] = None
    trade_tapes: list[PolyFillData] = []
    coinbase_premium: float = 0.0
    all_prices: dict[str, float] = {}
    all_reference_prices: dict[str, float] = {}
    strategy_profile: str = "garch-t"
    timeframe_intel: Optional[TimeframeIntel] = None
    parent_child: Optional[ParentChildContext] = None


class StrategyResult(BaseModel):
    """Output from a single strategy's predict() method.

    Contains the probability estimate, confidence level, and optional pipeline
    hints that guide the Rust pipeline's entry/exit behavior.
    """

    p_up: float
    """Estimated probability that the round settles UP (0.0 to 1.0)."""
    confidence: float
    """How confident the strategy is in its prediction (0.0 to 1.0).
    Used as a weight in ensemble combination and as a multiplier for edge calculation."""
    meta: dict = {}
    """Strategy-specific diagnostic metadata. Passed through to the API for debugging."""
    # Pipeline hints: strategies can guide entry/exit behavior
    hold_to_resolution: bool = False
    """If True, the pipeline should hold this position until round settlement
    rather than using stop-loss or early exit logic."""
    min_progress: Optional[float] = None
    """Minimum round progress (0.0-1.0) at which the pipeline should consider entering."""
    max_progress: Optional[float] = None
    """Maximum round progress beyond which the pipeline should not enter."""
    max_entry_price: Optional[float] = None
    """Maximum token price at which entry is acceptable (avoids buying at 0.90+)."""


class ComponentResult(BaseModel):
    """Per-strategy contribution to the ensemble result.

    Included in the PredictionResponse for transparency -- the Rust pipeline
    and frontend can see how each strategy voted.
    """

    name: str
    """Strategy name (matches registry name)."""
    p_up: float
    """This strategy's p_up estimate."""
    confidence: float
    """This strategy's confidence level."""


class PredictionResponse(BaseModel):
    """Final prediction returned to the Rust pipeline.

    This is the output of the ensemble combiner (or solo pass-through for
    single-strategy profiles). The Rust pipeline uses `direction` and `edge`
    to decide whether to enter a trade, and `confidence` for position sizing.
    """

    p_up: float
    """Combined probability estimate for UP outcome."""
    confidence: float
    """Combined confidence level (used for position sizing via Kelly criterion)."""
    edge: float
    """Expected edge: confidence * |p_up - market_price_up|."""
    direction: str
    """Trading direction: 'Up', 'Down', or 'Skip'."""
    components: list[ComponentResult] = []
    """Per-strategy breakdown of the ensemble."""
    meta: dict = {}
    """Ensemble metadata: mode, agreement, strategy hints."""

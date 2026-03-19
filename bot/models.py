"""
Shared types — dataclasses used across all modules.
Every inter-module communication uses these types, never raw dicts/tuples.

To extend: add new dataclasses here, import where needed.
"""
from dataclasses import dataclass
from typing import Optional


@dataclass
class Round:
    """A single 5-minute crypto binary options round."""
    round_ts: int           # Unix timestamp of round start
    asset: str              # btc/eth/sol/xrp
    condition_id: str       # Polymarket condition ID
    up_token: str           # Token ID for UP outcome
    down_token: str         # Token ID for DOWN outcome
    status: str = "new"     # new/placed/signaled/settled/skipped


@dataclass
class Order:
    """An order placed by the bot."""
    order_id: str           # Polymarket order ID
    round_ts: int
    asset: str
    token_side: str         # "UP" or "DOWN"
    order_type: str         # "BUY" or "SELL"
    price: float
    size: float
    status: str = "open"    # open/filled/cancelled
    placed_at: int = 0
    filled_at: Optional[int] = None


@dataclass
class Position:
    """An active position (BUY filled, awaiting exit)."""
    id: int
    round_ts: int
    asset: str
    token_side: str         # "UP" or "DOWN"
    entry_price: float
    entry_size: float
    entry_order: str        # BUY order ID
    sell_order: Optional[str] = None
    sell_price: Optional[float] = None
    sell_placed_at: Optional[int] = None  # when the current sell order was placed
    filled_at: Optional[int] = None       # when the BUY filled (position opened)
    status: str = "open"    # open/exiting/fallback/stepdown/emergency/closed
    pnl: Optional[float] = None
    opened_at: int = 0
    closed_at: Optional[int] = None


@dataclass
class Signal:
    """A directional signal from competitor tracking."""
    asset: str
    round_ts: int
    direction: str          # "UP" or "DOWN" — the side to KEEP
    source: str             # "competitor" or "bridge"
    confidence: float       # 0.0 to 1.0

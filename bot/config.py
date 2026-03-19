"""
Configuration — all tunable parameters in one place.
No parameter should ever be hardcoded elsewhere.

To extend: add new constants here, import in the module that needs them.

Strategy: Symmetric Vol Harvest v5 (unified exit)
  - BUY both UP and DOWN at $0.27 (4h early, GTC)
  - When one side fills, cancel opposite BUY, place SELL at $0.48
  - All assets use the same exit logic (no lucky settlement)
"""

# ── Assets ───────────────────────────────────────────────────
ASSETS: list[str] = ["btc", "eth", "sol", "xrp"]
TIMEFRAME: str = "5m"
ROUND_DURATION_S: int = 300

# ── Exit Strategy ──────────────────────────────────────────
# All assets: cancel opposite BUY on fill, emergency exit if sell fails
LUCKY_SETTLEMENT: set[str] = set()

# ── Order Pricing ────────────────────────────────────────────
BUY_PRICE: float = 0.27       # buy on both UP and DOWN at this price
SELL_TARGET: float = 0.48     # sell on rebound
MIN_SELL_PRICE: float = 0.30  # minimum price to accept on step-down sell (must be > BUY_PRICE)
SELL_STEPDOWN_S: int = 120    # step-down when < 120s (2min) left in round

# ── Order Sizing (shares) ───────────────────────────────────
# Symmetric: same size on both sides
# 19 × $0.27 = $5.13 per order, $10.26 per round-asset pair
# Start small for data collection, scale up once win rate is known
BUY_SIZE: int = 19
EMERGENCY_SELL_PRICE: float = 0.01

# ── Timing (seconds) ────────────────────────────────────────
LOOKAHEAD_HOURS: int = 4
DISCOVERY_INTERVAL_S: int = 300       # discover new rounds every 5 min
FILL_CHECK_INTERVAL_S: float = 1.0    # check fills every 1s (only active round polled)
EXIT_DEADLINE_S: int = 90            # emergency exit when < 90s (1.5min) left in round
BRAKE_PAUSE_S: int = 3600            # pause mode: cancel orders within this window (1 hour)

# ── Budget ───────────────────────────────────────────────────
# 4 assets × 2 orders × 19 × $0.27 = $41.04 per round set
# $350 / $41.04 = ~8 round sets ahead = 40 min coverage
BUDGET_TOTAL: float = 99999.0  # no artificial cap
MAX_OPEN_ORDERS: int = 2000
MAX_POSITIONS: int = 64
DAILY_DRAWDOWN_LIMIT: float = 9999.0  # disabled — pause trading

# ── API ──────────────────────────────────────────────────────
CLOB_HOST: str = "https://clob.polymarket.com"
GAMMA_HOST: str = "https://gamma-api.polymarket.com"
CHAIN_ID: int = 137
SECRETS_DIR: str = "/home/ubuntu/polybot/secrets"
API_DELAY_S: float = 0.35

# ── Paths ────────────────────────────────────────────────────
BOT_DB_PATH: str = "/home/ubuntu/polybot/bot.db"
COMPETITOR_DB_PATH: str = "/home/ubuntu/competitor_tracker.db"
LOG_PATH: str = "/home/ubuntu/polybot/bot.log"

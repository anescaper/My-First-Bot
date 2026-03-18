"""
Configuration — all tunable parameters in one place.
No parameter should ever be hardcoded elsewhere.

To extend: add new constants here, import in the module that needs them.

Strategy: Symmetric Vol Harvest
  - BUY both UP and DOWN at $0.26 (12h early, GTC)
  - When one side fills (market moves), place SELL at $0.48
  - Profit: $0.22/share on reversion toward center
  - Cancel opposite BUY on fill (only one side fills per round)
"""

# ── Assets ───────────────────────────────────────────────────
ASSETS: list[str] = ["btc", "eth"]
TIMEFRAME: str = "5m"
ROUND_DURATION_S: int = 300

# ── Order Pricing ────────────────────────────────────────────
BUY_PRICE: float = 0.26       # buy on both UP and DOWN at this price
SELL_TARGET: float = 0.48     # sell below center — partial reversion, not full reversal

# ── Order Sizing (shares) ───────────────────────────────────
# Symmetric: same size on both sides
# 19 × $0.26 = $4.94 per order, $9.88 per round-asset pair
# Start small for data collection, scale up once win rate is known
BUY_SIZE: int = 19
EMERGENCY_SELL_PRICE: float = 0.01

# ── Timing (seconds) ────────────────────────────────────────
LOOKAHEAD_HOURS: int = 12
DISCOVERY_INTERVAL_S: int = 300       # discover new rounds every 5 min
FILL_CHECK_INTERVAL_S: int = 3       # check fills every 3s
EXIT_DEADLINE_S: int = 240           # market-sell at T+4min

# ── Budget ───────────────────────────────────────────────────
# $350 / $20.80 per pair = 16 round-asset pairs max
# 16 pairs = 32 orders (UP + DOWN each)
# 4 assets → 4 rounds ahead = 20 min coverage
# 2 assets → 8 rounds ahead = 40 min coverage
BUDGET_TOTAL: float = 350.0
MAX_OPEN_ORDERS: int = 32
MAX_POSITIONS: int = 8
DAILY_DRAWDOWN_LIMIT: float = 35.0   # 10% of budget — pause trading

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

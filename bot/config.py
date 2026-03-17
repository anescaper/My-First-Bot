"""
Configuration — all tunable parameters in one place.
No parameter should ever be hardcoded elsewhere.

To extend: add new constants here, import in the module that needs them.
"""

# ── Assets ───────────────────────────────────────────────────
ASSETS: list[str] = ["btc", "eth", "sol", "xrp"]
TIMEFRAME: str = "5m"
ROUND_DURATION_S: int = 300

# ── Order Pricing ────────────────────────────────────────────
BUY_PRICE: float = 0.20
SELL_TARGET: float = 0.35

# ── Order Sizing (shares) ───────────────────────────────────
SIZE_UP: int = 15       # 15 × $0.20 = $3.00
SIZE_DOWN: int = 10     # 10 × $0.20 = $2.00

# ── Timing (seconds) ────────────────────────────────────────
LOOKAHEAD_HOURS: int = 12
DISCOVERY_INTERVAL_S: int = 300       # discover new rounds every 5 min
FILL_CHECK_INTERVAL_S: int = 3       # check fills every 3s
SIGNAL_WINDOW_START_S: int = 120     # start checking signals at T-120s
SIGNAL_WINDOW_END_S: int = 30        # stop checking signals at T-30s
EXIT_DEADLINE_S: int = 240           # market-sell at T+4min

# ── Budget ───────────────────────────────────────────────────
BUDGET_TOTAL: float = 350.0
MAX_OPEN_ORDERS: int = 60
MAX_POSITIONS: int = 5

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

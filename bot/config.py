"""
Configuration — all tunable parameters in one place.
No parameter should ever be hardcoded elsewhere.

To extend: add new constants here, import in the module that needs them.

Strategy: Symmetric Vol Harvest v5.2 (waterfall exit)
  - BUY both UP and DOWN at $0.27 (4h early, GTC)
  - When one side fills, cancel opposite BUY
  - Exit waterfall: $0.48 (30s) → $0.35 → step-down (best bid) → $0.15 stop-loss
"""

# ── Assets ───────────────────────────────────────────────────
ASSETS: list[str] = ["btc", "eth", "sol", "xrp"]
TIMEFRAME: str = "5m"
ROUND_DURATION_S: int = 300

# ── Exit Waterfall ───────────────────────────────────────────
# Tier 1: SELL at $0.48 immediately on fill (dream price, 30s window)
# Tier 2: Drop to $0.35 if $0.48 doesn't fill in 30s
# Tier 3: Step-down to best bid when < 120s left (one shot)
# Tier 4: Stop-loss at $0.15 when < 90s left (one shot)

# ── Order Pricing ────────────────────────────────────────────
BUY_PRICE: float = 0.27       # buy on both UP and DOWN at this price
SELL_TARGET: float = 0.48     # tier 1: dream price (30s window)
SELL_FALLBACK: float = 0.35   # tier 2: realistic target after 30s
SELL_FALLBACK_S: int = 30     # seconds to wait at dream price before falling back
MIN_SELL_PRICE: float = 0.30  # minimum price to accept on step-down sell
SELL_STEPDOWN_S: int = 120    # tier 3: step-down when < 120s (2min) left in round

# ── Order Sizing (shares) ───────────────────────────────────
# Symmetric: same size on both sides
# 19 × $0.27 = $5.13 per order, $10.26 per round-asset pair
BUY_SIZE: int = 19
EMERGENCY_SELL_PRICE: float = 0.15    # tier 4: stop-loss (not $0.01)

# ── Timing (seconds) ────────────────────────────────────────
STALE_BUY_CUTOFF_S: int = 58         # cancel unfilled BUYs after this many seconds into round
LOOKAHEAD_HOURS: int = 4
DISCOVERY_INTERVAL_S: int = 300       # discover new rounds every 5 min
FILL_CHECK_INTERVAL_S: float = 1.0    # check fills every 1s (only active round polled)
EXIT_DEADLINE_S: int = 90            # tier 4 stop-loss when < 90s (1.5min) left in round
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

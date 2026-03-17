#!/usr/bin/env python3
"""
Main — entry point and event loop.
Orchestrates all modules. Each tick: discover → signal → fill → exit → cleanup.

To extend: add new tick handlers in the main loop.
"""
import time
import signal
import logging

import config as C
import db
from client import create_client
from discovery import discover_rounds
from orders import place_preorders, check_fills, cancel_all
from signals import process_signals
from exits import manage_exits
from cleanup import cleanup_old_rounds

# ── Logging ──────────────────────────────────────────────────
logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [%(levelname)s] %(message)s",
    handlers=[
        logging.FileHandler(C.LOG_PATH),
        logging.StreamHandler(),
    ],
)
log = logging.getLogger("bot")

# ── Shutdown flag ────────────────────────────────────────────
_shutdown = False


def _handle_signal(signum, frame):
    global _shutdown
    _shutdown = True
    log.warning(f"Received signal {signum}, shutting down...")


def main():
    global _shutdown

    signal.signal(signal.SIGINT, _handle_signal)
    signal.signal(signal.SIGTERM, _handle_signal)

    log.info("=" * 50)
    log.info("VOL HARVEST BOT STARTING")
    log.info(f"  Budget: ${C.BUDGET_TOTAL}")
    log.info(f"  Buy: ${C.BUY_PRICE} → Sell: ${C.SELL_TARGET}")
    log.info(f"  Assets: {C.ASSETS}")
    log.info(f"  Lookahead: {C.LOOKAHEAD_HOURS}h")
    log.info("=" * 50)

    client = create_client()
    conn = db.init_db()

    last_discovery = 0
    last_cleanup = 0
    tick = 0

    while not _shutdown:
        try:
            now = time.time()
            tick += 1

            # ── Discovery: every DISCOVERY_INTERVAL ──────────
            if now - last_discovery > C.DISCOVERY_INTERVAL_S:
                discover_rounds(conn)
                place_preorders(client, conn)
                last_discovery = now

            # ── Signals: every tick (critical window only) ───
            process_signals(client, conn)

            # ── Fill check: every FILL_CHECK_INTERVAL ────────
            if tick % C.FILL_CHECK_INTERVAL_S == 0:
                check_fills(client, conn)

            # ── Exit management: every 5 ticks ───────────────
            if tick % 5 == 0:
                manage_exits(client, conn)

            # ── Cleanup: every 5 min ─────────────────────────
            if now - last_cleanup > 300:
                cleanup_old_rounds(client, conn)
                last_cleanup = now

            # ── Stats: every 100 ticks ───────────────────────
            if tick % 100 == 0:
                _log_stats(conn)

            time.sleep(1)

        except KeyboardInterrupt:
            _shutdown = True
        except Exception as e:
            log.error(f"Main loop error: {e}", exc_info=True)
            time.sleep(5)

    # ── Graceful shutdown ────────────────────────────────────
    log.info("Shutting down gracefully...")
    cancel_all(client, conn)
    _log_stats(conn)
    conn.close()
    log.info("Bot stopped.")


def _log_stats(conn):
    """Log current bot statistics."""
    open_orders = db.count_open_orders(conn)
    open_pos = db.count_open_positions(conn)
    pnl = db.total_pnl(conn)
    log.info(
        f"📊 Orders: {open_orders} | Positions: {open_pos} | "
        f"P&L: ${pnl:+.2f}"
    )


if __name__ == "__main__":
    main()

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
from orders import place_preorders, check_fills, cancel_all, cancel_inactive_buys
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
    log.info("VOL HARVEST BOT v5 — UNIFIED EXIT")
    log.info(f"  Budget: ${C.BUDGET_TOTAL}")
    log.info(f"  Buy: ${C.BUY_PRICE} → Sell: ${C.SELL_TARGET}")
    log.info(f"  Size: {C.BUY_SIZE} shares/side (${C.BUY_PRICE * C.BUY_SIZE:.2f}/order)")
    log.info(f"  Assets: {C.ASSETS}")
    log.info(f"  Lookahead: {C.LOOKAHEAD_HOURS}h | Fill check: {C.FILL_CHECK_INTERVAL_S}s")
    log.info(f"  Max orders/positions: {C.MAX_OPEN_ORDERS}/{C.MAX_POSITIONS}")
    log.info("=" * 50)

    client = create_client()

    # Wipe DB on startup — all real orders are cancelled anyway, stale DB state
    # causes ghost orders, fake brake triggers, and skipped rounds
    db.wipe_db()
    conn = db.init_db()
    log.info("Startup: DB wiped (clean slate)")

    # Cancel all existing orders on Polymarket to prevent duplicates from previous runs
    try:
        result = client.cancel_all()
        cancelled = result.get("canceled", [])
        log.info(f"Startup: cancelled {len(cancelled)} stale orders on Polymarket")
    except Exception as e:
        log.warning(f"Startup cancel_all failed: {e}")

    last_discovery = 0
    last_cleanup = 0
    last_fill_check = 0
    last_exit_check = 0
    last_stats = 0
    last_brake_check = 0
    brake_active = False
    tick = 0

    while not _shutdown:
        try:
            now = time.time()
            tick += 1

            # ── Emergency brake: 2+ assets failed to sell in last 2 rounds ──
            # Step 1: Stop new order generation
            # Step 2: Cancel all inactive (future) round BUY orders
            # Step 3: Keep processing active round (fills + exits) until it ends
            if now - last_brake_check > 10:
                failed_assets = db.recent_failed_sell_assets(conn, window_s=600)
                if len(failed_assets) >= 2 and not brake_active:
                    brake_active = True
                    log.warning(
                        f"🚨 EMERGENCY BRAKE: {len(failed_assets)} assets "
                        f"({', '.join(sorted(failed_assets))}) failed to sell "
                        f"in last 2 rounds"
                    )
                    # Cancel only future/inactive BUY orders, keep active round alive
                    cancel_inactive_buys(client, conn)
                elif len(failed_assets) < 2 and brake_active:
                    brake_active = False
                    log.info("✅ Emergency brake released — resuming normal operation")
                last_brake_check = now

            # ── Drawdown circuit breaker ───────────────────
            daily_loss = db.today_pnl(conn)
            if daily_loss <= -C.DAILY_DRAWDOWN_LIMIT:
                if tick % 100 == 1:  # Log once every ~100s
                    log.warning(
                        f"🛑 DRAWDOWN LIMIT: ${daily_loss:+.2f} today "
                        f"(limit -${C.DAILY_DRAWDOWN_LIMIT}). No new orders."
                    )
            elif brake_active:
                if tick % 100 == 1:
                    log.warning("🚨 BRAKE ACTIVE: no new orders until failed sells clear")
            else:
                # ── Discovery: every DISCOVERY_INTERVAL ──────────
                if now - last_discovery > C.DISCOVERY_INTERVAL_S:
                    discover_rounds(conn)
                    place_preorders(client, conn)
                    last_discovery = now

            # ── Signals: every tick (log only, no cancellation) ──
            process_signals(conn)

            # ── Fill check: every 3s ─────────────────────────
            if now - last_fill_check > C.FILL_CHECK_INTERVAL_S:
                check_fills(client, conn)
                last_fill_check = now

            # ── Exit management: every 5s ────────────────────
            if now - last_exit_check > 5:
                manage_exits(client, conn)
                last_exit_check = now

            # ── Cleanup: every 5 min ─────────────────────────
            if now - last_cleanup > 300:
                cleanup_old_rounds(client, conn)
                last_cleanup = now

            # ── Stats: every 100s ────────────────────────────
            if now - last_stats > 100:
                _log_stats(conn)
                last_stats = now

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

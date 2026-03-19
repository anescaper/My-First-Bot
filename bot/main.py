#!/usr/bin/env python3
"""
Main — entry point and event loop.
Orchestrates all modules. Each tick: discover → signal → fill → exit → cleanup.

To extend: add new tick handlers in the main loop.
"""
import time
import signal
import logging
from datetime import datetime, timezone, timedelta

import config as C
import db
from client import create_client
from discovery import discover_rounds
from orders import place_preorders, check_fills, cancel_all, cancel_inactive_buys, cancel_near_term_buys
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
    # causes ghost orders, fake pause triggers, and skipped rounds
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
    last_round_summary = 0   # track which round we last logged
    pause_active = False
    pause_activated_at = 0
    tick = 0

    while not _shutdown:
        try:
            now = time.time()
            tick += 1

            # ── Round summary table: log at end of each 5-min round ──
            current_round_ts = (int(now) // C.ROUND_DURATION_S) * C.ROUND_DURATION_S
            prev_round_ts = current_round_ts - C.ROUND_DURATION_S
            # When a new round starts, log the summary of the round that just ended
            if prev_round_ts > last_round_summary:
                _log_round_summary(conn, prev_round_ts)
                last_round_summary = prev_round_ts

                # ── Emergency pause: check prev + prev-prev round ──
                # 2 rounds × 4 assets = 8 positions. If 2+ failures → pause
                failures = db.get_pause_failures(conn, prev_round_ts)
                if len(failures) >= 2 and not pause_active:
                    pause_active = True
                    pause_activated_at = now
                    failed_desc = ", ".join(f"{a}@{ts}" for a, ts in failures)
                    log.warning(
                        f"⏸️ PAUSE MODE: {len(failures)} failures "
                        f"in last 2 rounds ({failed_desc})"
                    )
                    log.warning(
                        f"⏸️ Cancelling orders within next "
                        f"{C.BRAKE_PAUSE_S // 60} min — "
                        f"keeping far-future orders + discovery alive"
                    )
                    cancel_near_term_buys(client, conn)
                elif len(failures) < 2 and pause_active:
                    pause_active = False
                    log.info("✅ Pause mode released — resuming full operation")

            # ── Discovery + pre-orders: ALWAYS run (even during pause) ──
            # During pause, discovery still finds rounds and pre-orders still
            # places far-future orders. Only near-term orders get cancelled.
            if now - last_discovery > C.DISCOVERY_INTERVAL_S:
                # Drawdown check
                daily_loss = db.today_pnl(conn)
                if daily_loss <= -C.DAILY_DRAWDOWN_LIMIT:
                    if tick % 100 == 1:
                        log.warning(
                            f"🛑 DRAWDOWN LIMIT: ${daily_loss:+.2f} today "
                            f"(limit -${C.DAILY_DRAWDOWN_LIMIT}). No new orders."
                        )
                else:
                    discover_rounds(conn)
                    place_preorders(client, conn)
                    last_discovery = now

                    # During pause: continuously cancel near-term orders
                    # (new ones may have been placed by pre-orders)
                    if pause_active:
                        cancel_near_term_buys(client, conn)

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


def _log_round_summary(conn, round_ts: int):
    """Log a table summarizing all 4 assets for a completed round."""
    summary = db.get_round_summary(conn, round_ts)
    if not summary:
        return

    # Convert round_ts to human-readable ET time
    et = datetime.fromtimestamp(round_ts, tz=timezone(timedelta(hours=-4)))
    end_et = et + timedelta(seconds=C.ROUND_DURATION_S)
    time_label = f"{et.strftime('%I:%M')}-{end_et.strftime('%I:%M %p')} ET"

    log.info("=" * 70)
    log.info(f"📋 ROUND SUMMARY: {time_label} (ts={round_ts})")
    log.info("-" * 70)
    log.info(f"{'Asset':>5s} | {'Side':>4s} | {'Buy Cost':>9s} | {'Sell Rev':>9s} | {'P&L':>8s} | Result")
    log.info("-" * 70)

    total_pnl = 0.0
    failures = 0

    for asset in C.ASSETS:
        info = summary.get(asset, {"result": "no_fill"})
        result = info["result"]

        if result == "no_fill":
            log.info(f"{asset.upper():>5s} | {'—':>4s} | {'—':>9s} | {'—':>9s} | {'—':>8s} | no_fill")
        elif result == "active":
            buy_cost = info.get("buy_cost", 0)
            log.info(f"{asset.upper():>5s} | {info.get('side', '?'):>4s} | ${buy_cost:>7.2f} | {'pending':>9s} | {'—':>8s} | active")
        else:
            buy_cost = info.get("buy_cost", 0)
            sell_rev = info.get("sell_revenue", 0) or 0
            pnl = info.get("pnl", 0) or 0
            total_pnl += pnl
            marker = "✅" if result == "success" else "❌"
            if result == "failure":
                failures += 1
            log.info(
                f"{asset.upper():>5s} | {info.get('side', '?'):>4s} | "
                f"${buy_cost:>7.2f} | ${sell_rev:>7.2f} | "
                f"${pnl:>+7.2f} | {marker} {result}"
            )

    log.info("-" * 70)
    log.info(f"{'TOTAL':>5s} | {'':>4s} | {'':>9s} | {'':>9s} | ${total_pnl:>+7.2f} | {failures} failure(s)")
    log.info("=" * 70)


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

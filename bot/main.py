#!/usr/bin/env python3
"""
Main — entry point for the vol harvest bot.

3-phase startup:
  Phase 1: init_future_orders()  — scan 24h ahead, store rounds in DB
  Phase 2: configure_orders()    — place BUY orders with parameters
  Phase 3: run_bot()             — main loop (safe cancel: bot orders only)
"""
import logging

import config as C
import db
from client import create_client
from startup import init_future_orders, configure_orders, run_bot

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


def main():
    log.info("=" * 50)
    log.info("VOL HARVEST BOT — 3-PHASE STARTUP")
    log.info("=" * 50)

    client = create_client()
    conn = db.init_db()

    # ── Phase 1: Discover markets 24h ahead ──────────────────
    init_future_orders(conn, lookahead_hours=24)

    # ── Phase 2: Configure and place orders ──────────────────
    configure_orders(
        client, conn,
        assets=C.ASSETS,            # ["btc", "eth", "sol", "xrp"]
        side="BOTH",                # UP, DOWN, or BOTH
        buy_price=C.BUY_PRICE,      # 0.27
        buy_amount=C.BUY_PRICE * C.BUY_SIZE,  # $5.13
        sell_price=C.SELL_TARGET,    # 0.48
        span_hours=4.0,             # 4 hours ahead
    )

    # ── Phase 3: Run the bot loop ────────────────────────────
    run_bot(client, conn)


if __name__ == "__main__":
    main()

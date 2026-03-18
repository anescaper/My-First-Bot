"""
Startup — 3-phase bot initialization.

Phase 1: init_future_orders()  — Discover markets 24h ahead, store in DB
Phase 2: configure_orders()    — Place BUY orders with user parameters
Phase 3: run_bot()             — Start the main loop with safety checks

Usage:
    client = create_client()
    conn = db.init_db()

    init_future_orders(conn, lookahead_hours=24)

    configure_orders(
        client, conn,
        assets=["btc", "eth"],
        side="BOTH",
        buy_price=0.27,
        buy_amount=5.0,
        sell_price=0.48,
        span_hours=4.0,
    )

    run_bot(client, conn)
"""
import time
import signal as sig
import logging
import sqlite3
from py_clob_client.client import ClobClient

import config as C
import db
from models import Round, Order
from discovery import fetch_market
from client import place_order, cancel_order, BUY_SIDE

log = logging.getLogger("bot.startup")


# ═════════════════════════════════════════════════════════════
# Phase 1: Initialize Future Orders
# ═════════════════════════════════════════════════════════════

def init_future_orders(conn: sqlite3.Connection, lookahead_hours: int = 24) -> int:
    """
    Discover all upcoming markets from now to +lookahead_hours.
    Queries Gamma API for each 5-min slot × each asset.
    Stores discovered rounds in the database for persistent storage.

    Args:
        conn: SQLite connection
        lookahead_hours: scan window (default 24h)

    Returns:
        Number of new rounds discovered
    """
    now = int(time.time())
    current_5m = (now // C.ROUND_DURATION_S) * C.ROUND_DURATION_S
    new_count = 0
    max_slots = lookahead_hours * (3600 // C.ROUND_DURATION_S)

    log.info("=" * 50)
    log.info("PHASE 1: INIT FUTURE ORDERS")
    log.info(f"  Scanning {lookahead_hours}h ahead "
             f"({max_slots} slots × {len(C.ASSETS)} assets)")
    log.info("=" * 50)

    miss_streak = 0

    for i in range(1, max_slots + 1):
        ts = current_5m + i * C.ROUND_DURATION_S

        # Skip rounds too close to place orders
        if ts - now < C.ROUND_DURATION_S:
            continue

        # Skip timestamps where all assets are already tracked
        all_known = all(db.round_exists(conn, ts, a) for a in C.ASSETS)
        if all_known:
            miss_streak = 0
            continue

        round_found = False
        for asset in C.ASSETS:
            if db.round_exists(conn, ts, asset):
                continue

            rnd = fetch_market(asset, ts)
            if rnd:
                db.insert_round(conn, rnd)
                new_count += 1
                round_found = True
                time.sleep(C.API_DELAY_S)

        if round_found:
            miss_streak = 0
        else:
            miss_streak += 1
            if miss_streak >= 10:
                log.info(f"  10 consecutive misses at slot {i} — API hasn't listed further")
                break

        # Progress log every 50 slots
        if i % 50 == 0:
            log.info(f"  Progress: {i}/{max_slots} slots, {new_count} rounds discovered")

    conn.commit()
    log.info(f"  Phase 1 complete: {new_count} new rounds stored in DB")
    return new_count


# ═════════════════════════════════════════════════════════════
# Phase 2: Configure and Place Orders
# ═════════════════════════════════════════════════════════════

def configure_orders(
    client: ClobClient,
    conn: sqlite3.Connection,
    *,
    assets: list[str],
    side: str = "BOTH",
    buy_price: float,
    buy_amount: float,
    sell_price: float,
    span_hours: float = 4.0,
) -> int:
    """
    Configure and place BUY orders for discovered rounds.

    Orders are placed farthest-first (last hour backward to first hour)
    so that the most distant rounds get placed before closer ones.

    Updates runtime config (C.BUY_PRICE, C.BUY_SIZE, C.SELL_TARGET)
    so the bot loop uses these values for fill processing and exits.

    Args:
        client: ClobClient instance
        conn: SQLite connection
        assets: which assets to trade, e.g. ["btc", "eth", "sol", "xrp"]
        side: "UP", "DOWN", or "BOTH"
        buy_price: limit price per share for BUY (e.g. 0.27)
        buy_amount: dollar amount per order (shares = floor(amount / price))
        sell_price: target SELL price when BUY fills (e.g. 0.48)
        span_hours: time window starting from next round (e.g. 4.0)

    Returns:
        Number of orders placed on the CLOB
    """
    now = int(time.time())
    current_5m = (now // C.ROUND_DURATION_S) * C.ROUND_DURATION_S

    # Window: from next round to +span_hours
    start_ts = current_5m + C.ROUND_DURATION_S
    end_ts = start_ts + int(span_hours * 3600)

    buy_size = int(buy_amount / buy_price)
    if buy_size < 1:
        log.error(f"Buy amount ${buy_amount} too small for price ${buy_price}")
        return 0

    sides = ["UP", "DOWN"] if side.upper() == "BOTH" else [side.upper()]

    # Update runtime config for the bot loop
    C.BUY_PRICE = buy_price
    C.BUY_SIZE = buy_size
    C.SELL_TARGET = sell_price

    log.info("=" * 50)
    log.info("PHASE 2: CONFIGURE ORDERS")
    log.info(f"  Assets: {assets}")
    log.info(f"  Side(s): {sides}")
    log.info(f"  BUY: ${buy_price} × {buy_size} shares "
             f"(${buy_price * buy_size:.2f}/order)")
    log.info(f"  SELL target: ${sell_price}")
    log.info(f"  Window: now+5m → +{span_hours}h "
             f"({start_ts} → {end_ts})")
    log.info("=" * 50)

    # Query rounds in window for the requested assets, farthest first
    placeholders = ",".join("?" * len(assets))
    rows = conn.execute(
        f"""SELECT * FROM rounds
            WHERE status = 'new'
              AND round_ts >= ? AND round_ts <= ?
              AND asset IN ({placeholders})
            ORDER BY round_ts DESC""",
        [start_ts, end_ts] + assets
    ).fetchall()
    rounds = [Round(**dict(r)) for r in rows]

    log.info(f"  {len(rounds)} rounds in window (placing farthest first)")

    placed = 0
    for rnd in rounds:
        for s in sides:
            token_id = rnd.up_token if s == "UP" else rnd.down_token
            oid = place_order(client, token_id, BUY_SIDE, buy_price, buy_size)
            if oid:
                db.insert_order(conn, Order(
                    order_id=oid, round_ts=rnd.round_ts, asset=rnd.asset,
                    token_side=s, order_type="BUY",
                    price=buy_price, size=buy_size,
                    status="open", placed_at=int(time.time()),
                ))
                placed += 1

        status = "placed" if placed > 0 else "skipped"
        db.update_round_status(conn, rnd.round_ts, rnd.asset, status)
        conn.commit()

    log.info(f"  Phase 2 complete: {placed} BUY orders placed on CLOB")
    return placed


# ═════════════════════════════════════════════════════════════
# Phase 3: Run Bot
# ═════════════════════════════════════════════════════════════

def _cancel_bot_orders_only(client: ClobClient, conn: sqlite3.Connection) -> int:
    """
    Safety cancel: only cancel orders tracked in the bot's DB.
    Does NOT use client.cancel_all() — that would wipe the entire account
    including orders from other bots or manual trades.

    Returns:
        Number of orders cancelled
    """
    open_orders = db.get_open_orders(conn)
    if not open_orders:
        log.info("  No stale bot orders to cancel")
        return 0

    count = 0
    for order in open_orders:
        cancel_order(client, order.order_id)
        db.update_order_status(conn, order.order_id, "cancelled")
        count += 1

    if count:
        conn.commit()
    log.info(f"  Safety cancel: {count} bot-tracked orders cancelled")
    return count


def run_bot(client: ClobClient, conn: sqlite3.Connection):
    """
    Start the main event loop.

    Safety check at startup: only cancels orders tracked in the bot's DB.
    Does NOT call client.cancel_all() — ensures other markets are untouched.

    The loop handles: discovery → signals → fills → exits → cleanup.
    """
    from orders import check_fills, place_preorders
    from signals import process_signals
    from exits import manage_exits
    from cleanup import cleanup_old_rounds
    from discovery import discover_rounds

    log.info("=" * 50)
    log.info("PHASE 3: RUN BOT")
    log.info(f"  BUY: ${C.BUY_PRICE} × {C.BUY_SIZE} shares")
    log.info(f"  SELL target: ${C.SELL_TARGET}")
    log.info(f"  Assets: {C.ASSETS}")
    log.info(f"  Max orders/positions: {C.MAX_OPEN_ORDERS}/{C.MAX_POSITIONS}")
    log.info("=" * 50)

    # Safety: only cancel stale orders WE placed, not the whole account
    stale = db.get_open_orders(conn)
    if stale:
        log.info(f"  Found {len(stale)} stale orders from previous run — cancelling...")
        _cancel_bot_orders_only(client, conn)

    _shutdown = False

    def _handle_signal(signum, frame):
        nonlocal _shutdown
        _shutdown = True
        log.warning(f"Received signal {signum}, shutting down...")

    sig.signal(sig.SIGINT, _handle_signal)
    sig.signal(sig.SIGTERM, _handle_signal)

    last_discovery = 0
    last_cleanup = 0
    last_fill_check = 0
    last_exit_check = 0
    last_stats = 0
    tick = 0

    while not _shutdown:
        try:
            now = time.time()
            tick += 1

            # ── Drawdown circuit breaker ───────────────────
            daily_loss = db.today_pnl(conn)
            if daily_loss <= -C.DAILY_DRAWDOWN_LIMIT:
                if tick % 100 == 1:
                    log.warning(
                        f"🛑 DRAWDOWN LIMIT: ${daily_loss:+.2f} today "
                        f"(limit -${C.DAILY_DRAWDOWN_LIMIT}). No new orders."
                    )
            else:
                # ── Discovery + pre-orders ───────────────────
                if now - last_discovery > C.DISCOVERY_INTERVAL_S:
                    discover_rounds(conn)
                    place_preorders(client, conn)
                    last_discovery = now

            # ── Signals (info only in vol harvest mode) ──────
            process_signals(conn)

            # ── Fill check ───────────────────────────────────
            if now - last_fill_check > C.FILL_CHECK_INTERVAL_S:
                check_fills(client, conn)
                last_fill_check = now

            # ── Exit management ──────────────────────────────
            if now - last_exit_check > 5:
                manage_exits(client, conn)
                last_exit_check = now

            # ── Cleanup ──────────────────────────────────────
            if now - last_cleanup > 300:
                cleanup_old_rounds(client, conn)
                last_cleanup = now

            # ── Stats ────────────────────────────────────────
            if now - last_stats > 100:
                _log_stats(conn)
                last_stats = now

            time.sleep(1)

        except KeyboardInterrupt:
            _shutdown = True
        except Exception as e:
            log.error(f"Main loop error: {e}", exc_info=True)
            time.sleep(5)

    # ── Graceful shutdown — cancel only bot orders ───────────
    log.info("Shutting down gracefully...")
    _cancel_bot_orders_only(client, conn)
    _log_stats(conn)
    conn.close()
    log.info("Bot stopped.")


def _log_stats(conn: sqlite3.Connection):
    open_orders = db.count_open_orders(conn)
    open_pos = db.count_open_positions(conn)
    pnl = db.total_pnl(conn)
    log.info(
        f"📊 Orders: {open_orders} | Positions: {open_pos} | "
        f"P&L: ${pnl:+.2f}"
    )

"""
Exits — manage open positions, detect SELL fills, emergency exit.
Ensures we NEVER hold to settlement.

To extend: add new exit strategies (trailing stop, etc.) here.
"""
import time
import logging
import sqlite3
from py_clob_client.client import ClobClient

import config as C
import db
from models import Position, Order
from client import (
    cancel_order, get_order_status, place_order, SELL_SIDE
)

log = logging.getLogger("bot.exits")


def manage_exits(client: ClobClient, conn: sqlite3.Connection) -> int:
    """
    For each open position:
    1. Check if SELL filled → close with profit
    2. If past EXIT_DEADLINE → emergency market-sell

    Args:
        client: ClobClient instance
        conn: SQLite connection

    Returns:
        Number of positions closed
    """
    now = int(time.time())
    closed = 0

    positions = db.get_open_positions(conn)
    for pos in positions:
        elapsed = now - pos.round_ts

        # ── Check SELL fill ──────────────────────────────────
        if pos.sell_order and pos.status == "exiting":
            info = get_order_status(client, pos.sell_order)
            if info and info["size_matched"] > 0:
                if info["status"] in ("MATCHED", "FILLED", "CLOSED"):
                    pnl = (C.SELL_TARGET - pos.entry_price) * info["size_matched"]
                    db.close_position(
                        conn, pos.id, pnl, C.SELL_TARGET, now
                    )
                    db.update_order_status(conn, pos.sell_order, "filled", now)
                    log.info(
                        f"✅ PROFIT: {pos.asset} {pos.token_side} | "
                        f"${pos.entry_price}→${C.SELL_TARGET} | "
                        f"P&L: ${pnl:+.2f}"
                    )
                    closed += 1
                    conn.commit()
                    continue

        # ── Emergency exit at T+4min ─────────────────────────
        if elapsed >= C.EXIT_DEADLINE_S:
            log.warning(
                f"⚠️ EMERGENCY EXIT: {pos.asset} {pos.token_side} "
                f"T+{elapsed}s (deadline {C.EXIT_DEADLINE_S}s)"
            )
            _emergency_exit(client, conn, pos, now)
            closed += 1
            conn.commit()

    return closed


def _emergency_exit(
    client: ClobClient, conn: sqlite3.Connection,
    pos: Position, now: int
) -> None:
    """
    Force-exit a position: cancel SELL, market-sell at $0.01.

    Args:
        client: ClobClient instance
        conn: SQLite connection
        pos: Position to exit
        now: current unix timestamp
    """
    # Cancel existing SELL if any
    if pos.sell_order:
        cancel_order(client, pos.sell_order)
        db.update_order_status(conn, pos.sell_order, "cancelled")

    # Get token_id
    rnd = db.get_round(conn, pos.round_ts, pos.asset)
    if not rnd:
        # Can't find round — close at total loss
        db.close_position(
            conn, pos.id,
            pnl=-(pos.entry_price * pos.entry_size),
            sell_price=0.0, closed_at=now,
        )
        return

    token_id = rnd.up_token if pos.token_side == "UP" else rnd.down_token

    # Market sell at $0.01 (accept any price)
    sell_oid = place_order(client, token_id, SELL_SIDE, 0.01, pos.entry_size)

    if sell_oid:
        db.insert_order(conn, Order(
            order_id=sell_oid, round_ts=pos.round_ts, asset=pos.asset,
            token_side=pos.token_side, order_type="SELL",
            price=0.01, size=pos.entry_size,
            status="open", placed_at=now,
        ))
        log.info(f"  Market sell placed at $0.01")

    # Close position (estimate worst case P&L)
    pnl = -(pos.entry_price * pos.entry_size)
    db.close_position(conn, pos.id, pnl, 0.01, now)

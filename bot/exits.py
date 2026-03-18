"""
Exits — manage open positions, detect SELL fills, emergency exit.
Ensures we NEVER hold to settlement.

Exit logic:
  1. Check if SELL filled → close with profit
  2. Signal-aware: if strong signal says position won't rebound → early exit
  3. Past EXIT_DEADLINE → emergency market-sell at $0.01

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
from signals import get_signal_for_position

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
        elapsed = now - pos.opened_at

        # ── Check SELL fill ──────────────────────────────────
        if pos.sell_order and pos.status == "exiting":
            info = get_order_status(client, pos.sell_order)
            if info and info["size_matched"] > 0:
                if info["status"] in ("MATCHED", "FILLED", "CLOSED"):
                    actual_price = pos.sell_price or C.SELL_TARGET
                    pnl = (actual_price - pos.entry_price) * info["size_matched"]
                    db.close_position(
                        conn, pos.id, pnl, actual_price, now
                    )
                    db.update_order_status(conn, pos.sell_order, "filled", now)
                    db.log_pnl(conn, now, "profit_exit", pnl, db.total_pnl(conn) + pnl)
                    log.info(
                        f"✅ EXIT: {pos.asset} {pos.token_side} | "
                        f"${pos.entry_price}→${actual_price} | "
                        f"P&L: ${pnl:+.2f}"
                    )
                    closed += 1
                    conn.commit()
                    continue

        # ── Signal-aware early exit (only if not already exiting) ──
        if pos.status != "exiting" and elapsed >= 30:
            signal = get_signal_for_position(pos.asset, pos.round_ts)
            if signal and signal.confidence >= 0.7:
                # Signal says direction X. If we hold the OPPOSITE side, we're in trouble.
                # e.g., we hold DOWN but signal says UP → DOWN goes to $0
                if signal.direction != pos.token_side:
                    log.warning(
                        f"⚠️ SIGNAL EXIT: {pos.asset} {pos.token_side} "
                        f"— signal says {signal.direction} ({signal.confidence:.0%})"
                    )
                    _emergency_exit(client, conn, pos, now)
                    closed += 1
                    conn.commit()
                    continue

        # ── Emergency exit at T+4min (only if not already exiting) ──
        if elapsed >= C.EXIT_DEADLINE_S and pos.status != "exiting":
            log.warning(
                f"⚠️ EMERGENCY EXIT: {pos.asset} {pos.token_side} "
                f"T+{elapsed}s (deadline {C.EXIT_DEADLINE_S}s)"
            )
            _emergency_exit(client, conn, pos, now)
            closed += 1
            conn.commit()

        # ── Force-close stale exiting positions at T+8min ────
        elif elapsed >= C.EXIT_DEADLINE_S * 2 and pos.status == "exiting":
            log.warning(
                f"⚠️ FORCE CLOSE: {pos.asset} {pos.token_side} "
                f"T+{elapsed}s — sell never filled, retrying market sell"
            )
            # Cancel the orphaned SELL order on CLOB
            if pos.sell_order:
                cancel_order(client, pos.sell_order)
                db.update_order_status(conn, pos.sell_order, "cancelled")
            # Attempt one final market sell to avoid inventory leak
            rnd = db.get_round(conn, pos.round_ts, pos.asset)
            if rnd:
                token_id = rnd.up_token if pos.token_side == "UP" else rnd.down_token
                sell_oid = place_order(
                    client, token_id, SELL_SIDE,
                    C.EMERGENCY_SELL_PRICE, pos.entry_size
                )
                if sell_oid:
                    db.insert_order(conn, Order(
                        order_id=sell_oid, round_ts=pos.round_ts, asset=pos.asset,
                        token_side=pos.token_side, order_type="SELL",
                        price=C.EMERGENCY_SELL_PRICE, size=pos.entry_size,
                        status="open", placed_at=now,
                    ))
                    db.update_position_sell(conn, pos.id, sell_oid, C.EMERGENCY_SELL_PRICE)
                    log.info(f"  Final market sell placed at ${C.EMERGENCY_SELL_PRICE}")
                    conn.commit()
                    continue  # let next tick detect the fill
            # If no round found or sell failed — book total loss
            pnl = -(pos.entry_price * pos.entry_size)
            db.close_position(conn, pos.id, pnl, 0.0, now)
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

    # Market sell at emergency price (accept any price)
    sell_oid = place_order(
        client, token_id, SELL_SIDE,
        C.EMERGENCY_SELL_PRICE, pos.entry_size
    )

    if sell_oid:
        db.insert_order(conn, Order(
            order_id=sell_oid, round_ts=pos.round_ts, asset=pos.asset,
            token_side=pos.token_side, order_type="SELL",
            price=C.EMERGENCY_SELL_PRICE, size=pos.entry_size,
            status="open", placed_at=now,
        ))
        # Keep position as 'exiting' — the sell fill check in manage_exits()
        # will close it with the actual fill price
        db.update_position_sell(conn, pos.id, sell_oid, C.EMERGENCY_SELL_PRICE)
        conn.commit()  # Persist immediately — crash safety for CLOB orders
        log.info(f"  Market sell placed at ${C.EMERGENCY_SELL_PRICE}, awaiting fill")
    else:
        # No sell placed — close at total loss
        pnl = -(pos.entry_price * pos.entry_size)
        db.close_position(conn, pos.id, pnl, 0.0, now)
        log.warning(f"  Failed to place emergency sell, closed at loss")

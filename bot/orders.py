"""
Orders — place pre-orders, detect fills, cancel orders.
Handles the BUY side of the lifecycle.

To extend: add new order types or pricing strategies here.
"""
import time
import logging
import sqlite3
from py_clob_client.client import ClobClient

import config as C
import db
from models import Order, Position
from client import place_order, cancel_order, get_order_status, BUY_SIDE, SELL_SIDE

log = logging.getLogger("bot.orders")


def _committed_capital(conn: sqlite3.Connection) -> float:
    """
    Total capital locked: open BUY orders + open positions' entry cost.
    This is what's actually committed on the CLOB.
    """
    open_order_cost = db.sum_open_order_cost(conn)
    open_position_cost = db.sum_open_position_cost(conn)
    return open_order_cost + open_position_cost


def place_preorders(client: ClobClient, conn: sqlite3.Connection) -> int:
    """
    Place BUY orders on both UP and DOWN for all 'new' rounds.
    Enforces budget by tracking committed capital (orders + positions).

    Args:
        client: ClobClient instance
        conn: SQLite connection

    Returns:
        Number of orders placed
    """
    open_count = db.count_open_orders(conn)
    if open_count >= C.MAX_OPEN_ORDERS:
        log.debug(f"Order limit reached ({open_count}/{C.MAX_OPEN_ORDERS})")
        return 0

    open_positions = db.count_open_positions(conn)
    if open_positions >= C.MAX_POSITIONS:
        log.debug(f"Position limit reached ({open_positions}/{C.MAX_POSITIONS})")
        return 0

    # Budget check: committed capital must not exceed budget
    committed = _committed_capital(conn)
    pair_cost = C.BUY_PRICE * C.BUY_SIZE * 2  # UP + DOWN
    if committed + pair_cost > C.BUDGET_TOTAL:
        log.debug(f"Budget limit: ${committed:.2f} committed, ${C.BUDGET_TOTAL} max")
        return 0

    rounds = db.get_rounds_by_status(conn, "new")
    placed = 0

    for rnd in rounds:
        # Check both order limit AND budget before each pair
        if open_count + placed >= C.MAX_OPEN_ORDERS - 1:  # -1: room for both UP+DOWN
            break
        committed = _committed_capital(conn) + placed * C.BUY_PRICE * C.BUY_SIZE
        if committed + pair_cost > C.BUDGET_TOTAL:
            break

        # Place BUY UP
        up_oid = place_order(
            client, rnd.up_token, BUY_SIDE, C.BUY_PRICE, C.BUY_SIZE
        )
        if up_oid:
            db.insert_order(conn, Order(
                order_id=up_oid, round_ts=rnd.round_ts, asset=rnd.asset,
                token_side="UP", order_type="BUY",
                price=C.BUY_PRICE, size=C.BUY_SIZE,
                status="open", placed_at=int(time.time()),
            ))
            placed += 1

        # Place BUY DOWN
        down_oid = place_order(
            client, rnd.down_token, BUY_SIDE, C.BUY_PRICE, C.BUY_SIZE
        )
        if down_oid:
            db.insert_order(conn, Order(
                order_id=down_oid, round_ts=rnd.round_ts, asset=rnd.asset,
                token_side="DOWN", order_type="BUY",
                price=C.BUY_PRICE, size=C.BUY_SIZE,
                status="open", placed_at=int(time.time()),
            ))
            placed += 1

        # Update round status
        status = "placed" if (up_oid or down_oid) else "skipped"
        db.update_round_status(conn, rnd.round_ts, rnd.asset, status)
        conn.commit()

    if placed > 0:
        log.info(f"Placed {placed} pre-orders (${_committed_capital(conn):.2f} committed)")

    return placed


def check_fills(client: ClobClient, conn: sqlite3.Connection) -> list[Order]:
    """
    Check all open BUY orders for fills.
    When filled: cancel opposite side, create position, place SELL.

    Args:
        client: ClobClient instance
        conn: SQLite connection

    Returns:
        List of filled Order objects
    """
    open_buys = db.get_open_orders(conn, order_type="BUY")
    if not open_buys:
        return []

    filled = []
    now = int(time.time())

    for order in open_buys:
        # Enforce position limit — if full, cancel remaining BUYs
        if db.count_open_positions(conn) >= C.MAX_POSITIONS:
            cancel_order(client, order.order_id)
            db.update_order_status(conn, order.order_id, "cancelled")
            continue

        # Skip if we already have a position for this round (prevent double fill)
        existing = db.get_positions_for_round(conn, order.round_ts, order.asset)
        if existing:
            # Cancel this stale BUY — the other side already filled
            cancel_order(client, order.order_id)
            db.update_order_status(conn, order.order_id, "cancelled")
            continue

        info = get_order_status(client, order.order_id)
        if not info:
            continue

        if info["size_matched"] > 0 and info["status"] in ("MATCHED", "FILLED", "CLOSED"):
            # FILLED!
            matched = info["size_matched"]
            log.info(
                f"💰 FILL: {order.asset} {order.token_side} "
                f"{matched:.1f} shares @ ${order.price}"
            )

            db.update_order_status(conn, order.order_id, "filled", filled_at=now)

            # Cancel opposite side BUY
            opp_side = "DOWN" if order.token_side == "UP" else "UP"
            opp_orders = db.get_orders_for_round(
                conn, order.round_ts, order.asset,
                token_side=opp_side, order_type="BUY", status="open"
            )
            for opp in opp_orders:
                if cancel_order(client, opp.order_id):
                    db.update_order_status(conn, opp.order_id, "cancelled")
                    log.info(f"  Cancelled opposite {opp_side} BUY")

            # Get token_id for SELL
            rnd = db.get_round(conn, order.round_ts, order.asset)
            if not rnd:
                continue
            token_id = rnd.up_token if order.token_side == "UP" else rnd.down_token

            # Place SELL at target
            sell_oid = place_order(
                client, token_id, SELL_SIDE, C.SELL_TARGET, matched
            )

            # Create position
            pos = Position(
                id=0, round_ts=order.round_ts, asset=order.asset,
                token_side=order.token_side,
                entry_price=order.price, entry_size=matched,
                entry_order=order.order_id,
                sell_order=sell_oid, sell_price=C.SELL_TARGET if sell_oid else None,
                status="exiting" if sell_oid else "open",
                opened_at=now,
            )
            pos_id = db.insert_position(conn, pos)

            if sell_oid:
                db.insert_order(conn, Order(
                    order_id=sell_oid, round_ts=order.round_ts, asset=order.asset,
                    token_side=order.token_side, order_type="SELL",
                    price=C.SELL_TARGET, size=matched,
                    status="open", placed_at=now,
                ))
                log.info(
                    f"  📤 SELL placed: {matched:.1f} × ${C.SELL_TARGET}"
                )

            filled.append(order)
            conn.commit()

    return filled


def cancel_all(client: ClobClient, conn: sqlite3.Connection) -> int:
    """
    Emergency: cancel ALL open orders.

    Returns:
        Number of orders cancelled
    """
    open_orders = db.get_open_orders(conn)
    count = 0
    for order in open_orders:
        cancel_order(client, order.order_id)
        db.update_order_status(conn, order.order_id, "cancelled")
        count += 1
    conn.commit()
    log.warning(f"🛑 Cancelled {count} orders")
    return count

"""
Orders — place pre-orders, detect fills, cancel orders.
Handles the BUY side of the lifecycle.

v5.1 fixes:
  - Partial fills: only mark "filled" when fully matched, else "partial"
  - Late fills: poll previous round for 30s after boundary
  - Pause-aware pre-orders: skip near-term rounds during pause
  - Lucky settlement references removed (all assets unified)

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

# Grace period for polling the previous round after boundary (seconds)
PREV_ROUND_GRACE_S = 30


def _committed_capital(conn: sqlite3.Connection) -> float:
    """
    Total capital locked: open BUY orders + open positions' entry cost.
    This is what's actually committed on the CLOB.
    """
    open_order_cost = db.sum_open_order_cost(conn)
    open_position_cost = db.sum_open_position_cost(conn)
    return open_order_cost + open_position_cost


def place_preorders(
    client: ClobClient, conn: sqlite3.Connection,
    pause_active: bool = False, pause_round_ts: int = 0,
) -> int:
    """
    Place BUY orders on both UP and DOWN for all 'new' rounds.
    Enforces budget by tracking committed capital (orders + positions).

    During pause: skips rounds within the dead zone (next BRAKE_PAUSE_S).
    Only places orders for rounds beyond the pause window.

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
    now = int(time.time())
    current_round_ts = (now // C.ROUND_DURATION_S) * C.ROUND_DURATION_S

    for rnd in rounds:
        # During pause: skip rounds within the dead zone
        # Only place orders for rounds beyond pause_round_ts + BRAKE_PAUSE_S
        if pause_active:
            dead_zone_end = current_round_ts + C.BRAKE_PAUSE_S
            if rnd.round_ts <= dead_zone_end:
                # Mark as skipped so we don't re-check every cycle
                db.update_round_status(conn, rnd.round_ts, rnd.asset, "paused")
                continue

        # Check both order limit AND budget before each pair
        if open_count + placed >= C.MAX_OPEN_ORDERS - 1:
            break
        committed = _committed_capital(conn)
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
                status="open", placed_at=now,
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
                status="open", placed_at=now,
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
    Check open BUY orders for fills.

    Polls the currently active round PLUS the previous round for 30s after
    the boundary (catches late fills from the last seconds of the prior round).

    Returns:
        List of filled Order objects
    """
    now = int(time.time())
    current_round_ts = (now // C.ROUND_DURATION_S) * C.ROUND_DURATION_S
    prev_round_ts = current_round_ts - C.ROUND_DURATION_S
    elapsed_in_round = now - current_round_ts

    # Get orders for the current round
    open_buys = db.get_active_round_orders(conn, current_round_ts, order_type="BUY")

    # Also poll the previous round for the first 30 seconds (catch late fills)
    if elapsed_in_round <= PREV_ROUND_GRACE_S:
        prev_buys = db.get_active_round_orders(conn, prev_round_ts, order_type="BUY")
        open_buys.extend(prev_buys)

    if not open_buys:
        return []

    filled = []

    for order in open_buys:
        # Skip partial orders (already processed, waiting for remaining fill or expiry)
        if order.status == "partial":
            continue

        # Enforce position limit
        if db.count_open_positions(conn) >= C.MAX_POSITIONS:
            cancel_order(client, order.order_id)
            db.update_order_status(conn, order.order_id, "cancelled")
            conn.commit()
            continue

        # Skip if we already have a position for this round+asset (prevent double fill)
        existing = db.get_positions_for_round(conn, order.round_ts, order.asset)
        if existing:
            cancel_order(client, order.order_id)
            db.update_order_status(conn, order.order_id, "cancelled")
            conn.commit()
            continue

        info = get_order_status(client, order.order_id)
        if not info:
            continue

        if info["size_matched"] > 0 and info["status"] in ("MATCHED", "FILLED", "CLOSED"):
            matched = info["size_matched"]
            is_full_fill = matched >= order.size - 0.01  # tolerance for rounding

            log.info(
                f"💰 FILL: {order.asset} {order.token_side} "
                f"{matched:.1f}/{order.size:.0f} shares @ ${order.price}"
                f"{'' if is_full_fill else ' (PARTIAL)'}"
            )

            # Mark order status based on fill completeness
            if is_full_fill:
                db.update_order_status(conn, order.order_id, "filled", filled_at=now)
            else:
                # Partial fill: mark as "partial" so we don't re-process,
                # but the remaining shares stay on the exchange.
                # We cancel the remaining since we already have exposure.
                cancel_order(client, order.order_id)
                db.update_order_status(conn, order.order_id, "partial", filled_at=now)

            # Cancel opposite side BUY (we have exposure, no need for both sides)
            opp_side = "DOWN" if order.token_side == "UP" else "UP"
            opp_orders = db.get_orders_for_round(
                conn, order.round_ts, order.asset,
                token_side=opp_side, order_type="BUY", status="open"
            )
            for opp in opp_orders:
                if cancel_order(client, opp.order_id):
                    db.update_order_status(conn, opp.order_id, "cancelled")
                    log.info(f"  Cancelled opposite {opp_side} BUY")

            # Place SELL at target
            rnd = db.get_round(conn, order.round_ts, order.asset)
            if not rnd:
                continue
            token_id = rnd.up_token if order.token_side == "UP" else rnd.down_token

            sell_oid = place_order(
                client, token_id, SELL_SIDE, C.SELL_TARGET, matched
            )

            pos = Position(
                id=0, round_ts=order.round_ts, asset=order.asset,
                token_side=order.token_side,
                entry_price=order.price, entry_size=matched,
                entry_order=order.order_id,
                sell_order=sell_oid, sell_price=C.SELL_TARGET if sell_oid else None,
                sell_placed_at=now if sell_oid else None,
                filled_at=now,
                status="exiting" if sell_oid else "open",
                opened_at=now,
            )
            db.insert_position(conn, pos)

            if sell_oid:
                db.insert_order(conn, Order(
                    order_id=sell_oid, round_ts=order.round_ts, asset=order.asset,
                    token_side=order.token_side, order_type="SELL",
                    price=C.SELL_TARGET, size=matched,
                    status="open", placed_at=now,
                ))
                log.info(
                    f"  📤 SELL placed: {matched:.1f} x ${C.SELL_TARGET}"
                )

            filled.append(order)
            conn.commit()

    return filled


def cancel_all(client: ClobClient, conn: sqlite3.Connection) -> int:
    """Cancel ALL open orders. Returns count."""
    open_orders = db.get_open_orders(conn)
    count = 0
    for order in open_orders:
        if cancel_order(client, order.order_id):
            db.update_order_status(conn, order.order_id, "cancelled")
            count += 1
        else:
            log.warning(f"Failed to cancel {order.order_id[:16]}...")
    conn.commit()
    log.warning(f"🛑 Cancelled {count} orders")
    return count


def cancel_inactive_buys(client: ClobClient, conn: sqlite3.Connection) -> int:
    """Cancel BUY orders for non-active rounds only."""
    now = int(time.time())
    current_round_ts = (now // C.ROUND_DURATION_S) * C.ROUND_DURATION_S
    inactive_orders = db.get_inactive_buy_orders(conn, current_round_ts)
    count = 0
    for order in inactive_orders:
        if cancel_order(client, order.order_id):
            db.update_order_status(conn, order.order_id, "cancelled")
            count += 1
    conn.commit()
    if count:
        log.warning(f"🚨 Cancelled {count} inactive BUY orders (kept active round)")
    return count


def cancel_near_term_buys(client: ClobClient, conn: sqlite3.Connection) -> int:
    """
    PAUSE MODE: Cancel BUY orders for rounds within the next BRAKE_PAUSE_S.
    Keeps active round alive. Keeps far-future orders alive.
    """
    now = int(time.time())
    current_round_ts = (now // C.ROUND_DURATION_S) * C.ROUND_DURATION_S
    near_orders = db.get_near_term_buy_orders(conn, current_round_ts, C.BRAKE_PAUSE_S)
    count = 0
    for order in near_orders:
        if cancel_order(client, order.order_id):
            db.update_order_status(conn, order.order_id, "cancelled")
            count += 1
    conn.commit()
    if count:
        log.warning(
            f"⏸️ PAUSE: Cancelled {count} BUY orders within next "
            f"{C.BRAKE_PAUSE_S // 60} min (kept active + far-future)"
        )
    return count

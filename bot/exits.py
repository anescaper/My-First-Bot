"""
Exits — manage open positions, detect SELL fills, step-down, emergency exit.

Exit logic (unified for all assets, v5.1):
    1. Check if SELL filled -> close with profit
    2. Step-down sell: when < SELL_STEPDOWN_S left, hit best bid (ONE SHOT)
    3. Emergency market-sell: when < EXIT_DEADLINE_S left (ONE SHOT)
    4. Round expired: cancel sell, book loss

Status flow:
    open -> exiting (sell placed at target)
    exiting -> stepdown (step-down sell placed, one-shot)
    exiting/stepdown -> emergency (emergency sell placed, one-shot)
    any -> closed (sell filled or round expired)

Failure definition: sell_revenue < buy_cost ($0.27 x 19 = $5.13)
If 2+ failures across current + previous round -> emergency pause in main.py

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
    cancel_order, get_order_status, place_order, get_best_bid, SELL_SIDE
)

log = logging.getLogger("bot.exits")


def manage_exits(client: ClobClient, conn: sqlite3.Connection) -> int:
    """
    For each open position, apply the appropriate exit strategy.
    Uses time-to-round-end (not elapsed since fill) for all deadlines.

    Returns:
        Number of positions closed
    """
    now = int(time.time())
    closed = 0

    positions = db.get_open_positions(conn)
    for pos in positions:
        round_end = pos.round_ts + C.ROUND_DURATION_S
        time_left = round_end - now  # seconds until round settles
        round_ended = time_left <= 0

        # ── Check SELL fill (any status with a sell order) ─────
        if pos.sell_order and pos.status in ("exiting", "stepdown", "emergency"):
            info = get_order_status(client, pos.sell_order)
            if info and info["size_matched"] > 0:
                if info["status"] in ("MATCHED", "FILLED", "CLOSED"):
                    # Use matched amount to derive actual fill price
                    # size_matched * actual_price = total_revenue
                    # We approximate: the order was placed at pos.sell_price
                    # For accurate P&L, use the fill price from the exchange
                    actual_price = pos.sell_price or C.SELL_TARGET
                    pnl = (actual_price - pos.entry_price) * info["size_matched"]
                    db.close_position(
                        conn, pos.id, pnl, actual_price, now
                    )
                    db.update_order_status(conn, pos.sell_order, "filled", now)
                    db.log_pnl(conn, now, "profit_exit", pnl, db.total_pnl(conn) + pnl)
                    log.info(
                        f"{'✅' if pnl >= 0 else '❌'} EXIT: {pos.asset} {pos.token_side} | "
                        f"${pos.entry_price}->${actual_price} | "
                        f"P&L: ${pnl:+.2f}"
                    )
                    closed += 1
                    conn.commit()
                    continue

        # ── Round settled ────────────────────────────────────
        if round_ended:
            if pos.sell_order:
                cancel_order(client, pos.sell_order)
                db.update_order_status(conn, pos.sell_order, "cancelled")
            pnl = -(pos.entry_price * pos.entry_size)
            db.close_position(conn, pos.id, pnl, 0.0, now)
            db.log_pnl(conn, now, "round_expired", pnl, db.total_pnl(conn) + pnl)
            log.warning(
                f"⏰ ROUND EXPIRED: {pos.asset} {pos.token_side} | "
                f"bought ${pos.entry_price} x {pos.entry_size} | "
                f"P&L: ${pnl:+.2f} (sell never filled)"
            )
            closed += 1
            conn.commit()
            continue

        # ── Step-down sell: ONE SHOT when < SELL_STEPDOWN_S left ──
        # Only fires for "exiting" status. After step-down, status becomes
        # "stepdown" so this block never re-triggers.
        if pos.status == "exiting" and time_left <= C.SELL_STEPDOWN_S:
            rnd = db.get_round(conn, pos.round_ts, pos.asset)
            if rnd:
                token_id = rnd.up_token if pos.token_side == "UP" else rnd.down_token
                best_bid = get_best_bid(client, token_id)
                log.info(
                    f"🔍 STEP-DOWN: {pos.asset} {pos.token_side} | "
                    f"{time_left}s left | best_bid=${best_bid:.3f} | min=${C.MIN_SELL_PRICE}"
                )
                if best_bid >= C.MIN_SELL_PRICE:
                    if pos.sell_order:
                        cancel_order(client, pos.sell_order)
                        db.update_order_status(conn, pos.sell_order, "cancelled")
                    sell_oid = place_order(
                        client, token_id, SELL_SIDE, best_bid, pos.entry_size
                    )
                    if sell_oid:
                        db.insert_order(conn, Order(
                            order_id=sell_oid, round_ts=pos.round_ts, asset=pos.asset,
                            token_side=pos.token_side, order_type="SELL",
                            price=best_bid, size=pos.entry_size,
                            status="open", placed_at=now,
                        ))
                        db.update_position_sell(conn, pos.id, sell_oid, best_bid)
                        # Mark as stepdown so this block does not re-trigger
                        db.update_position_status(conn, pos.id, "stepdown")
                        pnl_est = (best_bid - pos.entry_price) * pos.entry_size
                        log.info(
                            f"📉 STEP-DOWN SELL: {pos.asset} {pos.token_side} | "
                            f"bid ${best_bid:.2f} (est P&L: ${pnl_est:+.2f})"
                        )
                        conn.commit()
                        continue
                    else:
                        log.warning(f"  Step-down place_order failed for {pos.asset}")

        # ── Emergency exit: ONE SHOT when < EXIT_DEADLINE_S left ──
        # Fires for "exiting" or "stepdown" status. After emergency sell,
        # status becomes "emergency" so this block never re-triggers.
        if time_left <= C.EXIT_DEADLINE_S and pos.status in ("exiting", "stepdown"):
            log.warning(
                f"⚠️ EMERGENCY EXIT: {pos.asset} {pos.token_side} | "
                f"{time_left}s left — market selling at ${C.EMERGENCY_SELL_PRICE}"
            )
            _emergency_exit(client, conn, pos, now)
            conn.commit()
            continue

        # ── Open position without sell order — emergency ──
        if time_left <= C.EXIT_DEADLINE_S and pos.status == "open":
            log.warning(
                f"⚠️ NO SELL ORDER: {pos.asset} {pos.token_side} | "
                f"{time_left}s left — emergency market sell"
            )
            _emergency_exit(client, conn, pos, now)
            conn.commit()
            continue

    return closed


def _emergency_exit(
    client: ClobClient, conn: sqlite3.Connection,
    pos: Position, now: int
) -> None:
    """
    Force-exit a position: cancel SELL, market-sell at $0.01.
    Sets status to "emergency" so this only fires ONCE.
    """
    if pos.sell_order:
        cancel_order(client, pos.sell_order)
        db.update_order_status(conn, pos.sell_order, "cancelled")

    rnd = db.get_round(conn, pos.round_ts, pos.asset)
    if not rnd:
        db.close_position(
            conn, pos.id,
            pnl=-(pos.entry_price * pos.entry_size),
            sell_price=0.0, closed_at=now,
        )
        return

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
        # Mark as emergency so this block does not re-trigger
        db.update_position_status(conn, pos.id, "emergency")
        log.info(f"  Market sell placed at ${C.EMERGENCY_SELL_PRICE}, awaiting fill")
    else:
        pnl = -(pos.entry_price * pos.entry_size)
        db.close_position(conn, pos.id, pnl, 0.0, now)
        log.warning(f"  Failed to place emergency sell, closed at loss ${pnl:+.2f}")

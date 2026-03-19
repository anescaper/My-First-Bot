"""
Exits — manage open positions, detect SELL fills, step-down, emergency exit.

Exit logic (unified for all assets, v5):
    1. Check if SELL filled → close with profit
    2. Step-down sell: when < SELL_STEPDOWN_S left, hit best bid if >= MIN_SELL_PRICE
    3. Emergency market-sell: when < EXIT_DEADLINE_S left
    4. Round expired: cancel sell, book loss

Failure definition: sell_revenue < buy_cost ($0.27 × 19 = $5.13)
If 2+ failures across current + previous round → emergency pause in main.py

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
from signals import get_signal_for_position

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
        elapsed = now - pos.opened_at
        round_ended = time_left <= 0
        is_lucky = pos.asset in C.LUCKY_SETTLEMENT

        # ── Check SELL fill (all assets) ─────────────────────
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

        # ── Round settled ────────────────────────────────────
        if round_ended:
            if is_lucky:
                # BTC: hold to settlement — cancel sell, let Polymarket settle
                if pos.sell_order:
                    cancel_order(client, pos.sell_order)
                    db.update_order_status(conn, pos.sell_order, "cancelled")
                pnl = -(pos.entry_price * pos.entry_size)  # worst case
                db.close_position(conn, pos.id, pnl, 0.0, now)
                db.log_pnl(conn, now, "settlement", pnl, db.total_pnl(conn) + pnl)
                log.info(
                    f"🍀 SETTLEMENT: {pos.asset} {pos.token_side} | "
                    f"bought ${pos.entry_price} × {pos.entry_size} | "
                    f"awaiting payout (booked ${pnl:+.2f} pending)"
                )
            else:
                # ETH/SOL/XRP: round expired without sell fill — loss
                if pos.sell_order:
                    cancel_order(client, pos.sell_order)
                    db.update_order_status(conn, pos.sell_order, "cancelled")
                pnl = -(pos.entry_price * pos.entry_size)
                db.close_position(conn, pos.id, pnl, 0.0, now)
                db.log_pnl(conn, now, "round_expired", pnl, db.total_pnl(conn) + pnl)
                log.warning(
                    f"⏰ ROUND EXPIRED: {pos.asset} {pos.token_side} | "
                    f"bought ${pos.entry_price} × {pos.entry_size} | "
                    f"P&L: ${pnl:+.2f} (sell never filled)"
                )
            closed += 1
            conn.commit()
            continue

        # ── Below here: only non-lucky assets do step-down/emergency ──

        if is_lucky:
            # BTC just waits — either sell fills or we go to settlement
            continue

        # ── Step-down sell: when < SELL_STEPDOWN_S left before round end ──
        # e.g. SELL_STEPDOWN_S=45 → step-down when < 45s left (after T+4:15)
        if pos.status == "exiting" and time_left <= C.SELL_STEPDOWN_S:
            rnd = db.get_round(conn, pos.round_ts, pos.asset)
            if rnd:
                token_id = rnd.up_token if pos.token_side == "UP" else rnd.down_token
                best_bid = get_best_bid(client, token_id)
                log.info(
                    f"🔍 STEP-DOWN CHECK: {pos.asset} {pos.token_side} | "
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
                        pnl_est = (best_bid - pos.entry_price) * pos.entry_size
                        log.info(
                            f"📉 STEP-DOWN SELL: {pos.asset} {pos.token_side} | "
                            f"bid ${best_bid:.2f} (est P&L: ${pnl_est:+.2f})"
                        )
                        conn.commit()
                        continue
                    else:
                        log.warning(f"  Step-down place_order failed for {pos.asset}")
            else:
                log.warning(f"  Step-down: round not found for {pos.asset} {pos.round_ts}")

        # ── Emergency exit: when < EXIT_DEADLINE_S left (T+3:30 = 90s left) ──
        if time_left <= C.EXIT_DEADLINE_S and pos.status == "exiting":
            log.warning(
                f"⚠️ EMERGENCY EXIT: {pos.asset} {pos.token_side} | "
                f"{time_left}s left — market selling"
            )
            _emergency_exit(client, conn, pos, now)
            closed += 1
            conn.commit()
            continue

        # ── Open position without sell order — place emergency sell ──
        if time_left <= C.EXIT_DEADLINE_S and pos.status == "open":
            log.warning(
                f"⚠️ NO SELL ORDER: {pos.asset} {pos.token_side} | "
                f"{time_left}s left — emergency market sell"
            )
            _emergency_exit(client, conn, pos, now)
            closed += 1
            conn.commit()
            continue

    return closed


def _emergency_exit(
    client: ClobClient, conn: sqlite3.Connection,
    pos: Position, now: int
) -> None:
    """
    Force-exit a position: cancel SELL, market-sell at $0.01.
    Only used for non-lucky-settlement assets.
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
        conn.commit()
        log.info(f"  Market sell placed at ${C.EMERGENCY_SELL_PRICE}, awaiting fill")
    else:
        pnl = -(pos.entry_price * pos.entry_size)
        db.close_position(conn, pos.id, pnl, 0.0, now)
        log.warning(f"  Failed to place emergency sell, closed at loss")

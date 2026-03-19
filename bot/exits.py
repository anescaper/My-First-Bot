"""
Exits — manage open positions with 4-tier waterfall exit.

Exit waterfall (v5.2):
    Tier 1: SELL at $0.48 immediately on fill (dream price)
    Tier 2: After 30s unfilled, cancel $0.48 → SELL at $0.35 (realistic)
    Tier 3: When < 120s left, cancel → SELL at best bid (step-down, one shot)
    Tier 4: When < 90s left, cancel → SELL at $0.15 (stop-loss, one shot)
    Round expired: cancel sell, book loss

Status flow:
    open -> exiting     (tier 1: sell at $0.48)
    exiting -> fallback (tier 2: sell at $0.35 after 30s)
    exiting/fallback -> stepdown  (tier 3: sell at best bid, one shot)
    exiting/fallback/stepdown -> emergency (tier 4: sell at $0.15, one shot)
    any -> closed       (sell filled or round expired)

Failure definition: sell_revenue < buy_cost ($0.27 x 19 = $5.13)
If 2+ failures across current + previous round -> emergency pause in main.py

To extend: add new exit tiers here.
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
    For each open position, apply the waterfall exit strategy.
    Uses time-to-round-end for all deadlines.

    Returns:
        Number of positions closed
    """
    now = int(time.time())
    closed = 0

    positions = db.get_open_positions(conn)
    for pos in positions:
        round_end = pos.round_ts + C.ROUND_DURATION_S
        time_left = round_end - now
        round_ended = time_left <= 0

        # ── Check SELL fill (any status with a sell order) ─────
        if pos.sell_order and pos.status in ("exiting", "fallback", "stepdown", "emergency"):
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

        # ── Tier 2: Fallback $0.48 → $0.35 after 30s ────────
        # Only fires for "exiting" status (tier 1 sell at $0.48).
        # If 30s have passed since sell was placed and it hasn't filled,
        # cancel and replace with $0.35.
        # Guard: don't fire within 5s of tier 3 boundary to prevent race.
        if pos.status == "exiting" and time_left > (C.SELL_STEPDOWN_S + 5):
            sell_age = now - (pos.sell_placed_at or pos.filled_at or now)
            if sell_age >= C.SELL_FALLBACK_S:
                rnd = db.get_round(conn, pos.round_ts, pos.asset)
                if rnd:
                    token_id = rnd.up_token if pos.token_side == "UP" else rnd.down_token
                    log.info(
                        f"📉 TIER 2 FALLBACK: {pos.asset} {pos.token_side} | "
                        f"$0.48 unfilled after {sell_age}s → dropping to ${C.SELL_FALLBACK}"
                    )
                    if pos.sell_order:
                        cancel_order(client, pos.sell_order)
                        db.update_order_status(conn, pos.sell_order, "cancelled")
                    sell_oid = place_order(
                        client, token_id, SELL_SIDE, C.SELL_FALLBACK, pos.entry_size
                    )
                    if sell_oid:
                        db.insert_order(conn, Order(
                            order_id=sell_oid, round_ts=pos.round_ts, asset=pos.asset,
                            token_side=pos.token_side, order_type="SELL",
                            price=C.SELL_FALLBACK, size=pos.entry_size,
                            status="open", placed_at=now,
                        ))
                        db.update_position_sell(conn, pos.id, sell_oid, C.SELL_FALLBACK)
                        db.update_position_status(conn, pos.id, "fallback")
                        pnl_est = (C.SELL_FALLBACK - pos.entry_price) * pos.entry_size
                        log.info(
                            f"  Fallback sell placed at ${C.SELL_FALLBACK} "
                            f"(est P&L: ${pnl_est:+.2f})"
                        )
                        conn.commit()
                        continue
                    else:
                        log.warning(f"  Fallback place_order failed for {pos.asset}")

        # ── Tier 3: Step-down to best bid when < 120s left ───
        # ONE SHOT: fires for "exiting" or "fallback" status.
        if pos.status in ("exiting", "fallback") and time_left <= C.SELL_STEPDOWN_S:
            rnd = db.get_round(conn, pos.round_ts, pos.asset)
            if rnd:
                token_id = rnd.up_token if pos.token_side == "UP" else rnd.down_token
                best_bid = get_best_bid(client, token_id)
                log.info(
                    f"🔍 TIER 3 STEP-DOWN: {pos.asset} {pos.token_side} | "
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
                        db.update_position_status(conn, pos.id, "stepdown")
                        pnl_est = (best_bid - pos.entry_price) * pos.entry_size
                        log.info(
                            f"  Step-down sell at ${best_bid:.2f} "
                            f"(est P&L: ${pnl_est:+.2f})"
                        )
                        conn.commit()
                        continue
                    else:
                        log.warning(f"  Step-down place_order failed for {pos.asset}")

        # ── Tier 4: Stop-loss at $0.15 when < 90s left ───────
        # ONE SHOT: fires for "exiting", "fallback", or "stepdown".
        if time_left <= C.EXIT_DEADLINE_S and pos.status in ("exiting", "fallback", "stepdown"):
            log.warning(
                f"⚠️ TIER 4 STOP-LOSS: {pos.asset} {pos.token_side} | "
                f"{time_left}s left — selling at ${C.EMERGENCY_SELL_PRICE}"
            )
            _stop_loss_exit(client, conn, pos, now)
            conn.commit()
            continue

        # ── Open position without sell order — emergency ──
        if time_left <= C.EXIT_DEADLINE_S and pos.status == "open":
            log.warning(
                f"⚠️ NO SELL ORDER: {pos.asset} {pos.token_side} | "
                f"{time_left}s left — stop-loss sell"
            )
            _stop_loss_exit(client, conn, pos, now)
            conn.commit()
            continue

    return closed


def _stop_loss_exit(
    client: ClobClient, conn: sqlite3.Connection,
    pos: Position, now: int
) -> None:
    """
    Force-exit a position at stop-loss price ($0.15).
    Sets status to "emergency" so this only fires ONCE.
    """
    if pos.sell_order:
        cancel_order(client, pos.sell_order)
        db.update_order_status(conn, pos.sell_order, "cancelled")

    rnd = db.get_round(conn, pos.round_ts, pos.asset)
    if not rnd:
        pnl = -(pos.entry_price * pos.entry_size)
        db.close_position(conn, pos.id, pnl, 0.0, now)
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
        db.update_position_status(conn, pos.id, "emergency")
        log.info(f"  Stop-loss sell placed at ${C.EMERGENCY_SELL_PRICE}")
    else:
        pnl = -(pos.entry_price * pos.entry_size)
        db.close_position(conn, pos.id, pnl, 0.0, now)
        log.warning(f"  Failed to place stop-loss sell, closed at loss ${pnl:+.2f}")

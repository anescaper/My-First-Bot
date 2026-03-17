"""
Signals — directional signal detection.
Reads competitor_tracker.db for mass cancellation events.

To extend: add new signal sources (e.g., Brownian Bridge)
by adding a new function and calling it from process_signals().
"""
import time
import logging
import sqlite3
from py_clob_client.client import ClobClient

import config as C
import db
from models import Signal
from client import cancel_order

log = logging.getLogger("bot.signals")


def read_competitor_signal(asset: str, round_ts: int) -> Signal | None:
    """
    Read competitor_tracker.db for cancellation events on this round.
    If competitors cancel more on one side, that's the directional signal.

    Args:
        asset: 'btc', 'eth', 'sol', 'xrp'
        round_ts: unix timestamp of round start

    Returns:
        Signal with direction to KEEP, or None if no clear signal
    """
    try:
        cdb = sqlite3.connect(C.COMPETITOR_DB_PATH, timeout=2)
        rows = cdb.execute(
            """SELECT side_cancelled, pct_cancelled, implied_direction
               FROM cancellation_events
               WHERE asset=? AND round_ts=?
               AND pct_cancelled > 30
               ORDER BY detected_at DESC LIMIT 10""",
            (asset, round_ts)
        ).fetchall()
        cdb.close()

        if not rows:
            return None

        # Vote counting: which direction do most events imply?
        up_votes = sum(1 for r in rows if r[2] == "UP")
        down_votes = sum(1 for r in rows if r[2] == "DOWN")

        if up_votes > down_votes and up_votes >= 2:
            confidence = up_votes / len(rows)
            return Signal(asset, round_ts, "UP", "competitor", confidence)
        elif down_votes > up_votes and down_votes >= 2:
            confidence = down_votes / len(rows)
            return Signal(asset, round_ts, "DOWN", "competitor", confidence)

        return None

    except Exception as e:
        log.debug(f"Competitor signal error: {e}")
        return None


def process_signals(client: ClobClient, conn: sqlite3.Connection) -> int:
    """
    For rounds in the signal window (T-120s to T-30s),
    check for directional signals and cancel the wrong side.

    Args:
        client: ClobClient instance
        conn: SQLite connection

    Returns:
        Number of signals acted on
    """
    now = int(time.time())
    acted = 0

    # Check rounds that are 'placed' (have open orders)
    rounds = db.get_rounds_by_status(conn, "placed")

    for rnd in rounds:
        secs_to_round = rnd.round_ts - now

        # Only act in the signal window
        if not (C.SIGNAL_WINDOW_END_S <= secs_to_round <= C.SIGNAL_WINDOW_START_S):
            continue

        # Get competitor signal
        signal = read_competitor_signal(rnd.asset, rnd.round_ts)
        if not signal:
            continue

        # Cancel the WRONG side (opposite of signal direction)
        cancel_side = "DOWN" if signal.direction == "UP" else "UP"

        orders_to_cancel = db.get_orders_for_round(
            conn, rnd.round_ts, rnd.asset,
            token_side=cancel_side, order_type="BUY", status="open"
        )

        for order in orders_to_cancel:
            if cancel_order(client, order.order_id):
                db.update_order_status(conn, order.order_id, "cancelled")
                log.info(
                    f"🎯 SIGNAL {signal.direction} ({signal.source}, "
                    f"{signal.confidence:.0%}) → cancelled {cancel_side} "
                    f"for {rnd.asset} T{secs_to_round:+d}s"
                )

        db.update_round_status(conn, rnd.round_ts, rnd.asset, "signaled")
        conn.commit()
        acted += 1

    return acted

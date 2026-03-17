"""
Cleanup — cancel stale orders, mark settled rounds.

To extend: add new cleanup rules here.
"""
import time
import logging
import sqlite3
from py_clob_client.client import ClobClient

import config as C
import db
from client import cancel_order

log = logging.getLogger("bot.cleanup")


def cleanup_old_rounds(client: ClobClient, conn: sqlite3.Connection) -> int:
    """
    Cancel orders and mark rounds that are past settlement (T+10min).

    Args:
        client: ClobClient instance
        conn: SQLite connection

    Returns:
        Number of rounds cleaned up
    """
    now = int(time.time())
    cutoff = now - 600  # rounds that ended > 10 min ago

    # Find stale rounds
    rows = conn.execute(
        """SELECT round_ts, asset FROM rounds
           WHERE status NOT IN ('settled', 'skipped')
           AND round_ts < ?""",
        (cutoff,)
    ).fetchall()

    cleaned = 0
    for row in rows:
        round_ts, asset = row[0], row[1]

        # Cancel any remaining open orders
        stale_orders = db.get_orders_for_round(
            conn, round_ts, asset, status="open"
        )
        for order in stale_orders:
            cancel_order(client, order.order_id)
            db.update_order_status(conn, order.order_id, "cancelled")

        db.update_round_status(conn, round_ts, asset, "settled")
        cleaned += 1

    if cleaned > 0:
        conn.commit()
        log.info(f"Cleaned up {cleaned} old rounds")

    return cleaned

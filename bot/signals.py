"""
Signals — directional signal detection from competitor tracking.

In SYMMETRIC VOL HARVEST mode, signals do NOT cancel pre-fill BUY orders.
Both UP and DOWN buys stay active — we need whichever side fills.

Signals are used for:
  1. Logging — track competitor behavior for analysis
  2. Exit management — if we have a position and signal says it won't rebound,
     trigger early exit (handled in exits.py)

To extend: add new signal sources (e.g., Brownian Bridge).
"""
import time
import logging
import sqlite3

import config as C
import db
from models import Signal

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


def get_signal_for_position(asset: str, round_ts: int) -> Signal | None:
    """
    Get signal relevant to an open position.
    Used by exits.py to decide if early exit is warranted.

    Returns:
        Signal if strong directional evidence exists, None otherwise
    """
    return read_competitor_signal(asset, round_ts)


def process_signals(conn: sqlite3.Connection) -> int:
    """
    Log competitor signals for monitoring. Does NOT cancel BUY orders.

    In symmetric vol harvest, both sides stay active until one fills.
    Signals are informational only at the pre-fill stage.

    Args:
        client: ClobClient instance
        conn: SQLite connection

    Returns:
        Number of signals detected (logged only, no action taken)
    """
    now = int(time.time())
    detected = 0

    # Check rounds in the critical window
    rounds = db.get_rounds_by_status(conn, "placed")

    for rnd in rounds:
        secs_to_round = rnd.round_ts - now

        # Only check near round start
        if not (30 <= secs_to_round <= 120):
            continue

        signal = read_competitor_signal(rnd.asset, rnd.round_ts)
        if signal:
            log.info(
                f"📡 SIGNAL {signal.direction} ({signal.source}, "
                f"{signal.confidence:.0%}) for {rnd.asset} T{secs_to_round:+d}s "
                f"[info only — no cancellation in vol harvest mode]"
            )
            detected += 1

    return detected

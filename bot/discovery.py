"""
Discovery — find upcoming 5m rounds via Gamma API.
Stores new rounds in the database.

To extend: add new assets to config.ASSETS, add new timeframes to config.
"""
import json
import time
import logging
import requests
import sqlite3

import config as C
from types import Round
import db

log = logging.getLogger("bot.discovery")


def fetch_market(asset: str, round_ts: int) -> Round | None:
    """
    Look up a single market on Gamma API by slug.

    Args:
        asset: 'btc', 'eth', 'sol', or 'xrp'
        round_ts: unix timestamp of round start

    Returns:
        Round dataclass, or None if not found
    """
    slug = f"{asset}-updown-{C.TIMEFRAME}-{round_ts}"
    try:
        resp = requests.get(
            f"{C.GAMMA_HOST}/markets",
            params={"slug": slug},
            timeout=5,
        )
        markets = resp.json()
        if not markets:
            return None

        m = markets[0]
        tokens = json.loads(m["clobTokenIds"])
        return Round(
            round_ts=round_ts,
            asset=asset,
            condition_id=m["conditionId"],
            up_token=tokens[0],
            down_token=tokens[1],
            status="new",
        )
    except Exception as e:
        log.debug(f"fetch_market {asset} {round_ts}: {e}")
        return None


def discover_rounds(conn: sqlite3.Connection) -> int:
    """
    Discover all upcoming rounds for the next LOOKAHEAD_HOURS.
    Inserts new rounds into DB.

    Args:
        conn: SQLite connection

    Returns:
        Number of new rounds discovered
    """
    now = int(time.time())
    current_5m = (now // C.ROUND_DURATION_S) * C.ROUND_DURATION_S
    new_count = 0

    # Generate all future round timestamps within lookahead
    max_rounds = C.LOOKAHEAD_HOURS * (3600 // C.ROUND_DURATION_S)

    for i in range(1, max_rounds + 1):
        ts = current_5m + i * C.ROUND_DURATION_S

        # Skip rounds starting in < 5 min (too late to pre-order)
        if ts - now < C.ROUND_DURATION_S:
            continue

        for asset in C.ASSETS:
            # Skip if already tracked
            if db.round_exists(conn, ts, asset):
                continue

            rnd = fetch_market(asset, ts)
            if rnd:
                db.insert_round(conn, rnd)
                new_count += 1
                time.sleep(C.API_DELAY_S)

    conn.commit()

    if new_count > 0:
        log.info(f"Discovered {new_count} new rounds")

    return new_count

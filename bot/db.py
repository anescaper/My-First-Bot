"""
Database — SQLite schema, read/write helpers.
All DB access goes through this module. No raw SQL elsewhere.

To extend: add new tables in init_db(), add new query functions below.
"""
import sqlite3
from typing import Optional
from models import Round, Order, Position

import config as C


def init_db() -> sqlite3.Connection:
    """Create database and tables. Returns connection."""
    conn = sqlite3.connect(C.BOT_DB_PATH)
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA busy_timeout=5000")

    conn.executescript("""
        CREATE TABLE IF NOT EXISTS rounds (
            round_ts    INTEGER NOT NULL,
            asset       TEXT    NOT NULL,
            condition_id TEXT,
            up_token    TEXT,
            down_token  TEXT,
            status      TEXT    DEFAULT 'new',
            PRIMARY KEY (round_ts, asset)
        );
        CREATE INDEX IF NOT EXISTS idx_rounds_status
            ON rounds(status);
        CREATE INDEX IF NOT EXISTS idx_rounds_ts
            ON rounds(round_ts);

        CREATE TABLE IF NOT EXISTS orders (
            order_id    TEXT PRIMARY KEY,
            round_ts    INTEGER NOT NULL,
            asset       TEXT    NOT NULL,
            token_side  TEXT    NOT NULL,
            order_type  TEXT    NOT NULL,
            price       REAL    NOT NULL,
            size        REAL    NOT NULL,
            status      TEXT    DEFAULT 'open',
            placed_at   INTEGER,
            filled_at   INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_orders_status
            ON orders(status);
        CREATE INDEX IF NOT EXISTS idx_orders_round
            ON orders(round_ts, asset);
        CREATE INDEX IF NOT EXISTS idx_orders_type_status
            ON orders(order_type, status);

        CREATE TABLE IF NOT EXISTS positions (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            round_ts    INTEGER NOT NULL,
            asset       TEXT    NOT NULL,
            token_side  TEXT    NOT NULL,
            entry_price REAL,
            entry_size  REAL,
            entry_order TEXT,
            sell_order  TEXT,
            sell_price  REAL,
            status      TEXT    DEFAULT 'open',
            pnl         REAL,
            opened_at   INTEGER,
            closed_at   INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_positions_status
            ON positions(status);
        CREATE INDEX IF NOT EXISTS idx_positions_round
            ON positions(round_ts, asset);

        CREATE TABLE IF NOT EXISTS pnl_log (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            ts          INTEGER NOT NULL,
            event       TEXT    NOT NULL,
            amount      REAL,
            balance     REAL
        );
        CREATE INDEX IF NOT EXISTS idx_pnl_ts
            ON pnl_log(ts);
    """)
    conn.commit()
    return conn


# ── Rounds ───────────────────────────────────────────────────

def round_exists(conn: sqlite3.Connection, round_ts: int, asset: str) -> bool:
    row = conn.execute(
        "SELECT 1 FROM rounds WHERE round_ts=? AND asset=?",
        (round_ts, asset)
    ).fetchone()
    return row is not None


def insert_round(conn: sqlite3.Connection, r: Round) -> None:
    conn.execute(
        "INSERT OR IGNORE INTO rounds VALUES (?,?,?,?,?,?)",
        (r.round_ts, r.asset, r.condition_id, r.up_token, r.down_token, r.status)
    )


def get_rounds_by_status(conn: sqlite3.Connection, status: str) -> list[Round]:
    rows = conn.execute(
        "SELECT * FROM rounds WHERE status=?", (status,)
    ).fetchall()
    return [Round(**dict(r)) for r in rows]


def update_round_status(conn: sqlite3.Connection, round_ts: int, asset: str, status: str) -> None:
    conn.execute(
        "UPDATE rounds SET status=? WHERE round_ts=? AND asset=?",
        (status, round_ts, asset)
    )


def get_round(conn: sqlite3.Connection, round_ts: int, asset: str) -> Optional[Round]:
    row = conn.execute(
        "SELECT * FROM rounds WHERE round_ts=? AND asset=?",
        (round_ts, asset)
    ).fetchone()
    return Round(**dict(row)) if row else None


# ── Orders ───────────────────────────────────────────────────

def insert_order(conn: sqlite3.Connection, o: Order) -> None:
    conn.execute(
        "INSERT INTO orders VALUES (?,?,?,?,?,?,?,?,?,?)",
        (o.order_id, o.round_ts, o.asset, o.token_side, o.order_type,
         o.price, o.size, o.status, o.placed_at, o.filled_at)
    )


def get_open_orders(conn: sqlite3.Connection, order_type: Optional[str] = None) -> list[Order]:
    if order_type:
        rows = conn.execute(
            "SELECT * FROM orders WHERE status='open' AND order_type=?",
            (order_type,)
        ).fetchall()
    else:
        rows = conn.execute(
            "SELECT * FROM orders WHERE status='open'"
        ).fetchall()
    return [Order(**dict(r)) for r in rows]


def get_active_round_orders(conn: sqlite3.Connection, current_round_ts: int,
                            order_type: Optional[str] = None) -> list[Order]:
    """Get open orders only for the currently active round (by round_ts).
    Much faster than polling all open orders — only checks orders that can
    actually fill right now."""
    if order_type:
        rows = conn.execute(
            "SELECT * FROM orders WHERE status='open' AND order_type=? AND round_ts=?",
            (order_type, current_round_ts)
        ).fetchall()
    else:
        rows = conn.execute(
            "SELECT * FROM orders WHERE status='open' AND round_ts=?",
            (current_round_ts,)
        ).fetchall()
    return [Order(**dict(r)) for r in rows]


def get_orders_for_round(
    conn: sqlite3.Connection, round_ts: int, asset: str,
    token_side: Optional[str] = None, order_type: Optional[str] = None,
    status: Optional[str] = None,
) -> list[Order]:
    query = "SELECT * FROM orders WHERE round_ts=? AND asset=?"
    params: list = [round_ts, asset]
    if token_side:
        query += " AND token_side=?"
        params.append(token_side)
    if order_type:
        query += " AND order_type=?"
        params.append(order_type)
    if status:
        query += " AND status=?"
        params.append(status)
    rows = conn.execute(query, params).fetchall()
    return [Order(**dict(r)) for r in rows]


def update_order_status(conn: sqlite3.Connection, order_id: str, status: str,
                        filled_at: Optional[int] = None) -> None:
    if filled_at is not None:
        conn.execute(
            "UPDATE orders SET status=?, filled_at=? WHERE order_id=?",
            (status, filled_at, order_id)
        )
    else:
        conn.execute(
            "UPDATE orders SET status=? WHERE order_id=?",
            (status, order_id)
        )


def count_open_orders(conn: sqlite3.Connection) -> int:
    return conn.execute(
        "SELECT COUNT(*) FROM orders WHERE status='open'"
    ).fetchone()[0]


def sum_open_order_cost(conn: sqlite3.Connection) -> float:
    """Total capital locked in open BUY orders (price × size)."""
    return conn.execute(
        "SELECT COALESCE(SUM(price * size), 0) FROM orders WHERE status='open' AND order_type='BUY'"
    ).fetchone()[0]


def sum_open_position_cost(conn: sqlite3.Connection) -> float:
    """Total capital locked in open positions (entry_price × entry_size)."""
    return conn.execute(
        "SELECT COALESCE(SUM(entry_price * entry_size), 0) FROM positions WHERE status IN ('open', 'exiting', 'holding')"
    ).fetchone()[0]


# ── Positions ────────────────────────────────────────────────

def insert_position(conn: sqlite3.Connection, p: Position) -> int:
    cur = conn.execute(
        """INSERT INTO positions
           (round_ts, asset, token_side, entry_price, entry_size,
            entry_order, sell_order, sell_price, status, opened_at)
           VALUES (?,?,?,?,?,?,?,?,?,?)""",
        (p.round_ts, p.asset, p.token_side, p.entry_price, p.entry_size,
         p.entry_order, p.sell_order, p.sell_price, p.status, p.opened_at)
    )
    return cur.lastrowid


def get_positions_for_round(conn: sqlite3.Connection, round_ts: int, asset: str) -> list[Position]:
    """Check if any position (open or closed) exists for a given round."""
    rows = conn.execute(
        "SELECT * FROM positions WHERE round_ts=? AND asset=?",
        (round_ts, asset)
    ).fetchall()
    return [Position(**dict(r)) for r in rows]


def get_open_positions(conn: sqlite3.Connection) -> list[Position]:
    rows = conn.execute(
        "SELECT * FROM positions WHERE status IN ('open', 'exiting', 'holding')"
    ).fetchall()
    return [Position(**dict(r)) for r in rows]


def close_position(conn: sqlite3.Connection, pos_id: int,
                   pnl: float, sell_price: float, closed_at: int) -> None:
    conn.execute(
        """UPDATE positions SET status='closed', pnl=?,
           sell_price=?, closed_at=? WHERE id=?""",
        (pnl, sell_price, closed_at, pos_id)
    )


def update_position_sell(conn: sqlite3.Connection, pos_id: int,
                         sell_order: str, sell_price: float) -> None:
    conn.execute(
        "UPDATE positions SET sell_order=?, sell_price=?, status='exiting' WHERE id=?",
        (sell_order, sell_price, pos_id)
    )


def today_pnl(conn: sqlite3.Connection) -> float:
    """Sum of closed position P&L since midnight UTC."""
    import time
    midnight = int(time.time()) - (int(time.time()) % 86400)
    return conn.execute(
        "SELECT COALESCE(SUM(pnl), 0) FROM positions WHERE status='closed' AND closed_at >= ?",
        (midnight,)
    ).fetchone()[0]


def total_pnl(conn: sqlite3.Connection) -> float:
    return conn.execute(
        "SELECT COALESCE(SUM(pnl), 0) FROM positions WHERE status='closed'"
    ).fetchone()[0]


def count_open_positions(conn: sqlite3.Connection) -> int:
    return conn.execute(
        "SELECT COUNT(*) FROM positions WHERE status IN ('open', 'exiting', 'holding')"
    ).fetchone()[0]


# ── P&L Log ──────────────────────────────────────────────────

def log_pnl(conn: sqlite3.Connection, ts: int, event: str,
            amount: float, balance: float) -> None:
    conn.execute(
        "INSERT INTO pnl_log (ts, event, amount, balance) VALUES (?,?,?,?)",
        (ts, event, amount, balance)
    )

"""
Client — ClobClient initialization and API wrappers.
All Polymarket API calls go through this module.

To extend: add new API methods here, keep rate limiting centralized.
"""
import time
import logging
from py_clob_client.client import ClobClient
from py_clob_client.clob_types import ApiCreds, OrderArgs, OrderType, BalanceAllowanceParams, AssetType
from py_clob_client.order_builder.constants import BUY, SELL

import os
import config as C

log = logging.getLogger("bot.client")

REQUIRED_SECRETS = [
    "polymarket_api_key",
    "polymarket_api_secret",
    "polymarket_passphrase",
    "polymarket_private_key",
    "polymarket_funder_address",
]

# Re-export constants for other modules
BUY_SIDE = BUY
SELL_SIDE = SELL


def read_secret(name: str) -> str:
    """Read a secret from the secrets directory."""
    with open(f"{C.SECRETS_DIR}/{name}") as f:
        return f.read().strip()


def create_client() -> ClobClient:
    """Initialize and return an authenticated ClobClient."""
    # Validate all secrets exist before trying to use them
    missing = [s for s in REQUIRED_SECRETS if not os.path.exists(f"{C.SECRETS_DIR}/{s}")]
    if missing:
        raise SystemExit(
            f"Missing secret files in {C.SECRETS_DIR}/:\n"
            + "\n".join(f"  - {s}" for s in missing)
            + "\nCreate these files with your Polymarket API credentials."
        )

    pk = read_secret("polymarket_private_key")
    if not pk.startswith("0x"):
        pk = "0x" + pk

    creds = ApiCreds(
        api_key=read_secret("polymarket_api_key"),
        api_secret=read_secret("polymarket_api_secret"),
        api_passphrase=read_secret("polymarket_passphrase"),
    )
    client = ClobClient(
        C.CLOB_HOST,
        key=pk,
        chain_id=C.CHAIN_ID,
        creds=creds,
        signature_type=2,
        funder=read_secret("polymarket_funder_address"),
    )
    # Approve token transfers for the exchange (needed for SELL orders)
    try:
        params = BalanceAllowanceParams(asset_type=AssetType.COLLATERAL, signature_type=2)
        client.update_balance_allowance(params)
        log.info("ClobClient initialized (allowances set)")
    except Exception as e:
        log.warning(f"update_balance_allowance failed: {e}")
        log.info("ClobClient initialized")
    return client


def _delay():
    """Rate limit: pause between API calls."""
    time.sleep(C.API_DELAY_S)


def _refresh_allowance(client: ClobClient) -> bool:
    """Refresh COLLATERAL allowance before SELL orders. Returns True on success."""
    for attempt in range(3):
        try:
            params = BalanceAllowanceParams(asset_type=AssetType.COLLATERAL, signature_type=2)
            client.update_balance_allowance(params)
            time.sleep(0.5)  # give chain time to confirm allowance
            return True
        except Exception as e:
            log.warning(f"Allowance refresh attempt {attempt+1}/3 failed: {e}")
            time.sleep(1.0)
    log.error("Allowance refresh failed after 3 attempts")
    return False


def place_order(client: ClobClient, token_id: str, side: str,
                price: float, size: float) -> str | None:
    """
    Place a GTC order. Returns order_id or None on failure.

    Args:
        client: ClobClient instance
        token_id: the token to trade
        side: BUY_SIDE or SELL_SIDE
        price: limit price (0.01 - 0.99)
        size: number of shares

    Returns:
        order_id string, or None if placement failed
    """
    max_attempts = 3 if side == SELL else 1
    for attempt in range(max_attempts):
        try:
            if side == SELL:
                if not _refresh_allowance(client):
                    continue  # retry after failed allowance

            args = OrderArgs(price=price, size=size, side=side, token_id=token_id)
            signed = client.create_order(args)
            result = client.post_order(signed, OrderType.GTC)
            _delay()

            oid = result.get("orderID") or result.get("id")
            if oid and result.get("success"):
                return oid
            else:
                err_msg = str(result)
                if "allowance" in err_msg.lower() and attempt < max_attempts - 1:
                    log.warning(f"SELL allowance error (attempt {attempt+1}/{max_attempts}), retrying...")
                    time.sleep(1.0)
                    continue
                log.warning(f"Order rejected: {result}")
                return None
        except Exception as e:
            err_str = str(e)
            if "allowance" in err_str.lower() and attempt < max_attempts - 1:
                log.warning(f"SELL allowance error (attempt {attempt+1}/{max_attempts}), retrying...")
                time.sleep(1.0)
                continue
            log.error(f"place_order failed: {e}")
            _delay()
            return None
    return None


def cancel_order(client: ClobClient, order_id: str) -> bool:
    """
    Cancel an order. Returns True if cancelled successfully.

    Args:
        order_id: Polymarket order ID to cancel

    Returns:
        True if cancelled, False if failed
    """
    try:
        result = client.cancel(order_id)
        _delay()
        cancelled = result.get("canceled", [])
        return order_id in cancelled
    except Exception as e:
        log.debug(f"cancel_order {order_id[:16]}...: {e}")
        _delay()
        return False


def get_order_status(client: ClobClient, order_id: str) -> dict | None:
    """
    Get current status of an order.

    Returns:
        dict with 'status' and 'size_matched' keys, or None on error
    """
    try:
        o = client.get_order(order_id)
        _delay()
        return {
            "status": (o.get("status") or "").upper(),
            "size_matched": float(o.get("size_matched", 0) or 0),
        }
    except Exception as e:
        log.debug(f"get_order_status {order_id[:16]}...: {e}")
        return None


def get_best_bid(client: ClobClient, token_id: str) -> float:
    """
    Get the best (highest) bid price for a token from the order book.

    Returns:
        Best bid price as float, or 0.0 if no bids / error
    """
    try:
        book = client.get_order_book(token_id)
        _delay()
        bids = book.get("bids", [])
        if bids:
            return max(float(b["price"]) for b in bids)
        return 0.0
    except Exception as e:
        log.debug(f"get_best_bid: {e}")
        _delay()
        return 0.0


def get_all_orders(client: ClobClient) -> list[dict]:
    """
    Get all open orders for this account.

    Returns:
        list of order dicts with 'id', 'status', 'size_matched'
    """
    try:
        orders = client.get_orders()
        _delay()
        if isinstance(orders, list):
            return orders
        return []
    except Exception as e:
        log.debug(f"get_all_orders: {e}")
        return []

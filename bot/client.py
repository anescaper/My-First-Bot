"""
Client — ClobClient initialization and API wrappers.
All Polymarket API calls go through this module.

To extend: add new API methods here, keep rate limiting centralized.
"""
import time
import logging
from py_clob_client.client import ClobClient
from py_clob_client.clob_types import ApiCreds, OrderArgs, OrderType
from py_clob_client.order_builder.constants import BUY, SELL

import config as C

log = logging.getLogger("bot.client")

# Re-export constants for other modules
BUY_SIDE = BUY
SELL_SIDE = SELL


def read_secret(name: str) -> str:
    """Read a secret from the secrets directory."""
    with open(f"{C.SECRETS_DIR}/{name}") as f:
        return f.read().strip()


def create_client() -> ClobClient:
    """Initialize and return an authenticated ClobClient."""
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
    log.info("ClobClient initialized")
    return client


def _delay():
    """Rate limit: pause between API calls."""
    time.sleep(C.API_DELAY_S)


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
    try:
        args = OrderArgs(price=price, size=size, side=side, token_id=token_id)
        signed = client.create_order(args)
        result = client.post_order(signed, OrderType.GTC)
        _delay()

        oid = result.get("orderID") or result.get("id")
        if oid and result.get("success"):
            return oid
        else:
            log.warning(f"Order rejected: {result}")
            return None
    except Exception as e:
        log.error(f"place_order failed: {e}")
        _delay()
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

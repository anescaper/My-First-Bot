"""
Phase 0: Polymarket data feasibility tests.

Tests:
  0.3 — Discover active crypto rounds via Gamma /markets API
  0.5 — CLOB WebSocket (book/price_change/last_trade_price events)
  0.7 — Spread viability: p_up + p_down < 0.97 for bilateral MM
  REST — Midpoint endpoint format verification

Findings from exploration:
  - Gamma /events?tag=crypto-prices returns stale/mislabeled data
  - Must use /markets endpoint, scan by volume, filter by keywords
  - Crypto Up/Down rounds + price threshold markets both exist
  - Many resolved markets remain "active" — filter by price != 0/1
"""

import asyncio
import json
import time
from collections import defaultdict
from datetime import datetime, timezone

import aiohttp
import websockets

GAMMA_MARKETS_URL = "https://gamma-api.polymarket.com/markets"
CLOB_WS_URL = "wss://ws-subscriptions-clob.polymarket.com/ws/market"
CLOB_REST_URL = "https://clob.polymarket.com"
WS_LISTEN_SECONDS = 30
PING_INTERVAL = 10

CRYPTO_KEYWORDS = [
    "bitcoin", "btc", "ethereum", "eth ", "solana", "sol ",
    "xrp", "crypto", "dogecoin", "doge",
]


def ts() -> str:
    return datetime.now(timezone.utc).strftime("%H:%M:%S")


def parse_json_field(val):
    """Parse a field that may be a JSON string or already a list."""
    if isinstance(val, list):
        return val
    if isinstance(val, str):
        try:
            return json.loads(val)
        except (json.JSONDecodeError, TypeError):
            return []
    return []


def is_crypto_market(question: str) -> bool:
    q = question.lower()
    return any(kw in q for kw in CRYPTO_KEYWORDS)


def is_unresolved(prices: list[float]) -> bool:
    """A market is unresolved if no outcome has price >= 0.999."""
    if not prices or len(prices) < 2:
        return False
    return max(prices) < 0.999


# ── Task 0.3: Discover active crypto markets ────────────────────────

async def discover_rounds(session: aiohttp.ClientSession) -> list[dict]:
    """Scan Gamma /markets for active, unresolved crypto markets."""
    print(f"\n{'='*60}")
    print(f"[{ts()}] TASK 0.3 — Discover active crypto markets")
    print(f"{'='*60}")

    rounds = []
    pages_scanned = 0

    # Scan top markets by 24h volume to find active crypto ones
    for offset in range(0, 200, 20):
        params = {
            "active": "true",
            "limit": "20",
            "offset": str(offset),
            "order": "volume24hr",
            "ascending": "false",
        }
        async with session.get(GAMMA_MARKETS_URL, params=params) as resp:
            if resp.status != 200:
                print(f"  Page {offset//20}: HTTP {resp.status}")
                break
            mkts = await resp.json()

        pages_scanned += 1
        if not mkts:
            break

        for mkt in mkts:
            question = mkt.get("question", "?")
            if not is_crypto_market(question):
                continue

            clob_ids = parse_json_field(mkt.get("clobTokenIds", "[]"))
            outcomes = parse_json_field(mkt.get("outcomes", "[]"))
            prices_raw = parse_json_field(mkt.get("outcomePrices", "[]"))
            prices = [float(p) for p in prices_raw]

            if not clob_ids or not is_unresolved(prices):
                continue

            token_ids = []
            for i, tid in enumerate(clob_ids):
                token_ids.append({
                    "token_id": tid,
                    "outcome": outcomes[i] if i < len(outcomes) else f"outcome_{i}",
                    "price": prices[i] if i < len(prices) else None,
                })

            rounds.append({
                "question": question,
                "condition_id": mkt.get("conditionId", ""),
                "token_ids": token_ids,
                "volume24hr": mkt.get("volume24hr", 0),
                "end_date": mkt.get("endDate", ""),
            })

        # Stop early if we have enough
        if len(rounds) >= 10:
            break

    print(f"  Scanned {pages_scanned} pages ({pages_scanned * 20} markets)")
    print(f"  Found {len(rounds)} unresolved crypto markets")

    # Also try to find any high-volume non-crypto market for WS/REST fallback
    fallback_tokens = []
    if not rounds:
        print("  No crypto markets found! Searching for any active market...")
        params = {
            "active": "true", "limit": "5",
            "order": "volume24hr", "ascending": "false",
        }
        async with session.get(GAMMA_MARKETS_URL, params=params) as resp:
            if resp.status == 200:
                for mkt in await resp.json():
                    prices_raw = parse_json_field(mkt.get("outcomePrices", "[]"))
                    prices = [float(p) for p in prices_raw]
                    clob_ids = parse_json_field(mkt.get("clobTokenIds", "[]"))
                    if clob_ids and is_unresolved(prices):
                        outcomes = parse_json_field(mkt.get("outcomes", "[]"))
                        token_ids = []
                        for i, tid in enumerate(clob_ids):
                            token_ids.append({
                                "token_id": tid,
                                "outcome": outcomes[i] if i < len(outcomes) else f"outcome_{i}",
                                "price": prices[i] if i < len(prices) else None,
                            })
                        rounds.append({
                            "question": mkt.get("question", "?"),
                            "condition_id": mkt.get("conditionId", ""),
                            "token_ids": token_ids,
                            "volume24hr": mkt.get("volume24hr", 0),
                            "end_date": mkt.get("endDate", ""),
                            "fallback": True,
                        })
                        break

    for r in rounds:
        fb = " [FALLBACK]" if r.get("fallback") else ""
        print(f"\n  {r['question']}{fb}")
        print(f"    vol24=${r['volume24hr']:,.0f}  ends={r['end_date']}")
        for t in r["token_ids"]:
            print(f"    {t['outcome']:>6}: price={t['price']}  token={t['token_id'][:40]}...")

    return rounds


# ── Task 0.5: CLOB WebSocket test ───────────────────────────────────

async def test_clob_ws(token_ids: list[str]) -> dict:
    """Connect to CLOB WS, subscribe, collect events for 30s."""
    print(f"\n{'='*60}")
    print(f"[{ts()}] TASK 0.5 — CLOB WebSocket test ({len(token_ids)} tokens)")
    print(f"{'='*60}")

    if not token_ids:
        print("  SKIP: No token IDs to subscribe to.")
        return {"skipped": True, "reason": "no_tokens"}

    # Limit to 10 tokens
    sub_ids = token_ids[:10]
    print(f"  Subscribing to {len(sub_ids)} token IDs")
    for tid in sub_ids:
        print(f"    {tid[:50]}...")

    event_counts = defaultdict(int)
    events_sample = []
    total_messages = 0
    first_event_at = None

    try:
        async with websockets.connect(CLOB_WS_URL, ping_interval=None) as ws:
            print(f"  Connected to {CLOB_WS_URL}")

            # Send subscription
            sub_msg = {"assets_ids": sub_ids, "type": "market"}
            await ws.send(json.dumps(sub_msg))
            print(f"  Sent subscription: type=market, assets={len(sub_ids)}")

            start = time.monotonic()
            last_ping = start

            while time.monotonic() - start < WS_LISTEN_SECONDS:
                remaining = WS_LISTEN_SECONDS - (time.monotonic() - start)
                if remaining <= 0:
                    break

                # Send PING every 10s to keep connection alive
                now = time.monotonic()
                if now - last_ping >= PING_INTERVAL:
                    try:
                        pong = await ws.ping()
                        await asyncio.wait_for(pong, timeout=5)
                        last_ping = now
                    except Exception:
                        last_ping = now

                try:
                    raw = await asyncio.wait_for(
                        ws.recv(), timeout=min(remaining, PING_INTERVAL)
                    )
                except asyncio.TimeoutError:
                    continue
                except websockets.ConnectionClosed as e:
                    print(f"  WS closed: {e}")
                    break

                total_messages += 1
                if first_event_at is None:
                    first_event_at = time.monotonic() - start

                try:
                    data = json.loads(raw)
                except json.JSONDecodeError:
                    event_counts["unparseable"] += 1
                    continue

                # Handle both single objects and arrays
                items = data if isinstance(data, list) else [data]
                for item in items:
                    etype = item.get("event_type", item.get("type", "unknown"))
                    event_counts[etype] += 1
                    if len(events_sample) < 8:
                        events_sample.append(item)

            elapsed = time.monotonic() - start
            print(f"\n  Listened for {elapsed:.1f}s")
            print(f"  Total WS messages: {total_messages}")
            if first_event_at is not None:
                print(f"  First event after: {first_event_at:.2f}s")
            else:
                print("  No events received during listen period")

    except Exception as e:
        print(f"  WS ERROR: {type(e).__name__}: {e}")
        return {"error": str(e), "event_counts": dict(event_counts)}

    print(f"\n  Event type breakdown:")
    if event_counts:
        for etype, count in sorted(event_counts.items(), key=lambda x: -x[1]):
            print(f"    {etype}: {count}")
    else:
        print("    (no events)")

    if events_sample:
        print(f"\n  Sample events ({len(events_sample)}):")
        for i, ev in enumerate(events_sample):
            display = {}
            for k, v in ev.items():
                if isinstance(v, (list, dict)) and len(str(v)) > 200:
                    display[k] = f"<{type(v).__name__} len={len(v)}>"
                elif isinstance(v, str) and len(v) > 80:
                    display[k] = v[:80] + "..."
                else:
                    display[k] = v
            print(f"    [{i}] {json.dumps(display, default=str)}")

    target_types = {"book", "price_change", "last_trade_price", "tick_size_change"}
    found = target_types & set(event_counts.keys())
    print(f"\n  Target event types found: {found or 'NONE'}")

    return {
        "total_messages": total_messages,
        "event_counts": dict(event_counts),
        "target_types_found": list(found),
        "first_event_delay_s": first_event_at,
    }


# ── Task 0.7: Spread viability ──────────────────────────────────────

def analyze_spreads(rounds: list[dict]) -> list[dict]:
    """Check p_up + p_down for bilateral market-making viability.

    For a binary market, p_yes + p_no should theoretically be <= 1.0.
    The spread (1 - sum) represents the vig/overround.
    For bilateral MM we need sum < 0.97 (3%+ margin).
    """
    print(f"\n{'='*60}")
    print(f"[{ts()}] TASK 0.7 — Spread viability analysis")
    print(f"{'='*60}")

    results = []
    for r in rounds:
        tokens = r["token_ids"]
        if len(tokens) < 2:
            continue

        prices = [t["price"] for t in tokens if t["price"] is not None]
        if len(prices) < 2:
            continue

        total = sum(prices)
        spread = 1.0 - total
        viable = total < 0.97

        result = {
            "question": r["question"],
            "prices": {t["outcome"]: t["price"] for t in tokens},
            "total": round(total, 6),
            "spread": round(spread, 6),
            "spread_pct": round(spread * 100, 2),
            "viable_for_bilateral_mm": viable,
        }
        results.append(result)

        status = "VIABLE" if viable else "TOO TIGHT"
        print(f"\n  {r['question']}")
        for t in tokens:
            print(f"    {t['outcome']:>6}: {t['price']}")
        print(f"    Sum: {total:.6f}  Spread: {spread:.6f} ({spread*100:.2f}%)  [{status}]")

    if not results:
        print("  No binary markets with prices found for spread analysis.")

    viable_count = sum(1 for r in results if r["viable_for_bilateral_mm"])
    print(f"\n  Viable markets (sum < 0.97): {viable_count}/{len(results)}")
    return results


# ── REST fallback test ───────────────────────────────────────────────

async def test_rest_midpoint(session: aiohttp.ClientSession, token_id: str) -> dict:
    """Test CLOB REST midpoint endpoint."""
    print(f"\n{'='*60}")
    print(f"[{ts()}] REST fallback — /midpoint test")
    print(f"{'='*60}")

    url = f"{CLOB_REST_URL}/midpoint"
    params = {"token_id": token_id}
    print(f"  GET {url}?token_id={token_id[:50]}...")

    try:
        async with session.get(url, params=params) as resp:
            status = resp.status
            text = await resp.text()
            print(f"  Status: {status}")
            print(f"  Response: {text[:500]}")

            result = {"status": status, "raw": text[:500]}
            if status == 200:
                try:
                    data = json.loads(text)
                    result["parsed"] = data
                    result["format_ok"] = True
                    print(f"  Parsed JSON: {json.dumps(data, indent=2)}")
                except json.JSONDecodeError:
                    result["format_ok"] = False
                    print("  WARNING: Response is not valid JSON")
            else:
                result["format_ok"] = False

            # Also test /book endpoint as additional data source
            print(f"\n  Also testing /book endpoint...")
            book_url = f"{CLOB_REST_URL}/book"
            async with session.get(book_url, params={"token_id": token_id}) as bresp:
                bstatus = bresp.status
                btext = await bresp.text()
                print(f"  GET /book  →  {bstatus}")
                if bstatus == 200:
                    try:
                        bdata = json.loads(btext)
                        # Show structure without dumping full orderbook
                        if isinstance(bdata, dict):
                            keys = list(bdata.keys())
                            print(f"  Book keys: {keys}")
                            for k in ["bids", "asks"]:
                                if k in bdata and isinstance(bdata[k], list):
                                    print(f"    {k}: {len(bdata[k])} levels")
                                    if bdata[k]:
                                        print(f"    {k}[0]: {bdata[k][0]}")
                        result["book_ok"] = True
                    except json.JSONDecodeError:
                        result["book_ok"] = False
                else:
                    print(f"  Book response: {btext[:200]}")
                    result["book_ok"] = False

            # Also test /price endpoint
            print(f"\n  Also testing /price endpoint...")
            price_url = f"{CLOB_REST_URL}/price"
            async with session.get(price_url, params={
                "token_id": token_id, "side": "BUY"
            }) as presp:
                pstatus = presp.status
                ptext = await presp.text()
                print(f"  GET /price?side=BUY  →  {pstatus}")
                print(f"  Response: {ptext[:300]}")
                result["price_ok"] = pstatus == 200

            return result

    except Exception as e:
        print(f"  ERROR: {e}")
        return {"error": str(e)}


# ── Main ─────────────────────────────────────────────────────────────

async def main():
    print(f"Phase 0: Polymarket Data Feasibility Tests")
    print(f"Started at {datetime.now(timezone.utc).isoformat()}")
    print(f"WS listen duration: {WS_LISTEN_SECONDS}s")

    async with aiohttp.ClientSession() as session:
        # Task 0.3: Discover rounds
        rounds = await discover_rounds(session)

        # Collect all token IDs (prefer unresolved crypto markets)
        all_token_ids = []
        for r in rounds:
            for t in r["token_ids"]:
                all_token_ids.append(t["token_id"])

        # Task 0.5: CLOB WS
        ws_result = await test_clob_ws(all_token_ids)

        # Task 0.7: Spread analysis
        spread_results = analyze_spreads(rounds)

        # REST fallback
        rest_result = {}
        if all_token_ids:
            rest_result = await test_rest_midpoint(session, all_token_ids[0])
        else:
            print(f"\n{'='*60}")
            print(f"[{ts()}] REST fallback — SKIPPED (no tokens)")
            print(f"{'='*60}")

    # ── Summary ──────────────────────────────────────────────────────
    print(f"\n{'='*60}")
    print(f"SUMMARY")
    print(f"{'='*60}")
    print(f"  Rounds discovered:     {len(rounds)}")
    print(f"  Token IDs found:       {len(all_token_ids)}")

    if not ws_result.get("skipped"):
        print(f"  WS messages received:  {ws_result.get('total_messages', 0)}")
        ec = ws_result.get("event_counts", {})
        print(f"  WS event types:        {dict(ec)}")
        target = ws_result.get("target_types_found", [])
        print(f"  Target types found:    {target}")
        ws_ok = len(target) > 0
        print(f"  WS feasibility:        {'PASS' if ws_ok else 'FAIL - no target events'}")
        if not ws_ok and ws_result.get("total_messages", 0) > 0:
            print(f"    (received messages but none matched target types)")
    else:
        print(f"  WS test:               SKIPPED ({ws_result.get('reason')})")

    viable = [r for r in spread_results if r["viable_for_bilateral_mm"]]
    print(f"  Spread viable markets: {len(viable)}/{len(spread_results)}")
    if spread_results:
        avg_total = sum(r["total"] for r in spread_results) / len(spread_results)
        avg_spread = sum(r["spread_pct"] for r in spread_results) / len(spread_results)
        print(f"  Avg price sum:         {avg_total:.4f}")
        print(f"  Avg spread:            {avg_spread:.2f}%")
        print(f"  Spread feasibility:    {'PASS' if viable else 'FAIL - all too tight'}")

    rest_ok = rest_result.get("format_ok", False)
    book_ok = rest_result.get("book_ok", False)
    price_ok = rest_result.get("price_ok", False)
    print(f"  REST /midpoint:        {'PASS' if rest_ok else 'FAIL or SKIPPED'}")
    print(f"  REST /book:            {'PASS' if book_ok else 'FAIL or SKIPPED'}")
    print(f"  REST /price:           {'PASS' if price_ok else 'FAIL or SKIPPED'}")

    overall = len(rounds) > 0
    print(f"\n  Overall data access:   {'PASS' if overall else 'NEEDS INVESTIGATION'}")
    print(f"\nCompleted at {datetime.now(timezone.utc).isoformat()}")


if __name__ == "__main__":
    asyncio.run(main())

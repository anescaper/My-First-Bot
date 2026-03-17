"""
Phase 0 Data Feasibility — Binance Data Sources

Task 0.1: Binance Spot WebSocket (btcusdt@trade) — 30s tick count
Task 0.2: Binance Futures REST — funding rate, OI, taker buy/sell ratio
"""

import asyncio
import json
import time
import sys
import requests
import websockets


# ── Task 0.1: Spot WebSocket Trade Stream ─────────────────────────────────────

async def test_spot_ws(duration_s: int = 30):
    uri = "wss://stream.binance.com:9443/ws/btcusdt@trade"
    ticks = []
    prices = []
    t0 = time.monotonic()
    deadline = t0 + duration_s

    print(f"\n{'='*60}")
    print(f"TASK 0.1 — Binance Spot WS (btcusdt@trade, {duration_s}s)")
    print(f"{'='*60}")
    print(f"Connecting to {uri} ...")

    try:
        async with websockets.connect(uri, close_timeout=5) as ws:
            print(f"Connected. Collecting trades for {duration_s}s ...\n")
            while time.monotonic() < deadline:
                try:
                    raw = await asyncio.wait_for(
                        ws.recv(),
                        timeout=max(0.1, deadline - time.monotonic()),
                    )
                    msg = json.loads(raw)
                    ticks.append(msg)
                    prices.append(float(msg.get("p", 0)))
                except asyncio.TimeoutError:
                    break
    except Exception as e:
        print(f"WS ERROR: {e}")
        return False

    elapsed = time.monotonic() - t0
    count = len(ticks)
    rate = count / elapsed if elapsed > 0 else 0

    print(f"  Duration        : {elapsed:.1f}s")
    print(f"  Ticks received  : {count}")
    print(f"  Rate            : {rate:.1f} ticks/s")

    if prices:
        print(f"  Price range     : {min(prices):.2f} – {max(prices):.2f}")
        print(f"  Last price      : {prices[-1]:.2f}")

    # Show one sample message
    if ticks:
        sample = ticks[0]
        print(f"\n  Sample message fields: {sorted(sample.keys())}")
        print(f"  Sample: {json.dumps(sample, indent=4)}")

    expected_min, expected_max = 300, 1500
    in_range = expected_min <= count <= expected_max
    status = "PASS" if in_range else "WARN"
    print(f"\n  Expected {expected_min}–{expected_max} ticks → {status} ({count})")
    if count < 100:
        print("  FAIL — fewer than 100 ticks, data source may be unreliable")
        return False
    return True


# ── Task 0.2: Futures REST Endpoints ─────────────────────────────────────────

def test_futures_rest():
    print(f"\n{'='*60}")
    print("TASK 0.2 — Binance Futures REST (BTCUSDT)")
    print(f"{'='*60}")

    results = {}
    all_ok = True

    # 1. Funding Rate
    print("\n─── Funding Rate ───")
    url = "https://fapi.binance.com/fapi/v1/fundingRate"
    try:
        r = requests.get(url, params={"symbol": "BTCUSDT", "limit": 1}, timeout=10)
        r.raise_for_status()
        data = r.json()
        print(f"  Status   : {r.status_code}")
        print(f"  Response : {json.dumps(data, indent=4)}")
        if data and isinstance(data, list) and len(data) > 0:
            fr = data[0]
            rate_val = float(fr.get("fundingRate", 0))
            print(f"  Funding rate  : {rate_val:.6f} ({rate_val*100:.4f}%)")
            print(f"  Fields        : {sorted(fr.keys())}")
            results["funding_rate"] = rate_val
            print("  ✓ Format OK")
        else:
            print("  ✗ Unexpected format")
            all_ok = False
    except Exception as e:
        print(f"  ERROR: {e}")
        all_ok = False

    # 2. Open Interest
    print("\n─── Open Interest ───")
    url = "https://fapi.binance.com/fapi/v1/openInterest"
    try:
        r = requests.get(url, params={"symbol": "BTCUSDT"}, timeout=10)
        r.raise_for_status()
        data = r.json()
        print(f"  Status   : {r.status_code}")
        print(f"  Response : {json.dumps(data, indent=4)}")
        if data and isinstance(data, dict):
            oi = float(data.get("openInterest", 0))
            print(f"  Open Interest : {oi:.2f} BTC")
            print(f"  Fields        : {sorted(data.keys())}")
            results["open_interest"] = oi
            print("  ✓ Format OK")
        else:
            print("  ✗ Unexpected format")
            all_ok = False
    except Exception as e:
        print(f"  ERROR: {e}")
        all_ok = False

    # 3. Taker Long/Short Ratio
    print("\n─── Taker Buy/Sell Ratio ───")
    url = "https://fapi.binance.com/futures/data/takerlongshortRatio"
    try:
        r = requests.get(
            url, params={"symbol": "BTCUSDT", "period": "5m", "limit": 1}, timeout=10
        )
        r.raise_for_status()
        data = r.json()
        print(f"  Status   : {r.status_code}")
        print(f"  Response : {json.dumps(data, indent=4)}")
        if data and isinstance(data, list) and len(data) > 0:
            rec = data[0]
            ratio = float(rec.get("buySellRatio", 0))
            buy_vol = float(rec.get("buyVol", 0))
            sell_vol = float(rec.get("sellVol", 0))
            print(f"  Buy/Sell ratio : {ratio:.4f}")
            print(f"  Buy volume     : {buy_vol:.2f}")
            print(f"  Sell volume    : {sell_vol:.2f}")
            print(f"  Fields         : {sorted(rec.keys())}")
            results["taker_ratio"] = ratio
            print("  ✓ Format OK")
        else:
            print("  ✗ Unexpected format")
            all_ok = False
    except Exception as e:
        print(f"  ERROR: {e}")
        all_ok = False

    return all_ok, results


# ── Main ──────────────────────────────────────────────────────────────────────

async def main():
    print("Phase 0 — Binance Data Feasibility Test")
    print(f"Started at {time.strftime('%Y-%m-%d %H:%M:%S UTC', time.gmtime())}")

    ws_ok = await test_spot_ws(duration_s=30)
    rest_ok, rest_data = test_futures_rest()

    print(f"\n{'='*60}")
    print("SUMMARY")
    print(f"{'='*60}")
    print(f"  Task 0.1 (Spot WS)      : {'PASS' if ws_ok else 'FAIL'}")
    print(f"  Task 0.2 (Futures REST)  : {'PASS' if rest_ok else 'FAIL'}")
    if rest_data:
        print(f"    Funding rate           : {rest_data.get('funding_rate', 'N/A')}")
        print(f"    Open interest (BTC)    : {rest_data.get('open_interest', 'N/A')}")
        print(f"    Taker buy/sell ratio   : {rest_data.get('taker_ratio', 'N/A')}")

    overall = ws_ok and rest_ok
    print(f"\n  Overall                  : {'ALL PASS' if overall else 'SOME FAILURES'}")
    print(f"{'='*60}\n")

    return 0 if overall else 1


if __name__ == "__main__":
    rc = asyncio.run(main())
    sys.exit(rc)

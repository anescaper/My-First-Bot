"""
Fleet Analysis — compare 8 Polymarket bots after 72+ hours of paper trading.

Usage:
    python fleet_analysis.py [--host HOST] [--output PATH]

Fetches /status, /metrics, /crypto/metrics, /history/positions, /history/signals
from each bot instance, computes per-bot performance metrics, runs bootstrap
significance tests, applies fee adjustments, and flags kill conditions.
"""

import argparse
import json
import sys
import os
from datetime import datetime, timezone
from typing import Optional

import numpy as np
import requests

# ── Bot fleet configuration ──────────────────────────────────────────────────

BOTS = {
    "A":   {"port": 4301, "type": "directional", "label": "Control (Baseline)"},
    "B1":  {"port": 4305, "type": "directional", "label": "GARCH-t"},
    "D1":  {"port": 4311, "type": "directional", "label": "IMM Adaptive"},
    "D2":  {"port": 4312, "type": "directional", "label": "Monte Carlo"},
    "D3":  {"port": 4313, "type": "directional", "label": "Stochastic Vol"},
    "D4":  {"port": 4314, "type": "directional", "label": "LMSR Filter"},
    "MM1": {"port": 4321, "type": "mm",          "label": "Bilateral MM"},
    "H1":  {"port": 4331, "type": "directional", "label": "Factor Model"},
}

# ── Fee model (Section 5.4) ─────────────────────────────────────────────────

TAKER_FEE = 0.02   # 2% taker fee
MAKER_FEE = 0.00   # 0% maker fee
MM_MAKER_RATIO = 0.50  # MM assumes 50% maker / 50% taker

# ── Kill conditions (Section 11.2) ──────────────────────────────────────────

KILL_DIRECTIONAL = {"sharpe_min": 1.0, "win_rate_min": 40.0, "max_dd_max": 15.0}
KILL_MM = {"sharpe_min": 1.5, "win_rate_min": 85.0, "max_dd_max": 10.0}

# ── Bootstrap config ────────────────────────────────────────────────────────

BOOTSTRAP_N = 1000
BOOTSTRAP_CI = 0.95


# ═══════════════════════════════════════════════════════════════════════════════
# Data fetching
# ═══════════════════════════════════════════════════════════════════════════════

def fetch_json(url: str, timeout: float = 10.0) -> Optional[dict]:
    try:
        resp = requests.get(url, timeout=timeout)
        resp.raise_for_status()
        return resp.json()
    except (requests.RequestException, ValueError):
        return None


def fetch_bot_data(host: str, port: int) -> dict:
    """Fetch all relevant endpoints for a single bot."""
    base = f"{host}:{port}"
    data = {}

    data["status"] = fetch_json(f"{base}/status")
    data["metrics"] = fetch_json(f"{base}/metrics")
    data["crypto_metrics"] = fetch_json(f"{base}/crypto/metrics")
    data["positions"] = fetch_json(f"{base}/positions")
    data["closed_positions"] = fetch_json(f"{base}/closed-positions")

    # History endpoints — fetch all available data (high limit)
    data["history_positions"] = fetch_json(
        f"{base}/history/positions?pipeline=crypto&limit=10000"
    )
    data["history_signals"] = fetch_json(
        f"{base}/history/signals?pipeline=crypto&limit=10000"
    )
    data["history_metrics"] = fetch_json(
        f"{base}/history/metrics?pipeline=crypto&limit=10000"
    )

    return data


def fetch_all_bots(host: str) -> dict:
    """Fetch data from all 8 bots. Returns {bot_id: data_dict}."""
    results = {}
    for bot_id, info in BOTS.items():
        print(f"  Fetching {bot_id} ({info['label']}) on port {info['port']}...", end=" ")
        data = fetch_bot_data(host, info["port"])
        if data["status"] is not None:
            print("OK")
            results[bot_id] = data
        else:
            print("UNREACHABLE")
    return results


# ═══════════════════════════════════════════════════════════════════════════════
# Per-bot metrics computation
# ═══════════════════════════════════════════════════════════════════════════════

def compute_bot_metrics(bot_id: str, data: dict) -> dict:
    """Extract and cross-check metrics for a single bot."""
    info = BOTS[bot_id]

    # Primary source: /metrics or /crypto/metrics
    m = data.get("crypto_metrics") or data.get("metrics") or {}

    total_trades = int(m.get("total_trades", 0))
    wins = int(m.get("wins", 0))
    losses = int(m.get("losses", 0))
    win_rate = float(m.get("win_rate", 0.0))
    sharpe = float(m.get("sharpe_ratio", 0.0))
    max_dd = float(m.get("max_drawdown", 0.0))
    profit_factor = float(m.get("profit_factor", 0.0))
    total_pnl = float(m.get("total_pnl", 0.0))

    # Cross-check from trade history
    history_pos = []
    hp_data = data.get("history_positions")
    if hp_data and isinstance(hp_data, dict):
        history_pos = hp_data.get("positions", [])
    elif hp_data and isinstance(hp_data, list):
        history_pos = hp_data

    pnl_series = [float(p.get("pnl", 0.0)) for p in history_pos if p.get("pnl") is not None]

    hist_trades = len(pnl_series)
    hist_pnl = sum(pnl_series)
    hist_wins = sum(1 for p in pnl_series if p > 0)
    hist_win_rate = (hist_wins / hist_trades * 100) if hist_trades > 0 else 0.0
    hist_gross_profit = sum(p for p in pnl_series if p > 0)
    hist_gross_loss = abs(sum(p for p in pnl_series if p < 0))
    hist_profit_factor = (hist_gross_profit / hist_gross_loss) if hist_gross_loss > 0 else float("inf")

    avg_profit_per_trade = (total_pnl / total_trades) if total_trades > 0 else 0.0

    # Uptime
    uptime_hours = 0.0
    status = data.get("status")
    if status and status.get("started_at"):
        try:
            started = datetime.fromisoformat(status["started_at"].replace("Z", "+00:00"))
            uptime_hours = (datetime.now(timezone.utc) - started).total_seconds() / 3600
        except (ValueError, TypeError):
            pass

    # Equity curve from metrics endpoint (general pipeline)
    equity_curve = []
    gm = data.get("metrics")
    if gm and isinstance(gm, dict):
        equity_curve = gm.get("equity_curve", [])

    return {
        "bot_id": bot_id,
        "label": info["label"],
        "type": info["type"],
        "port": info["port"],
        # Primary metrics (from /metrics endpoint)
        "total_trades": total_trades,
        "wins": wins,
        "losses": losses,
        "win_rate": win_rate,
        "sharpe": sharpe,
        "max_drawdown": max_dd,
        "profit_factor": profit_factor,
        "total_pnl": total_pnl,
        "avg_profit_per_trade": avg_profit_per_trade,
        "uptime_hours": round(uptime_hours, 2),
        # Cross-check from history
        "hist_trades": hist_trades,
        "hist_pnl": hist_pnl,
        "hist_win_rate": hist_win_rate,
        "hist_profit_factor": hist_profit_factor,
        # Raw PnL series for bootstrap
        "pnl_series": pnl_series,
        "equity_curve": equity_curve,
    }


# ═══════════════════════════════════════════════════════════════════════════════
# Bootstrap confidence intervals (Section 5.3)
# ═══════════════════════════════════════════════════════════════════════════════

def bootstrap_sharpe(pnl_series: list, n_resamples: int = BOOTSTRAP_N,
                     ci: float = BOOTSTRAP_CI) -> dict:
    """Compute bootstrap confidence interval for annualized Sharpe ratio.

    Uses daily returns approximation: each trade's PnL is treated as a
    return observation. Annualization factor = sqrt(365) for crypto markets.
    """
    arr = np.array(pnl_series, dtype=np.float64)
    if len(arr) < 5:
        return {"sharpe_mean": 0.0, "ci_lower": 0.0, "ci_upper": 0.0, "n_trades": len(arr)}

    rng = np.random.default_rng(42)
    sharpes = np.empty(n_resamples)

    for i in range(n_resamples):
        sample = rng.choice(arr, size=len(arr), replace=True)
        mu = np.mean(sample)
        sigma = np.std(sample, ddof=1)
        sharpes[i] = (mu / sigma * np.sqrt(365)) if sigma > 1e-12 else 0.0

    alpha = (1 - ci) / 2
    return {
        "sharpe_mean": float(np.mean(sharpes)),
        "ci_lower": float(np.percentile(sharpes, alpha * 100)),
        "ci_upper": float(np.percentile(sharpes, (1 - alpha) * 100)),
        "n_trades": len(arr),
    }


def pairwise_sharpe_test(bots_metrics: list) -> list:
    """For each pair of bots, test whether Sharpe difference is significant.

    Uses bootstrap of the difference: resample both PnL series, compute
    Sharpe for each, take difference. If 95% CI excludes 0, significant.
    """
    results = []
    rng = np.random.default_rng(123)

    for i in range(len(bots_metrics)):
        for j in range(i + 1, len(bots_metrics)):
            a = bots_metrics[i]
            b = bots_metrics[j]
            arr_a = np.array(a["pnl_series"], dtype=np.float64)
            arr_b = np.array(b["pnl_series"], dtype=np.float64)

            if len(arr_a) < 5 or len(arr_b) < 5:
                results.append({
                    "bot_a": a["bot_id"],
                    "bot_b": b["bot_id"],
                    "diff_mean": 0.0,
                    "ci_lower": 0.0,
                    "ci_upper": 0.0,
                    "significant": False,
                    "insufficient_data": True,
                })
                continue

            diffs = np.empty(BOOTSTRAP_N)
            for k in range(BOOTSTRAP_N):
                sa = rng.choice(arr_a, size=len(arr_a), replace=True)
                sb = rng.choice(arr_b, size=len(arr_b), replace=True)
                mu_a, sig_a = np.mean(sa), np.std(sa, ddof=1)
                mu_b, sig_b = np.mean(sb), np.std(sb, ddof=1)
                sharpe_a = (mu_a / sig_a * np.sqrt(365)) if sig_a > 1e-12 else 0.0
                sharpe_b = (mu_b / sig_b * np.sqrt(365)) if sig_b > 1e-12 else 0.0
                diffs[k] = sharpe_a - sharpe_b

            ci_lo = float(np.percentile(diffs, 2.5))
            ci_hi = float(np.percentile(diffs, 97.5))
            significant = (ci_lo > 0) or (ci_hi < 0)

            results.append({
                "bot_a": a["bot_id"],
                "bot_b": b["bot_id"],
                "diff_mean": float(np.mean(diffs)),
                "ci_lower": ci_lo,
                "ci_upper": ci_hi,
                "significant": significant,
                "insufficient_data": False,
            })

    return results


# ═══════════════════════════════════════════════════════════════════════════════
# Fee-adjusted analysis (Section 5.4)
# ═══════════════════════════════════════════════════════════════════════════════

def apply_fees(metrics: dict) -> dict:
    """Recalculate P&L and Sharpe after applying realistic fee model."""
    bot_type = metrics["type"]
    pnl_series = metrics["pnl_series"]

    if not pnl_series:
        return {
            "total_pnl_post_fee": 0.0,
            "sharpe_post_fee": 0.0,
            "total_fees": 0.0,
            "fee_rate": 0.0,
        }

    # Estimate trade sizes from history — use absolute PnL as proxy for trade size
    # Each trade has entry + exit = 2 transactions
    # Fee per trade = size * fee_rate * 2 (round-trip)
    # Conservative: estimate size from position data, fall back to PnL-based heuristic
    if bot_type == "mm":
        # MM: 50% maker (0%) + 50% taker (2%) = 1% effective per side
        effective_rate = MM_MAKER_RATIO * MAKER_FEE + (1 - MM_MAKER_RATIO) * TAKER_FEE
    else:
        # Directional: all taker
        effective_rate = TAKER_FEE

    # For each trade, estimate fee as: trade_notional * effective_rate * 2 (round-trip)
    # Without exact size data, use a heuristic: assume average position ~$50
    # This will be refined once we have position size data from history
    avg_position_size = 50.0  # conservative estimate for paper trading
    fee_per_trade = avg_position_size * effective_rate * 2  # round-trip

    total_fees = fee_per_trade * len(pnl_series)
    fee_adjusted_pnl = [p - fee_per_trade for p in pnl_series]
    total_pnl_post_fee = sum(fee_adjusted_pnl)

    # Sharpe post-fee
    arr = np.array(fee_adjusted_pnl, dtype=np.float64)
    if len(arr) >= 2:
        mu = np.mean(arr)
        sigma = np.std(arr, ddof=1)
        sharpe_post_fee = (mu / sigma * np.sqrt(365)) if sigma > 1e-12 else 0.0
    else:
        sharpe_post_fee = 0.0

    return {
        "total_pnl_post_fee": round(total_pnl_post_fee, 4),
        "sharpe_post_fee": round(sharpe_post_fee, 4),
        "total_fees": round(total_fees, 4),
        "fee_rate": effective_rate,
    }


# ═══════════════════════════════════════════════════════════════════════════════
# Kill condition checks (Section 11.2)
# ═══════════════════════════════════════════════════════════════════════════════

def check_kill_conditions(metrics: dict) -> dict:
    """Check if a bot meets any kill condition."""
    bot_type = metrics["type"]
    thresholds = KILL_MM if bot_type == "mm" else KILL_DIRECTIONAL

    violations = []
    if metrics["sharpe"] < thresholds["sharpe_min"]:
        violations.append(
            f"Sharpe {metrics['sharpe']:.2f} < {thresholds['sharpe_min']}"
        )
    if metrics["win_rate"] < thresholds["win_rate_min"]:
        violations.append(
            f"Win rate {metrics['win_rate']:.1f}% < {thresholds['win_rate_min']}%"
        )
    if metrics["max_drawdown"] > thresholds["max_dd_max"]:
        violations.append(
            f"Max DD {metrics['max_drawdown']:.1f}% > {thresholds['max_dd_max']}%"
        )

    return {
        "kill": len(violations) > 0,
        "violations": violations,
        "thresholds": thresholds,
    }


# ═══════════════════════════════════════════════════════════════════════════════
# Output formatting
# ═══════════════════════════════════════════════════════════════════════════════

def print_separator(char: str = "=", width: int = 120):
    print(char * width)


def print_header(title: str):
    print()
    print_separator()
    print(f"  {title}")
    print_separator()
    print()


def print_metrics_table(all_metrics: list):
    """Print formatted performance table for all bots."""
    print_header("PER-BOT PERFORMANCE METRICS")

    header = (
        f"{'Bot':<6} {'Label':<20} {'Type':<5} {'Trades':>7} {'Wins':>5} "
        f"{'WR%':>7} {'Sharpe':>8} {'MaxDD%':>8} {'PF':>8} "
        f"{'PnL($)':>10} {'$/Trade':>9} {'Uptime':>8}"
    )
    print(header)
    print("-" * len(header))

    for m in all_metrics:
        print(
            f"{m['bot_id']:<6} {m['label']:<20} {m['type']:<5} "
            f"{m['total_trades']:>7} {m['wins']:>5} "
            f"{m['win_rate']:>6.1f}% {m['sharpe']:>8.3f} "
            f"{m['max_drawdown']:>7.2f}% {m['profit_factor']:>8.3f} "
            f"{m['total_pnl']:>10.2f} {m['avg_profit_per_trade']:>9.4f} "
            f"{m['uptime_hours']:>7.1f}h"
        )

    # Cross-check section
    print()
    print("  Cross-check (from trade history):")
    sub_header = f"  {'Bot':<6} {'H.Trades':>8} {'H.PnL($)':>10} {'H.WR%':>7} {'H.PF':>8} {'Match':>6}"
    print(sub_header)
    print("  " + "-" * (len(sub_header) - 2))
    for m in all_metrics:
        match = "YES" if abs(m["total_pnl"] - m["hist_pnl"]) < 0.01 else "DIFF"
        pf_str = f"{m['hist_profit_factor']:.3f}" if m["hist_profit_factor"] != float("inf") else "inf"
        print(
            f"  {m['bot_id']:<6} {m['hist_trades']:>8} "
            f"{m['hist_pnl']:>10.2f} {m['hist_win_rate']:>6.1f}% "
            f"{pf_str:>8} {match:>6}"
        )


def print_bootstrap_results(all_metrics: list, bootstrap_results: dict):
    """Print bootstrap CI for each bot's Sharpe."""
    print_header("BOOTSTRAP SHARPE CONFIDENCE INTERVALS (1000 resamples, 95% CI)")

    header = f"{'Bot':<6} {'Label':<20} {'Sharpe':>8} {'CI Lower':>10} {'CI Upper':>10} {'N':>6}"
    print(header)
    print("-" * len(header))

    for m in all_metrics:
        bs = bootstrap_results.get(m["bot_id"], {})
        print(
            f"{m['bot_id']:<6} {m['label']:<20} "
            f"{bs.get('sharpe_mean', 0):>8.3f} "
            f"{bs.get('ci_lower', 0):>10.3f} "
            f"{bs.get('ci_upper', 0):>10.3f} "
            f"{bs.get('n_trades', 0):>6}"
        )


def print_pairwise_results(pairwise: list):
    """Print pairwise Sharpe comparison."""
    print_header("PAIRWISE SHARPE SIGNIFICANCE TESTS")

    sig_pairs = [p for p in pairwise if p["significant"]]
    insuf_pairs = [p for p in pairwise if p.get("insufficient_data")]

    if sig_pairs:
        header = f"{'Pair':<12} {'Diff':>8} {'CI Lower':>10} {'CI Upper':>10} {'Result':<12}"
        print(header)
        print("-" * len(header))
        for p in sig_pairs:
            winner = p["bot_a"] if p["diff_mean"] > 0 else p["bot_b"]
            print(
                f"{p['bot_a']+'v'+p['bot_b']:<12} "
                f"{p['diff_mean']:>8.3f} "
                f"{p['ci_lower']:>10.3f} "
                f"{p['ci_upper']:>10.3f} "
                f"{winner + ' wins':<12}"
            )
    else:
        print("  No statistically significant Sharpe differences found.")

    if insuf_pairs:
        print(f"\n  {len(insuf_pairs)} pair(s) skipped due to insufficient data (<5 trades).")

    print(f"\n  Total pairs tested: {len(pairwise)}")
    print(f"  Significant: {len(sig_pairs)}")
    print(f"  Not significant: {len(pairwise) - len(sig_pairs) - len(insuf_pairs)}")


def print_fee_analysis(all_metrics: list, fee_results: dict):
    """Print fee-adjusted results."""
    print_header("FEE-ADJUSTED ANALYSIS (taker 2%, maker 0%)")

    header = (
        f"{'Bot':<6} {'Label':<20} {'Type':<5} "
        f"{'Pre-Fee PnL':>12} {'Fees':>10} {'Post-Fee PnL':>13} "
        f"{'Pre Sharpe':>11} {'Post Sharpe':>12}"
    )
    print(header)
    print("-" * len(header))

    for m in all_metrics:
        f = fee_results.get(m["bot_id"], {})
        print(
            f"{m['bot_id']:<6} {m['label']:<20} {m['type']:<5} "
            f"{m['total_pnl']:>12.2f} {f.get('total_fees', 0):>10.2f} "
            f"{f.get('total_pnl_post_fee', 0):>13.2f} "
            f"{m['sharpe']:>11.3f} {f.get('sharpe_post_fee', 0):>12.3f}"
        )

    print()
    print("  Fee assumptions:")
    print("    Directional bots: 100% taker (2% per side, worst case)")
    print("    MM1 (Bilateral): 50% maker (0%) + 50% taker (2%) = 1% effective")
    print(f"    Estimated position size: $50 (paper trading default)")


def print_kill_analysis(all_metrics: list, kill_results: dict):
    """Print kill condition results."""
    print_header("KILL CONDITION ANALYSIS")

    kills = [(m["bot_id"], m["label"], kill_results[m["bot_id"]])
             for m in all_metrics if kill_results[m["bot_id"]]["kill"]]
    survivors = [(m["bot_id"], m["label"], kill_results[m["bot_id"]])
                 for m in all_metrics if not kill_results[m["bot_id"]]["kill"]]

    if kills:
        print("  RECOMMENDED FOR KILL:")
        for bot_id, label, kr in kills:
            violations_str = "; ".join(kr["violations"])
            print(f"    {bot_id} ({label}): {violations_str}")
    else:
        print("  No bots meet kill conditions.")

    print()
    if survivors:
        print("  SURVIVORS:")
        for bot_id, label, kr in survivors:
            print(f"    {bot_id} ({label}): All thresholds passed")

    # Print thresholds
    print()
    print("  Kill thresholds:")
    print(f"    Directional: Sharpe < {KILL_DIRECTIONAL['sharpe_min']}, "
          f"WR < {KILL_DIRECTIONAL['win_rate_min']}%, "
          f"MaxDD > {KILL_DIRECTIONAL['max_dd_max']}%")
    print(f"    MM:          Sharpe < {KILL_MM['sharpe_min']}, "
          f"WR < {KILL_MM['win_rate_min']}%, "
          f"MaxDD > {KILL_MM['max_dd_max']}%")


def print_rankings(all_metrics: list, fee_results: dict):
    """Print pre-fee and post-fee rankings, plus top 3 recommendation."""
    print_header("RANKINGS")

    # Pre-fee ranking by Sharpe
    pre_fee_ranked = sorted(all_metrics, key=lambda m: m["sharpe"], reverse=True)
    print("  Pre-fee ranking (by Sharpe):")
    for rank, m in enumerate(pre_fee_ranked, 1):
        print(f"    {rank}. {m['bot_id']} ({m['label']}): "
              f"Sharpe={m['sharpe']:.3f}, PnL=${m['total_pnl']:.2f}")

    # Post-fee ranking by Sharpe
    print()
    post_fee_ranked = sorted(
        all_metrics,
        key=lambda m: fee_results.get(m["bot_id"], {}).get("sharpe_post_fee", 0),
        reverse=True,
    )
    print("  Post-fee ranking (by Sharpe):")
    for rank, m in enumerate(post_fee_ranked, 1):
        f = fee_results.get(m["bot_id"], {})
        print(f"    {rank}. {m['bot_id']} ({m['label']}): "
              f"Sharpe={f.get('sharpe_post_fee', 0):.3f}, "
              f"PnL=${f.get('total_pnl_post_fee', 0):.2f}")

    # Top 3 recommendation
    print()
    print_separator("-")
    print("  TOP 3 FOR LIVE TRADING (post-fee Sharpe):")
    print_separator("-")
    for rank, m in enumerate(post_fee_ranked[:3], 1):
        f = fee_results.get(m["bot_id"], {})
        print(
            f"    #{rank}  {m['bot_id']} ({m['label']})"
            f"  |  Sharpe: {f.get('sharpe_post_fee', 0):.3f}"
            f"  |  WR: {m['win_rate']:.1f}%"
            f"  |  PnL: ${f.get('total_pnl_post_fee', 0):.2f}"
            f"  |  MaxDD: {m['max_drawdown']:.2f}%"
        )
    print()


# ═══════════════════════════════════════════════════════════════════════════════
# Main
# ═══════════════════════════════════════════════════════════════════════════════

def main():
    parser = argparse.ArgumentParser(
        description="Fleet analysis for 8 Polymarket bots after 72+ hours of paper trading."
    )
    parser.add_argument(
        "--host", default="http://167.172.70.15",
        help="Bot API host (default: http://167.172.70.15)",
    )
    parser.add_argument(
        "--output", default="data/fleet_analysis.json",
        help="Output JSON path (default: data/fleet_analysis.json)",
    )
    args = parser.parse_args()

    host = args.host.rstrip("/")
    output_path = args.output

    print_header("POLYMARKET BOT FLEET ANALYSIS")
    print(f"  Host: {host}")
    print(f"  Bots: {len(BOTS)}")
    print(f"  Time: {datetime.now(timezone.utc).strftime('%Y-%m-%d %H:%M:%S UTC')}")
    print()

    # ── 1. Fetch data ────────────────────────────────────────────────────────

    print("Fetching bot data...")
    raw_data = fetch_all_bots(host)
    print(f"\n  Reachable: {len(raw_data)}/{len(BOTS)} bots")

    if not raw_data:
        print("\nERROR: No bots reachable. Check host and ports.")
        sys.exit(1)

    # ── 2. Compute per-bot metrics ───────────────────────────────────────────

    all_metrics = []
    for bot_id in BOTS:
        if bot_id in raw_data:
            m = compute_bot_metrics(bot_id, raw_data[bot_id])
            all_metrics.append(m)

    print_metrics_table(all_metrics)

    # ── 3. Bootstrap confidence intervals ────────────────────────────────────

    bootstrap_results = {}
    for m in all_metrics:
        bootstrap_results[m["bot_id"]] = bootstrap_sharpe(m["pnl_series"])

    print_bootstrap_results(all_metrics, bootstrap_results)

    # ── 4. Pairwise Sharpe comparison ────────────────────────────────────────

    pairwise = pairwise_sharpe_test(all_metrics)
    print_pairwise_results(pairwise)

    # ── 5. Fee-adjusted analysis ─────────────────────────────────────────────

    fee_results = {}
    for m in all_metrics:
        fee_results[m["bot_id"]] = apply_fees(m)

    print_fee_analysis(all_metrics, fee_results)

    # ── 6. Kill condition checks ─────────────────────────────────────────────

    kill_results = {}
    for m in all_metrics:
        kill_results[m["bot_id"]] = check_kill_conditions(m)

    print_kill_analysis(all_metrics, kill_results)

    # ── 7. Rankings and recommendations ──────────────────────────────────────

    print_rankings(all_metrics, fee_results)

    # ── 8. Save JSON output ──────────────────────────────────────────────────

    output = {
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "host": host,
        "bots_reachable": len(raw_data),
        "bots_total": len(BOTS),
        "metrics": [],
        "bootstrap": bootstrap_results,
        "pairwise_tests": pairwise,
        "fee_analysis": {},
        "kill_conditions": {},
        "rankings": {
            "pre_fee_sharpe": [],
            "post_fee_sharpe": [],
            "top_3_live": [],
        },
    }

    # Serialize metrics (strip pnl_series for JSON)
    for m in all_metrics:
        entry = {k: v for k, v in m.items() if k != "pnl_series"}
        output["metrics"].append(entry)

    output["fee_analysis"] = fee_results
    output["kill_conditions"] = kill_results

    # Rankings
    pre_ranked = sorted(all_metrics, key=lambda m: m["sharpe"], reverse=True)
    post_ranked = sorted(
        all_metrics,
        key=lambda m: fee_results.get(m["bot_id"], {}).get("sharpe_post_fee", 0),
        reverse=True,
    )
    output["rankings"]["pre_fee_sharpe"] = [m["bot_id"] for m in pre_ranked]
    output["rankings"]["post_fee_sharpe"] = [m["bot_id"] for m in post_ranked]
    output["rankings"]["top_3_live"] = [m["bot_id"] for m in post_ranked[:3]]

    # Ensure output directory exists
    os.makedirs(os.path.dirname(output_path) or ".", exist_ok=True)

    with open(output_path, "w") as f:
        json.dump(output, f, indent=2, default=str)

    print(f"  Results saved to: {output_path}")
    print()


if __name__ == "__main__":
    main()

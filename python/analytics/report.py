#!/usr/bin/env python3
"""
Polymarket Bot Analytics Report Generator

Reads SQLite databases and generates an interactive HTML report with
temporal patterns, performance attribution, edge calibration, and risk analysis.

Usage:
    python report.py                          # Auto-detect DBs in ./data/
    python report.py --db data/polybot-b1.db  # Single bot
    python report.py --db *.db --fleet        # Fleet comparison
    python report.py --datahub data/datahub.db --market-context  # Include market volume/vol
"""

import argparse
import sqlite3
import os
import sys
from datetime import datetime, timezone
from pathlib import Path

import numpy as np
import pandas as pd
import plotly.graph_objects as go
from plotly.subplots import make_subplots
import plotly.io as pio


# ── Data Loading ──────────────────────────────────────────────────────────

def load_positions(db_path: str) -> pd.DataFrame:
    try:
        conn = sqlite3.connect(db_path)
        df = pd.read_sql_query(
            "SELECT * FROM positions ORDER BY closed_at ASC", conn
        )
        conn.close()
    except Exception:
        return pd.DataFrame()
    if df.empty:
        return df
    df["closed_at"] = pd.to_datetime(df["closed_at"], utc=True)
    df["opened_at"] = pd.to_datetime(df["opened_at"], utc=True)
    df["hour"] = df["closed_at"].dt.hour
    df["weekday"] = df["closed_at"].dt.day_name()
    df["duration_min"] = (df["closed_at"] - df["opened_at"]).dt.total_seconds() / 60
    df["win"] = (df["pnl"] > 0).astype(int)
    df["cum_pnl"] = df["pnl"].cumsum()
    return df


def load_signals(db_path: str) -> pd.DataFrame:
    try:
        conn = sqlite3.connect(db_path)
        df = pd.read_sql_query(
            "SELECT * FROM signals ORDER BY created_at ASC", conn
        )
        conn.close()
    except Exception:
        return pd.DataFrame()
    if df.empty:
        return df
    df["created_at"] = pd.to_datetime(df["created_at"], utc=True)
    df["hour"] = df["created_at"].dt.hour
    # Parse asset from "BTC FiveMin" format
    df["parsed_asset"] = df["asset"].str.split(" ").str[0]
    df["parsed_tf"] = df["asset"].str.split(" ").str[1].fillna("")
    return df


def load_rounds(db_path: str) -> pd.DataFrame:
    try:
        conn = sqlite3.connect(db_path)
        df = pd.read_sql_query(
            "SELECT * FROM crypto_rounds_history ORDER BY round_end ASC", conn
        )
        conn.close()
    except Exception:
        return pd.DataFrame()
    if df.empty:
        return df
    df["round_end"] = pd.to_datetime(df["round_end"], utc=True)
    df["round_start"] = pd.to_datetime(df["round_start"], utc=True)
    return df


def load_metrics(db_path: str) -> pd.DataFrame:
    try:
        conn = sqlite3.connect(db_path)
        df = pd.read_sql_query(
            "SELECT * FROM metrics_snapshots ORDER BY timestamp ASC", conn
        )
        conn.close()
    except Exception:
        return pd.DataFrame()
    if df.empty:
        return df
    df["timestamp"] = pd.to_datetime(df["timestamp"], utc=True)
    return df


def load_candles(db_path: str) -> pd.DataFrame:
    try:
        conn = sqlite3.connect(db_path)
        df = pd.read_sql_query("SELECT * FROM candles ORDER BY open_time ASC", conn)
        conn.close()
    except Exception:
        return pd.DataFrame()
    if df.empty:
        return df
    df["datetime"] = pd.to_datetime(df["open_time"], unit="ms", utc=True)
    df["hour"] = df["datetime"].dt.hour
    return df


# ── Chart Generators ──────────────────────────────────────────────────────

COLORS = {
    "green": "#22c55e", "red": "#ef4444", "blue": "#3b82f6",
    "orange": "#f97316", "purple": "#a855f7", "cyan": "#06b6d4",
    "gray": "#6b7280", "yellow": "#eab308",
    "BTC": "#f7931a", "ETH": "#627eea", "SOL": "#9945ff", "XRP": "#00aae4",
}


def chart_pnl_by_hour(pos: pd.DataFrame) -> go.Figure:
    """P&L and win rate by hour-of-day."""
    hourly = pos.groupby("hour").agg(
        total_pnl=("pnl", "sum"),
        count=("pnl", "count"),
        wins=("win", "sum"),
    ).reindex(range(24), fill_value=0)
    hourly["win_rate"] = (hourly["wins"] / hourly["count"].replace(0, np.nan) * 100).fillna(0)

    fig = make_subplots(specs=[[{"secondary_y": True}]])
    fig.add_trace(
        go.Bar(
            x=hourly.index, y=hourly["total_pnl"],
            name="P&L ($)",
            marker_color=[COLORS["green"] if v >= 0 else COLORS["red"] for v in hourly["total_pnl"]],
            opacity=0.8,
        ),
        secondary_y=False,
    )
    fig.add_trace(
        go.Scatter(
            x=hourly.index, y=hourly["win_rate"],
            name="Win Rate %", mode="lines+markers",
            line=dict(color=COLORS["cyan"], width=2),
        ),
        secondary_y=True,
    )
    fig.add_hline(y=50, line_dash="dash", line_color=COLORS["gray"],
                  opacity=0.5, secondary_y=True)
    fig.update_layout(
        title="P&L and Win Rate by Hour (UTC)",
        xaxis_title="Hour (UTC)", xaxis=dict(dtick=1),
        yaxis_title="P&L ($)", yaxis2_title="Win Rate %",
        template="plotly_dark", height=450,
    )
    return fig


def chart_trade_volume_heatmap(pos: pd.DataFrame) -> go.Figure:
    """Trade count heatmap: asset × hour."""
    if pos.empty:
        return go.Figure()
    pivot = pos.groupby(["asset", "hour"]).size().unstack(fill_value=0)
    pivot = pivot.reindex(columns=range(24), fill_value=0)

    fig = go.Figure(data=go.Heatmap(
        z=pivot.values, x=[f"{h:02d}" for h in range(24)],
        y=pivot.index.tolist(),
        colorscale="YlOrRd", hoverongaps=False,
        text=pivot.values, texttemplate="%{text}",
    ))
    fig.update_layout(
        title="Trade Count Heatmap: Asset × Hour (UTC)",
        xaxis_title="Hour (UTC)", yaxis_title="Asset",
        template="plotly_dark", height=300,
    )
    return fig


def chart_pnl_heatmap(pos: pd.DataFrame) -> go.Figure:
    """P&L heatmap: asset × hour."""
    if pos.empty:
        return go.Figure()
    pivot = pos.groupby(["asset", "hour"])["pnl"].sum().unstack(fill_value=0)
    pivot = pivot.reindex(columns=range(24), fill_value=0)

    fig = go.Figure(data=go.Heatmap(
        z=pivot.values, x=[f"{h:02d}" for h in range(24)],
        y=pivot.index.tolist(),
        colorscale="RdYlGn", zmid=0, hoverongaps=False,
        text=np.round(pivot.values, 2), texttemplate="$%{text}",
    ))
    fig.update_layout(
        title="P&L Heatmap: Asset × Hour (UTC)",
        xaxis_title="Hour (UTC)", yaxis_title="Asset",
        template="plotly_dark", height=300,
    )
    return fig


def chart_equity_curve(pos: pd.DataFrame) -> go.Figure:
    """Equity curve with drawdown overlay."""
    fig = make_subplots(rows=2, cols=1, shared_xaxes=True,
                        row_heights=[0.7, 0.3], vertical_spacing=0.05)

    # Equity curve
    fig.add_trace(
        go.Scatter(
            x=pos["closed_at"], y=pos["cum_pnl"],
            name="Cumulative P&L", mode="lines",
            line=dict(color=COLORS["green"], width=2),
            fill="tozeroy", fillcolor="rgba(34,197,94,0.1)",
        ),
        row=1, col=1,
    )

    # Drawdown
    running_max = pos["cum_pnl"].cummax()
    drawdown = pos["cum_pnl"] - running_max
    fig.add_trace(
        go.Scatter(
            x=pos["closed_at"], y=drawdown,
            name="Drawdown", mode="lines",
            line=dict(color=COLORS["red"], width=1.5),
            fill="tozeroy", fillcolor="rgba(239,68,68,0.15)",
        ),
        row=2, col=1,
    )

    fig.update_layout(
        title="Equity Curve & Drawdown",
        template="plotly_dark", height=500, showlegend=True,
    )
    fig.update_yaxes(title_text="P&L ($)", row=1, col=1)
    fig.update_yaxes(title_text="Drawdown ($)", row=2, col=1)
    return fig


def chart_performance_by_asset(pos: pd.DataFrame) -> go.Figure:
    """P&L, win rate, trade count by asset."""
    by_asset = pos.groupby("asset").agg(
        total_pnl=("pnl", "sum"),
        count=("pnl", "count"),
        wins=("win", "sum"),
        avg_pnl=("pnl", "mean"),
    )
    by_asset["win_rate"] = by_asset["wins"] / by_asset["count"] * 100

    fig = make_subplots(rows=1, cols=3, subplot_titles=["Total P&L", "Win Rate %", "Trade Count"])
    colors = [COLORS.get(a, COLORS["blue"]) for a in by_asset.index]

    fig.add_trace(go.Bar(x=by_asset.index, y=by_asset["total_pnl"],
                         marker_color=colors, showlegend=False), row=1, col=1)
    fig.add_trace(go.Bar(x=by_asset.index, y=by_asset["win_rate"],
                         marker_color=colors, showlegend=False), row=1, col=2)
    fig.add_trace(go.Bar(x=by_asset.index, y=by_asset["count"],
                         marker_color=colors, showlegend=False), row=1, col=3)

    fig.add_hline(y=50, line_dash="dash", line_color=COLORS["gray"],
                  opacity=0.5, row=1, col=2)
    fig.update_layout(title="Performance by Asset", template="plotly_dark", height=350)
    return fig


def chart_performance_by_timeframe(pos: pd.DataFrame) -> go.Figure:
    """P&L and win rate by timeframe."""
    by_tf = pos.groupby("timeframe").agg(
        total_pnl=("pnl", "sum"),
        count=("pnl", "count"),
        wins=("win", "sum"),
    )
    by_tf["win_rate"] = by_tf["wins"] / by_tf["count"] * 100

    fig = make_subplots(rows=1, cols=2, subplot_titles=["Total P&L by Timeframe", "Win Rate by Timeframe"])
    fig.add_trace(go.Bar(
        x=by_tf.index, y=by_tf["total_pnl"],
        marker_color=[COLORS["green"] if v >= 0 else COLORS["red"] for v in by_tf["total_pnl"]],
        showlegend=False,
    ), row=1, col=1)
    fig.add_trace(go.Bar(
        x=by_tf.index, y=by_tf["win_rate"],
        marker_color=COLORS["blue"], showlegend=False,
    ), row=1, col=2)
    fig.add_hline(y=50, line_dash="dash", line_color=COLORS["gray"],
                  opacity=0.5, row=1, col=2)
    fig.update_layout(title="Performance by Timeframe", template="plotly_dark", height=350)
    return fig


def chart_performance_by_direction(pos: pd.DataFrame) -> go.Figure:
    """P&L split by Up vs Down."""
    by_dir = pos.groupby("direction").agg(
        total_pnl=("pnl", "sum"),
        count=("pnl", "count"),
        wins=("win", "sum"),
        avg_pnl=("pnl", "mean"),
    )
    by_dir["win_rate"] = by_dir["wins"] / by_dir["count"] * 100

    fig = make_subplots(rows=1, cols=3,
                        subplot_titles=["Total P&L", "Win Rate %", "Avg P&L per Trade"])
    dir_colors = [COLORS["green"] if d == "Up" else COLORS["red"] for d in by_dir.index]
    fig.add_trace(go.Bar(
        x=by_dir.index, y=by_dir["total_pnl"],
        marker_color=dir_colors, showlegend=False,
    ), row=1, col=1)
    fig.add_trace(go.Bar(
        x=by_dir.index, y=by_dir["win_rate"],
        marker_color=dir_colors, showlegend=False,
    ), row=1, col=2)
    fig.add_trace(go.Bar(
        x=by_dir.index, y=by_dir["avg_pnl"],
        marker_color=dir_colors, showlegend=False,
    ), row=1, col=3)
    fig.update_layout(title="Performance by Direction (Up vs Down)", template="plotly_dark", height=350)
    return fig


def chart_exit_reasons(pos: pd.DataFrame) -> go.Figure:
    """Exit reason distribution."""
    # Normalize exit reasons
    reasons = pos["exit_reason"].str.replace(r"Stop loss:.*", "Stop loss", regex=True)
    reasons = reasons.str.replace(r"Edge reversed.*", "Edge reversed", regex=True)
    reasons = reasons.str.replace(r"Round ending.*with loss", "Round ending (loss)", regex=True)
    reasons = reasons.str.replace(r"Round no longer found.*", "Round disappeared", regex=True)

    counts = reasons.value_counts()
    fig = go.Figure(data=[go.Pie(
        labels=counts.index, values=counts.values,
        hole=0.4, textinfo="label+percent",
        marker_colors=[COLORS["green"], COLORS["red"], COLORS["orange"],
                       COLORS["blue"], COLORS["purple"], COLORS["gray"]][:len(counts)],
    )])
    fig.update_layout(title="Exit Reason Distribution", template="plotly_dark", height=400)
    return fig


def chart_edge_calibration(rounds: pd.DataFrame) -> go.Figure:
    """Predicted edge vs actual win rate — calibration plot."""
    if rounds.empty or "edge" not in rounds.columns:
        return go.Figure()

    rounds = rounds.copy()
    rounds["actual_win"] = (rounds["resolved_direction"] == "Up").astype(int)
    # For Down predictions, invert
    # edge > 0 means we predicted Up, edge < 0 means Down
    rounds["predicted_win"] = (rounds["our_p_up"] > 0.5).astype(int)
    rounds["correct"] = (rounds["predicted_win"] == rounds["actual_win"]).astype(int)

    # Bucket by absolute edge
    rounds["abs_edge"] = rounds["edge"].abs()
    rounds["edge_bucket"] = pd.cut(rounds["abs_edge"], bins=10)

    cal = rounds.groupby("edge_bucket", observed=True).agg(
        win_rate=("correct", "mean"),
        count=("correct", "count"),
        avg_edge=("abs_edge", "mean"),
    ).dropna()

    fig = go.Figure()
    fig.add_trace(go.Scatter(
        x=cal["avg_edge"] * 100, y=cal["win_rate"] * 100,
        mode="markers+text", text=cal["count"].astype(str),
        textposition="top center",
        marker=dict(size=cal["count"].clip(upper=50) * 0.8 + 8, color=COLORS["cyan"]),
        name="Actual",
    ))
    # Perfect calibration line
    fig.add_trace(go.Scatter(
        x=[0, cal["avg_edge"].max() * 100],
        y=[50, 50 + cal["avg_edge"].max() * 100],
        mode="lines", line=dict(dash="dash", color=COLORS["gray"]),
        name="Perfect Calibration",
    ))
    fig.update_layout(
        title="Edge Calibration: Predicted Edge vs Actual Win Rate (bubble size = trade count)",
        xaxis_title="Predicted Edge (%)", yaxis_title="Actual Win Rate (%)",
        template="plotly_dark", height=450,
    )
    return fig


def chart_rolling_metrics(pos: pd.DataFrame, window: int = 30) -> go.Figure:
    """Rolling win rate and average P&L (N-trade window)."""
    if len(pos) < window:
        window = min(window, max(1, len(pos)))

    rolling_wr = pos["win"].rolling(window).mean() * 100
    rolling_pnl = pos["pnl"].rolling(window).mean()

    fig = make_subplots(specs=[[{"secondary_y": True}]])
    fig.add_trace(
        go.Scatter(
            x=pos["closed_at"], y=rolling_wr,
            name=f"Win Rate ({window}-trade)", mode="lines",
            line=dict(color=COLORS["cyan"], width=2),
        ),
        secondary_y=False,
    )
    fig.add_trace(
        go.Scatter(
            x=pos["closed_at"], y=rolling_pnl,
            name=f"Avg P&L ({window}-trade)", mode="lines",
            line=dict(color=COLORS["orange"], width=2),
        ),
        secondary_y=True,
    )
    fig.add_hline(y=50, line_dash="dash", line_color=COLORS["gray"],
                  opacity=0.5, secondary_y=False)
    fig.add_hline(y=0, line_dash="dash", line_color=COLORS["gray"],
                  opacity=0.5, secondary_y=True)
    fig.update_layout(
        title=f"Rolling Metrics ({window}-Trade Window)",
        yaxis_title="Win Rate %", yaxis2_title="Avg P&L ($)",
        template="plotly_dark", height=400,
    )
    return fig


def chart_trade_duration(pos: pd.DataFrame) -> go.Figure:
    """Trade duration distribution."""
    fig = go.Figure()
    for tf in sorted(pos["timeframe"].unique()):
        subset = pos[pos["timeframe"] == tf]
        fig.add_trace(go.Histogram(
            x=subset["duration_min"], name=tf,
            opacity=0.7, nbinsx=30,
        ))
    fig.update_layout(
        title="Trade Duration Distribution by Timeframe",
        xaxis_title="Duration (minutes)", yaxis_title="Count",
        barmode="overlay", template="plotly_dark", height=350,
    )
    return fig


def chart_signal_analysis(sig: pd.DataFrame) -> go.Figure:
    """Signal entry rate and edge distribution."""
    fig = make_subplots(rows=1, cols=2,
                        subplot_titles=["Signal Actions by Hour", "Edge Distribution: Entered vs Rejected"])

    # Action by hour
    for action in ["Entered", "Rejected", "Skipped"]:
        subset = sig[sig["action"] == action]
        hourly = subset.groupby("hour").size().reindex(range(24), fill_value=0)
        fig.add_trace(go.Bar(x=hourly.index, y=hourly.values, name=action, opacity=0.8), row=1, col=1)

    # Edge distribution
    entered = sig[sig["action"] == "Entered"]["edge"]
    rejected = sig[sig["action"] == "Rejected"]["edge"]
    if not entered.empty:
        fig.add_trace(go.Histogram(x=entered, name="Entered", opacity=0.7,
                                   marker_color=COLORS["green"], nbinsx=30), row=1, col=2)
    if not rejected.empty:
        fig.add_trace(go.Histogram(x=rejected, name="Rejected", opacity=0.7,
                                   marker_color=COLORS["red"], nbinsx=30), row=1, col=2)

    fig.update_layout(
        title="Signal Analysis", template="plotly_dark", height=380,
        barmode="stack",
    )
    fig.update_xaxes(title_text="Hour (UTC)", dtick=1, row=1, col=1)
    fig.update_xaxes(title_text="Edge", row=1, col=2)
    return fig


def chart_market_volume(candles: pd.DataFrame) -> go.Figure:
    """Market volume by hour-of-day per asset (from data-hub candles)."""
    if candles.empty:
        return go.Figure()

    # Use 1h candles for clearest hourly pattern
    hourly = candles[candles["interval"] == "1h"] if "1h" in candles["interval"].values else candles
    vol_by_hour = hourly.groupby(["asset", "hour"])["volume"].mean().reset_index()

    fig = go.Figure()
    for asset in sorted(vol_by_hour["asset"].unique()):
        subset = vol_by_hour[vol_by_hour["asset"] == asset]
        fig.add_trace(go.Bar(
            x=subset["hour"], y=subset["volume"],
            name=asset.upper(),
            marker_color=COLORS.get(asset.upper(), COLORS["blue"]),
            opacity=0.8,
        ))
    fig.update_layout(
        title="Average Market Volume by Hour (UTC) — Exchange Data",
        xaxis_title="Hour (UTC)", yaxis_title="Volume",
        xaxis=dict(dtick=1), barmode="group",
        template="plotly_dark", height=400,
    )
    return fig


def chart_market_volatility(candles: pd.DataFrame) -> go.Figure:
    """Realized volatility by hour-of-day per asset."""
    if candles.empty:
        return go.Figure()

    c = candles[candles["interval"] == "1h"].copy() if "interval" in candles.columns and "1h" in candles["interval"].values else candles.copy()
    c["log_return"] = np.log(c["close"] / c["open"])
    vol_by_hour = c.groupby(["asset", "hour"])["log_return"].std().reset_index()
    vol_by_hour.columns = ["asset", "hour", "volatility"]

    fig = go.Figure()
    for asset in sorted(vol_by_hour["asset"].unique()):
        subset = vol_by_hour[vol_by_hour["asset"] == asset]
        fig.add_trace(go.Scatter(
            x=subset["hour"], y=subset["volatility"] * 100,
            name=asset.upper(), mode="lines+markers",
            line=dict(color=COLORS.get(asset.upper(), COLORS["blue"]), width=2),
        ))
    fig.update_layout(
        title="Realized Volatility by Hour (UTC) — Identifies High/Low Activity Periods",
        xaxis_title="Hour (UTC)", yaxis_title="Volatility (%)",
        xaxis=dict(dtick=1),
        template="plotly_dark", height=400,
    )
    return fig


# ── Fleet Comparison ──────────────────────────────────────────────────────

def chart_fleet_equity(fleet_data: dict[str, pd.DataFrame]) -> go.Figure:
    """Compare equity curves across multiple bots."""
    fig = go.Figure()
    bot_colors = [COLORS["blue"], COLORS["orange"], COLORS["green"],
                  COLORS["purple"], COLORS["cyan"], COLORS["red"]]
    for i, (name, pos) in enumerate(fleet_data.items()):
        if pos.empty:
            continue
        fig.add_trace(go.Scatter(
            x=pos["closed_at"], y=pos["cum_pnl"],
            name=name, mode="lines",
            line=dict(color=bot_colors[i % len(bot_colors)], width=2),
        ))
    fig.update_layout(
        title="Fleet Equity Curves Comparison",
        xaxis_title="Time", yaxis_title="Cumulative P&L ($)",
        template="plotly_dark", height=450,
    )
    return fig


def chart_fleet_summary(fleet_data: dict[str, pd.DataFrame]) -> go.Figure:
    """Fleet comparison summary: P&L, win rate, Sharpe, trade count."""
    rows = []
    for name, pos in fleet_data.items():
        if pos.empty:
            continue
        pnl_series = pos["pnl"]
        rows.append({
            "bot": name,
            "total_pnl": pnl_series.sum(),
            "trades": len(pos),
            "win_rate": pos["win"].mean() * 100,
            "avg_pnl": pnl_series.mean(),
            "sharpe": pnl_series.mean() / pnl_series.std() if pnl_series.std() > 0 else 0,
            "max_dd": (pos["cum_pnl"] - pos["cum_pnl"].cummax()).min(),
        })
    if not rows:
        return go.Figure()

    summary = pd.DataFrame(rows)
    fig = make_subplots(rows=1, cols=4,
                        subplot_titles=["Total P&L ($)", "Win Rate %", "Sharpe/Trade", "Max Drawdown ($)"])
    fig.add_trace(go.Bar(x=summary["bot"], y=summary["total_pnl"],
                         marker_color=[COLORS["green"] if v >= 0 else COLORS["red"] for v in summary["total_pnl"]],
                         showlegend=False), row=1, col=1)
    fig.add_trace(go.Bar(x=summary["bot"], y=summary["win_rate"],
                         marker_color=COLORS["cyan"], showlegend=False), row=1, col=2)
    fig.add_trace(go.Bar(x=summary["bot"], y=summary["sharpe"],
                         marker_color=COLORS["purple"], showlegend=False), row=1, col=3)
    fig.add_trace(go.Bar(x=summary["bot"], y=summary["max_dd"],
                         marker_color=COLORS["red"], showlegend=False), row=1, col=4)
    fig.update_layout(title="Fleet Performance Summary", template="plotly_dark", height=350)
    return fig


# ── Summary Stats ─────────────────────────────────────────────────────────

def compute_summary(pos: pd.DataFrame) -> dict:
    if pos.empty:
        return {}
    pnl = pos["pnl"]
    wins = pos[pos["pnl"] > 0]
    losses = pos[pos["pnl"] <= 0]
    return {
        "total_trades": len(pos),
        "total_pnl": f"${pnl.sum():.2f}",
        "win_rate": f"{pos['win'].mean() * 100:.1f}%",
        "avg_win": f"${wins['pnl'].mean():.2f}" if not wins.empty else "$0",
        "avg_loss": f"${losses['pnl'].mean():.2f}" if not losses.empty else "$0",
        "profit_factor": f"{abs(wins['pnl'].sum() / losses['pnl'].sum()):.2f}" if losses['pnl'].sum() != 0 else "∞",
        "sharpe_per_trade": f"{pnl.mean() / pnl.std():.3f}" if pnl.std() > 0 else "0",
        "max_drawdown": f"${(pos['cum_pnl'] - pos['cum_pnl'].cummax()).min():.2f}",
        "best_trade": f"${pnl.max():.2f}",
        "worst_trade": f"${pnl.min():.2f}",
        "avg_duration": f"{pos['duration_min'].mean():.1f} min",
        "first_trade": pos["opened_at"].min().strftime("%Y-%m-%d %H:%M"),
        "last_trade": pos["closed_at"].max().strftime("%Y-%m-%d %H:%M"),
    }


# ── HTML Report ───────────────────────────────────────────────────────────

def build_html_report(
    charts: list[go.Figure],
    summary: dict,
    bot_name: str,
    timestamp: str,
) -> str:
    chart_divs = ""
    for i, fig in enumerate(charts):
        chart_divs += f'<div class="chart-container">{pio.to_html(fig, full_html=False, include_plotlyjs=(i == 0))}</div>\n'

    summary_html = ""
    if summary:
        summary_html = '<div class="summary-grid">'
        for k, v in summary.items():
            label = k.replace("_", " ").title()
            summary_html += f'<div class="stat"><span class="label">{label}</span><span class="value">{v}</span></div>'
        summary_html += "</div>"

    return f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<title>Polymarket Bot Analytics — {bot_name}</title>
<style>
  * {{ margin: 0; padding: 0; box-sizing: border-box; }}
  body {{ font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
         background: #0f1117; color: #e5e7eb; padding: 24px; }}
  h1 {{ font-size: 28px; margin-bottom: 8px; }}
  .meta {{ color: #9ca3af; margin-bottom: 24px; font-size: 14px; }}
  .summary-grid {{
    display: grid; grid-template-columns: repeat(auto-fill, minmax(180px, 1fr));
    gap: 12px; margin-bottom: 32px;
  }}
  .stat {{
    background: #1f2937; border-radius: 8px; padding: 16px;
    display: flex; flex-direction: column;
  }}
  .stat .label {{ font-size: 12px; color: #9ca3af; text-transform: uppercase; margin-bottom: 4px; }}
  .stat .value {{ font-size: 20px; font-weight: 600; }}
  .chart-container {{ margin-bottom: 24px; background: #1f2937; border-radius: 8px; padding: 8px; }}
  .section-title {{ font-size: 20px; margin: 32px 0 16px; border-bottom: 1px solid #374151; padding-bottom: 8px; }}
</style>
</head>
<body>
<h1>Polymarket Bot Analytics — {bot_name}</h1>
<p class="meta">Generated: {timestamp} | v1.0.0</p>

<h2 class="section-title">Summary</h2>
{summary_html}

{chart_divs}

</body>
</html>"""


# ── Main ──────────────────────────────────────────────────────────────────

def generate_single_report(db_path: str, datahub_path: str | None, output_dir: str) -> str:
    bot_name = Path(db_path).stem
    print(f"Loading data from {db_path}...")

    pos = load_positions(db_path)
    sig = load_signals(db_path)
    rounds = load_rounds(db_path)
    metrics = load_metrics(db_path)

    if pos.empty:
        print(f"  No positions found in {db_path}, skipping.")
        return ""

    print(f"  {len(pos)} positions, {len(sig)} signals, {len(rounds)} rounds")

    charts = []

    # Section 1: Temporal patterns
    charts.append(chart_pnl_by_hour(pos))
    charts.append(chart_trade_volume_heatmap(pos))
    charts.append(chart_pnl_heatmap(pos))

    # Section 2: Equity & risk
    charts.append(chart_equity_curve(pos))
    charts.append(chart_rolling_metrics(pos))

    # Section 3: Performance attribution
    charts.append(chart_performance_by_asset(pos))
    charts.append(chart_performance_by_timeframe(pos))
    charts.append(chart_performance_by_direction(pos))
    charts.append(chart_exit_reasons(pos))
    charts.append(chart_trade_duration(pos))

    # Section 4: Signal analysis
    if not sig.empty:
        charts.append(chart_signal_analysis(sig))

    # Section 5: Edge calibration
    if not rounds.empty:
        charts.append(chart_edge_calibration(rounds))

    # Section 6: Market context (if datahub available)
    if datahub_path and os.path.exists(datahub_path):
        candles = load_candles(datahub_path)
        if not candles.empty:
            charts.append(chart_market_volume(candles))
            charts.append(chart_market_volatility(candles))

    summary = compute_summary(pos)
    timestamp = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC")

    html = build_html_report(charts, summary, bot_name, timestamp)

    os.makedirs(output_dir, exist_ok=True)
    out_path = os.path.join(output_dir, f"{bot_name}_report.html")
    with open(out_path, "w") as f:
        f.write(html)

    print(f"  Report saved: {out_path}")
    return out_path


def generate_fleet_report(db_paths: list[str], output_dir: str) -> str:
    fleet_data = {}
    for db_path in db_paths:
        name = Path(db_path).stem
        pos = load_positions(db_path)
        if not pos.empty:
            fleet_data[name] = pos
            print(f"  {name}: {len(pos)} positions, P&L ${pos['pnl'].sum():.2f}")

    if not fleet_data:
        print("No data found in any database.")
        return ""

    charts = [
        chart_fleet_equity(fleet_data),
        chart_fleet_summary(fleet_data),
    ]

    # Add per-bot mini equity curves
    for name, pos in fleet_data.items():
        charts.append(chart_pnl_by_hour(pos))
        charts[-1].update_layout(title=f"{name}: P&L by Hour")

    timestamp = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC")
    html = build_html_report(charts, {}, "Fleet Comparison", timestamp)

    os.makedirs(output_dir, exist_ok=True)
    out_path = os.path.join(output_dir, "fleet_report.html")
    with open(out_path, "w") as f:
        f.write(html)

    print(f"Fleet report saved: {out_path}")
    return out_path


def main():
    parser = argparse.ArgumentParser(description="Polymarket Bot Analytics Report")
    parser.add_argument("--db", nargs="+", help="Bot database path(s)")
    parser.add_argument("--datahub", help="Data-hub database path (for market context)")
    parser.add_argument("--fleet", action="store_true", help="Generate fleet comparison report")
    parser.add_argument("--output", default="reports", help="Output directory (default: reports/)")
    parser.add_argument("--all", action="store_true", help="Auto-detect all DBs in data/")
    args = parser.parse_args()

    db_paths = args.db or []

    if args.all or not db_paths:
        data_dir = "data"
        if os.path.isdir(data_dir):
            db_paths = sorted(
                str(p) for p in Path(data_dir).glob("polybot-*.db")
            )
            print(f"Auto-detected {len(db_paths)} bot databases in {data_dir}/")
        if not db_paths:
            print("No databases found. Use --db <path> or place .db files in data/")
            sys.exit(1)

    datahub = args.datahub
    if not datahub:
        candidate = "data/datahub.db"
        if os.path.exists(candidate):
            datahub = candidate

    if args.fleet and len(db_paths) > 1:
        generate_fleet_report(db_paths, args.output)

    for db_path in db_paths:
        generate_single_report(db_path, datahub, args.output)

    print(f"\nDone! Open reports in browser: open {args.output}/*.html")


if __name__ == "__main__":
    main()

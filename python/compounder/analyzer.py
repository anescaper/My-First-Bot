"""
Post-trade analysis and model improvement.
Logs trades, extracts loss patterns, updates model weights.
"""

from dataclasses import dataclass, field
from typing import List, Optional
from datetime import datetime
import json


@dataclass
class TradeRecord:
    trade_id: str
    market_id: str
    question: str
    direction: str
    entry_price: float
    exit_price: float
    size: float
    pnl: float
    edge_at_entry: float
    z_score_at_entry: float
    signal_source: str
    timestamp: datetime
    outcome: Optional[bool] = None  # True if market resolved in our favor


@dataclass
class LossPattern:
    pattern_type: str  # "overconfidence", "stale_signal", "liquidity_trap", etc.
    frequency: int
    avg_loss: float
    prevention_rule: str


class Compounder:
    """Iterative learning from trade outcomes."""

    def __init__(self, log_path: str = "trades.jsonl"):
        self.log_path = log_path
        self.trades: List[TradeRecord] = []

    def log_trade(self, trade: TradeRecord):
        """Append trade to log file."""
        self.trades.append(trade)
        with open(self.log_path, "a") as f:
            f.write(json.dumps({
                "trade_id": trade.trade_id,
                "market_id": trade.market_id,
                "question": trade.question,
                "direction": trade.direction,
                "entry_price": trade.entry_price,
                "exit_price": trade.exit_price,
                "size": trade.size,
                "pnl": trade.pnl,
                "edge_at_entry": trade.edge_at_entry,
                "z_score_at_entry": trade.z_score_at_entry,
                "timestamp": trade.timestamp.isoformat(),
            }) + "\n")

    def analyze_losses(self) -> List[LossPattern]:
        """Extract loss patterns from trade history."""
        losses = [t for t in self.trades if t.pnl < 0]
        if not losses:
            return []

        patterns = []
        # Pattern: Overconfidence (high edge but loss)
        overconfident = [t for t in losses if t.edge_at_entry > 0.10]
        if overconfident:
            patterns.append(LossPattern(
                pattern_type="overconfidence",
                frequency=len(overconfident),
                avg_loss=sum(t.pnl for t in overconfident) / len(overconfident),
                prevention_rule="Cap edge confidence at 0.10, require multi-source confirmation",
            ))

        return patterns

    def summary(self) -> dict:
        """Generate performance summary."""
        if not self.trades:
            return {"total_trades": 0}

        wins = [t for t in self.trades if t.pnl > 0]
        losses = [t for t in self.trades if t.pnl < 0]
        total_pnl = sum(t.pnl for t in self.trades)

        return {
            "total_trades": len(self.trades),
            "wins": len(wins),
            "losses": len(losses),
            "win_rate": len(wins) / len(self.trades) if self.trades else 0,
            "total_pnl": total_pnl,
            "avg_pnl": total_pnl / len(self.trades),
            "best_trade": max(t.pnl for t in self.trades),
            "worst_trade": min(t.pnl for t in self.trades),
        }


if __name__ == "__main__":
    c = Compounder()
    print(c.summary())

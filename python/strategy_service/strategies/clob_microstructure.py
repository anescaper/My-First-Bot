"""CLOB microstructure strategy — uses order book, trade flow, and cross-exchange signals.

Unlike displacement z-score strategies (GARCH-T, Monte Carlo, etc.) which all compute
the same signal from Binance spot candles, this strategy uses CLOB-native data:

1. Order Book Imbalance (OBI): bid/ask depth asymmetry on the Up token.
   When bids >> asks, liquidity providers expect p_up to rise.
2. Trade Flow Imbalance (TFI): buyer-initiated vs seller-initiated fills.
   Informed traders tend to be aggressive takers — their direction predicts.
3. Coinbase Premium: cross-exchange price divergence (US institutional flow).
4. Token Momentum: dp_up/dt from CLOB price changes.
"""

from typing import Optional

import numpy as np

from strategy_service.models import PredictionRequest, StrategyResult
from strategy_service.strategies.base import Strategy
from strategy_service.strategies.registry import register_strategy


@register_strategy
class ClobMicrostructureStrategy(Strategy):
    """CLOB-native multi-signal strategy using order book, trade flow, and cross-exchange data.

    This is the only strategy that uses CLOB-specific data instead of Binance candles.
    It combines four independent microstructure signals, each weighted by empirical
    informativeness:
    - TFI (3.0): trade flow imbalance -- strongest single CLOB signal.
    - OBI (2.5): order book imbalance -- strong but can be spoofed.
    - Token Momentum (2.0): dp_up/dt from CLOB price changes.
    - Coinbase Premium (1.5): cross-exchange signal, informative but noisy.

    Hardcoded signal weights reflect empirical testing on Polymarket crypto markets.
    """

    def name(self) -> str:
        return "clob_microstructure"

    def required_data(self) -> list[str]:
        return ["order_book"]

    def weight(self) -> float:
        return 3.0

    def min_data_points(self) -> int:
        return 0  # doesn't need candles

    def predict(self, ctx: PredictionRequest) -> Optional[StrategyResult]:
        signals = []
        weights = []
        meta = {}

        # --- Signal 1: Order Book Imbalance (OBI) ---
        obi = self._order_book_imbalance(ctx)
        if obi is not None:
            signals.append(obi)
            weights.append(2.5)
            meta["obi"] = round(obi, 4)

        # --- Signal 2: Trade Flow Imbalance (TFI) ---
        tfi = self._trade_flow_imbalance(ctx)
        if tfi is not None:
            signals.append(tfi)
            weights.append(3.0)
            meta["tfi"] = round(tfi, 4)

        # --- Signal 3: Coinbase Premium ---
        cbp = self._coinbase_premium_signal(ctx)
        if cbp is not None:
            signals.append(cbp)
            weights.append(1.5)
            meta["cbp"] = round(cbp, 4)

        # --- Signal 4: Token Momentum ---
        tmom = self._token_momentum(ctx)
        if tmom is not None:
            signals.append(tmom)
            weights.append(2.0)
            meta["tmom"] = round(tmom, 4)

        if not signals:
            return None

        # Weighted combination → composite signal in [-1, 1]
        total_weight = sum(weights)
        composite = sum(s * w for s, w in zip(signals, weights)) / total_weight
        composite = max(-1.0, min(1.0, composite))

        # With only 1 signal, require strong conviction
        if len(signals) == 1:
            if abs(composite) < 0.3:
                return None
        else:
            # Direction agreement: fraction of signals that agree with composite
            agree_count = sum(1 for s in signals if s * composite > 0)
            agreement = agree_count / len(signals)
            if agreement < 0.5:
                return None

        agreement = 1.0 if len(signals) == 1 else sum(1 for s in signals if s * composite > 0) / len(signals)

        # Map composite to p_up: sigmoid-like mapping
        # composite > 0 → p_up > 0.5 (bullish)
        p_up = 0.5 + 0.45 * composite

        # Confidence scales with signal strength and agreement
        confidence = min(abs(composite) * agreement * 1.2, 0.50)

        meta["composite"] = round(composite, 4)
        meta["n_signals"] = len(signals)
        meta["agreement"] = round(agreement, 2)

        return StrategyResult(p_up=p_up, confidence=confidence, hold_to_resolution=True, max_entry_price=0.80, meta=meta)

    def _order_book_imbalance(self, ctx: PredictionRequest) -> Optional[float]:
        """Compute bid-ask volume imbalance on the Up token side.

        OBI = (bid_vol - ask_vol) / (bid_vol + ask_vol), range [-1, 1].
        Positive OBI = more bids = buyers waiting to buy Up token = bullish.

        Minimum total volume threshold is 3 shares (lowered from earlier versions
        to fire with sparse early-round data on Polymarket's thin books).

        Args:
            ctx: Prediction context with order_book field.

        Returns:
            OBI signal in [-1, 1], or None if book is empty/too thin.
        """
        if not ctx.order_book:
            return None

        bids = ctx.order_book.get("bids_up", [])
        asks = ctx.order_book.get("asks_up", [])
        if not bids and not asks:
            return None

        bid_vol = sum(size for _, size in bids)
        ask_vol = sum(size for _, size in asks)

        total = bid_vol + ask_vol
        if total < 3:  # lowered: fire with sparse early data
            return None

        # OBI in [-1, 1]: positive = more bids = bullish for Up token
        obi = (bid_vol - ask_vol) / total
        return obi

    def _trade_flow_imbalance(self, ctx: PredictionRequest) -> Optional[float]:
        """Compute buyer vs seller initiated flow from trade tapes.

        TFI = (up_buy_vol - up_sell_vol) / total_vol, range [-1, 1].
        Positive TFI = net buying of Up tokens = bullish informed flow.

        Taker-initiated fills (is_buyer_maker=False) are treated as informed flow.
        Down token buys are interpreted as Up token sells (inverse relationship).

        Args:
            ctx: Prediction context with trade_tapes field.

        Returns:
            TFI signal in [-1, 1], or None if no trades available.
        """
        if not ctx.trade_tapes or len(ctx.trade_tapes) < 1:
            return None

        # Separate Up-token and Down-token fills
        up_buy_vol = 0.0
        up_sell_vol = 0.0

        for fill in ctx.trade_tapes:
            if fill.side == "Up":
                if fill.is_buyer_maker:
                    up_sell_vol += fill.size
                else:
                    up_buy_vol += fill.size
            elif fill.side == "Down":
                if fill.is_buyer_maker:
                    up_buy_vol += fill.size
                else:
                    up_sell_vol += fill.size

        total = up_buy_vol + up_sell_vol
        if total < 1:  # lowered: even a single fill is informative early
            return None

        # TFI in [-1, 1]: positive = net buying = bullish
        tfi = (up_buy_vol - up_sell_vol) / total
        return tfi

    def _coinbase_premium_signal(self, ctx: PredictionRequest) -> Optional[float]:
        """Cross-exchange premium as directional signal.

        Coinbase premium = (coinbase_price - binance_price) / binance_price.
        Positive premium indicates US institutional buying pressure (bullish).

        The * 500 scaling factor converts typical premiums (0.0001-0.002) to
        signal range [-1, 1]. A 0.1% premium -> signal of ~0.5.

        Args:
            ctx: Prediction context with coinbase_premium field.

        Returns:
            Signal in [-1, 1], or None if premium is negligible (<0.01%).
        """
        premium = ctx.coinbase_premium
        if abs(premium) < 0.0001:  # no meaningful premium
            return None

        # Clamp to [-1, 1]: premium of 0.1% → signal of ~0.5
        signal = max(-1.0, min(1.0, premium * 500))
        return signal

    def _token_momentum(self, ctx: PredictionRequest) -> Optional[float]:
        """Momentum of p_up from CLOB token trajectory.

        Computes dp_up/dt (token price velocity) from the last 10 trajectory ticks.
        The * 100 scaling converts typical velocities (0.001-0.01 per second) to
        signal range [-1, 1].

        Args:
            ctx: Prediction context with token_trajectory field.

        Returns:
            Signal in [-1, 1], or None if insufficient trajectory data (<3 ticks).
        """
        traj = ctx.token_trajectory
        if len(traj) < 3:  # lowered from 5
            return None

        recent = traj[-10:]
        p_ups = [t.p_up for t in recent]
        times = [t.t for t in recent]

        dt = times[-1] - times[0]
        if dt <= 0:
            return None

        # Velocity: dp_up / dt (change per second)
        dp = p_ups[-1] - p_ups[0]
        velocity = dp / dt

        # Normalize: typical velocity is 0.001-0.01 per second
        signal = max(-1.0, min(1.0, velocity * 100))
        return signal

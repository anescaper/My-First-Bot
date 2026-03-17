"""Agreement-based ensemble combiner with weighted prediction combination.

Two modes of operation:
1. **Solo mode**: when a profile has exactly 1 strategy, the strategy's output
   passes through directly with full internal meta exposed for tracking. This
   avoids the overhead of voting/agreement when there is only one voter.

2. **Ensemble mode**: when a profile has 2+ strategies, predictions are combined
   using confidence-weighted averaging with an agreement multiplier. Strategies
   that agree on direction boost overall confidence; disagreement reduces it.

Hardcoded thresholds:
- 0.15 minimum confidence gate: below this, signal is too weak to trade.
  Chosen empirically -- signals below 0.15 confidence had negative expected
  value after accounting for Polymarket fees (2% per trade).
- Timeframe multiplier (0.75 + 0.4 * tf_agree): boosts confidence when
  cross-timeframe signals agree with the ensemble direction. The 0.75 base
  ensures some signal survives even without TF agreement; 0.4 scale gives
  a max 1.15x multiplier at full agreement.
"""

import time

from strategy_service.models import (
    ComponentResult,
    PredictionRequest,
    PredictionResponse,
)


def combine_predictions(
    results: list[tuple[str, float, "StrategyResult"]],
    req: PredictionRequest,
) -> PredictionResponse:
    """Combine strategy predictions using agreement-based weighting.

    The combination formula:
        weighted_p_up = sum(weight * confidence * p_up) / sum(weight * confidence)
        agreement = max(up_voters, down_voters) / total_voters
        ensemble_confidence = avg_weighted_confidence * agreement

    Args:
        results: list of (strategy_name, weight, StrategyResult) tuples.
            Weight comes from the strategy's weight() method.
        req: original PredictionRequest for context (market prices, TF intel).

    Returns:
        PredictionResponse with combined prediction. Direction is determined by
        comparing weighted_p_up to market_price_up (not to 0.5), because the
        bot needs edge OVER the market price, not just directional conviction.
    """
    if not results:
        return PredictionResponse(
            p_up=0.5, confidence=0.0, edge=0.0,
            direction="Skip", components=[], meta={},
        )

    # --- Solo mode: single strategy pass-through with full trace ---
    if len(results) == 1:
        return _solo_prediction(results[0], req)

    # --- Ensemble mode: weighted agreement ---
    total_weight = sum(w * r.confidence for _, w, r in results)
    if total_weight < 1e-10:
        return PredictionResponse(
            p_up=0.5, confidence=0.0, edge=0.0,
            direction="Skip", components=[], meta={},
        )

    weighted_p_up = sum(w * r.confidence * r.p_up for _, w, r in results) / total_weight

    up_count = sum(1 for _, _, r in results if r.p_up > 0.5)
    down_count = sum(1 for _, _, r in results if r.p_up < 0.5)
    agreement = max(up_count, down_count) / len(results)

    avg_confidence = sum(w * r.confidence for _, w, r in results) / sum(w for _, w, _ in results)
    ensemble_confidence = avg_confidence * agreement

    if ensemble_confidence < 0.15:
        return PredictionResponse(
            p_up=weighted_p_up, confidence=ensemble_confidence, edge=0.0,
            direction="Skip",
            components=[ComponentResult(name=n, p_up=r.p_up, confidence=r.confidence) for n, _, r in results],
            meta={"mode": "ensemble", "agreement": agreement, "gate": "below_0.15"},
        )

    market_price_up = req.round.price_up

    if req.timeframe_intel and abs(req.timeframe_intel.agreement_score) > 0.05:
        is_up = weighted_p_up > market_price_up
        tf_agree = req.timeframe_intel.direction_agreement_up if is_up else req.timeframe_intel.direction_agreement_down
        tf_multiplier = 0.75 + 0.4 * tf_agree
        ensemble_confidence *= tf_multiplier

    edge = ensemble_confidence * abs(weighted_p_up - market_price_up)

    if weighted_p_up > market_price_up:
        direction = "Up"
    elif weighted_p_up < market_price_up:
        direction = "Down"
    else:
        direction = "Skip"

    # Aggregate pipeline hints: hold_to_resolution if ANY strategy says so,
    # max_entry_price = min of all strategies that set it
    any_hold = any(r.hold_to_resolution for _, _, r in results)
    entry_prices = [r.max_entry_price for _, _, r in results if r.max_entry_price is not None]
    min_entry = min(entry_prices) if entry_prices else None

    ensemble_meta: dict = {
        "mode": "ensemble", "agreement": agreement, "n_strategies": len(results),
        "strategy_meta": {
            "hold_to_resolution": any_hold,
        },
    }
    if min_entry is not None:
        ensemble_meta["strategy_meta"]["max_entry_price"] = min_entry

    return PredictionResponse(
        p_up=weighted_p_up,
        confidence=ensemble_confidence,
        edge=edge,
        direction=direction,
        components=[ComponentResult(name=n, p_up=r.p_up, confidence=r.confidence) for n, _, r in results],
        meta=ensemble_meta,
    )


def _solo_prediction(
    entry: tuple[str, float, "StrategyResult"],
    req: PredictionRequest,
) -> PredictionResponse:
    """Direct pass-through for a single strategy -- no voting, full trace.

    Used when a profile maps to exactly one strategy (e.g. 'garch-t' -> [garch_t]).
    The strategy's p_up, confidence, and pipeline hints pass through directly.
    Cross-timeframe modulation is still applied as a sanity check.

    Args:
        entry: (strategy_name, weight, StrategyResult) tuple.
        req: original PredictionRequest for context.

    Returns:
        PredictionResponse with the single strategy's prediction and full
        strategy_meta for debugging.
    """
    name, _weight, result = entry

    confidence = result.confidence

    # Minimum confidence gate (same threshold as ensemble)
    if confidence < 0.15:
        return PredictionResponse(
            p_up=result.p_up, confidence=confidence, edge=0.0,
            direction="Skip",
            components=[ComponentResult(name=name, p_up=result.p_up, confidence=confidence)],
            meta={
                "mode": "solo",
                "strategy": name,
                "gate": "below_0.15",
                "strategy_meta": result.meta or {},
            },
        )

    market_price_up = req.round.price_up

    # Cross-timeframe boost/dampening (still useful as a sanity check)
    if req.timeframe_intel and abs(req.timeframe_intel.agreement_score) > 0.05:
        is_up = result.p_up > market_price_up
        tf_agree = req.timeframe_intel.direction_agreement_up if is_up else req.timeframe_intel.direction_agreement_down
        tf_multiplier = 0.75 + 0.4 * tf_agree
        confidence *= tf_multiplier

    edge = confidence * abs(result.p_up - market_price_up)

    if result.p_up > market_price_up:
        direction = "Up"
    elif result.p_up < market_price_up:
        direction = "Down"
    else:
        direction = "Skip"

    # Merge pipeline hints into strategy_meta for Rust to parse
    strategy_meta = dict(result.meta) if result.meta else {}
    strategy_meta["hold_to_resolution"] = result.hold_to_resolution
    if result.min_progress is not None:
        strategy_meta["min_progress"] = result.min_progress
    if result.max_progress is not None:
        strategy_meta["max_progress"] = result.max_progress
    if result.max_entry_price is not None:
        strategy_meta["max_entry_price"] = result.max_entry_price

    return PredictionResponse(
        p_up=result.p_up,
        confidence=confidence,
        edge=edge,
        direction=direction,
        components=[ComponentResult(name=name, p_up=result.p_up, confidence=result.confidence)],
        meta={
            "mode": "solo",
            "strategy": name,
            "strategy_meta": strategy_meta,
            "timestamp": time.time(),
        },
    )

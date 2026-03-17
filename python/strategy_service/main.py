"""FastAPI strategy service -- the brain of the bot fleet.

This service receives prediction requests from the Rust bot pipeline over HTTP
and returns probability estimates (p_up) for binary outcome Polymarket markets.

Architecture:
    - The Rust bot calls POST /predict each cycle with market data.
    - The request is routed to all strategies registered for the requested profile.
    - Each strategy produces an independent prediction (or abstains).
    - The ensemble combiner produces a final weighted prediction.

The service runs on port 8100 (hardcoded in Docker Compose) and is called by
the Rust bot via STRATEGY_SERVICE_URL environment variable.
"""

import logging

from fastapi import FastAPI

from strategy_service.models import PredictionRequest, PredictionResponse
from strategy_service.strategies.registry import StrategyRegistry
from strategy_service.ensemble import combine_predictions

# Import strategy modules to trigger @register_strategy decorators.
# Each strategy file uses the @register_strategy decorator at module level,
# which registers the strategy instance into StrategyRegistry on import.
import strategy_service.strategies  # noqa: F401

logger = logging.getLogger(__name__)

app = FastAPI(title="Polymarket Strategy Service", version="0.1.0")


@app.post("/predict")
async def predict(req: PredictionRequest) -> PredictionResponse:
    """Run all strategies for the requested profile and combine predictions.

    This is the primary endpoint consumed by the Rust bot pipeline. Each bot
    profile (e.g. "garch-t", "fair-value") maps to a specific set of strategies
    defined in PROFILE_STRATEGIES (registry.py).

    Args:
        req: PredictionRequest containing round info, market data, and profile name.

    Returns:
        PredictionResponse with combined p_up, confidence, edge, and direction.
        Returns a neutral (p_up=0.5, confidence=0.0) response if no strategies
        are registered for the profile or all strategies abstain.
    """
    strategies = StrategyRegistry.for_profile(req.strategy_profile)

    if not strategies:
        logger.warning(f"No strategies registered for profile '{req.strategy_profile}'")
        return PredictionResponse(
            p_up=0.5, confidence=0.0, edge=0.0,
            direction="Skip", components=[], meta={"error": "no_strategies"},
        )

    results = []
    for s in strategies:
        try:
            result = s.predict(req)
            if result is not None:
                results.append((s.name(), s.weight(), result))
        except Exception as e:
            logger.warning(f"Strategy {s.name()} failed: {e}")

    return combine_predictions(results, req)


@app.get("/strategies")
async def list_strategies():
    """List all registered strategies with their metadata.

    Returns a dict keyed by strategy name, with required_data, weight, and
    min_data_points for each. Used by the frontend to display strategy info
    and by operators to verify strategy registration on startup.
    """
    return {
        name: {
            "required_data": s.required_data(),
            "weight": s.weight(),
            "min_data_points": s.min_data_points(),
        }
        for name, s in StrategyRegistry._strategies.items()
    }


@app.get("/health")
async def health():
    """Health check endpoint for Docker healthcheck and monitoring.

    Returns the number and names of registered strategies so operators can
    verify all expected strategies loaded successfully.
    """
    return {
        "status": "ok",
        "strategies_registered": len(StrategyRegistry._strategies),
        "strategy_names": StrategyRegistry.all_names(),
    }

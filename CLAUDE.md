# My-First-Bot

Polymarket crypto binary options trading bot with Brownian Bridge path-conditioned strategy.

## Architecture

**2 containers**: Rust binary (DataHub + pipeline) + Python FastAPI strategy service.

### Rust Workspace (7 crates)

- `scanner` — CLOB market scanner, crypto round discovery (BTC/ETH/SOL/XRP x 5m/15m/1h), multi-source price feed (Binance + Pyth)
- `risk` — Confidence-based sizing, per-asset risk gates, circuit breakers, composable risk gate chain
- `executor` — ECDSA signing (DevSigner + LiveSigner), CLOB order submission, paper trading
- `api` — REST API + WebSocket for monitoring (axum), SQLite persistence
- `bot` — Main binary: pipeline orchestrator
- `data` — Data client: fetches cycle snapshots from data-hub
- `data-hub` — Separate binary: market scanner, price feeds, round tracking, intel computation

### Python Strategy Service

- FastAPI on port 8100, strategies self-register via `@register_strategy`
- `python/strategy_service/` — main.py, strategies/, ensemble.py, models.py
- Primary strategy: **Brownian Bridge** (path-conditioned child round prediction)
- Secondary: **Hierarchical Cascade** (multi-timeframe agreement)
- Libraries: `arch` (GARCH), `scipy` (Student-t), `sklearn` (PCA)

## Strategy

The bot uses parent-child round relationships in Polymarket crypto binary markets:

- 15m parent contains 3 x 5m children
- 1h parent contains 4 x 15m children

**Key signal (Q6 research, 11,348 markets):**

- Parent UP + prior children [DOWN, DOWN] -> child 3 UP 100% (n=18)
- Parent DOWN + prior children [UP, UP] -> child 3 DOWN 100% (n=10)

**Signal ranking (strongest to weakest):**

1. Path conditioning: 100% (n=18)
2. 1h spread direction: 92-95% (n=146)
3. Hierarchical cascade (1h+15m agree): 77.8% (n=108)
4. 15m parent lock: 75% (n=1,108)
5. 5m spread direction: 76-78% (n=2,016)
6. Momentum: 56-57% (weak)

## Quick Commands

```bash
cargo build                    # Build all crates
cargo run --bin polybot        # Run the bot
cargo test                     # Run tests
docker compose up -d           # Run in Docker (bot + strategy service)
```

## API

Bot exposes REST API on port 4200:

- GET /health, /status, /config
- POST /start, /stop, /kill
- GET /positions, /signals, /metrics
- GET /crypto/positions, /crypto/rounds, /crypto/metrics, /crypto/strategies
- GET /history/positions, /history/signals, /history/rounds
- WS /stream

## Deployment

- **Docker**: `polybot` (Rust) + `strategy-service` (Python)
- **Communication**: `STRATEGY_SERVICE_URL=http://strategy-service:8100`
- **Mode**: Paper trading by default (BOT_MODE=paper)

## Conventions

- Error handling: `anyhow::Result<()>` for pipeline, `.unwrap_or_default()` for non-critical
- State: `Arc<AppState>` with `std::sync::RwLock` (NOT tokio RwLock)
- Naming: structs CamelCase, functions snake_case, crates polybot-\* (kebab)
- All public structs: `#[derive(Debug, Clone, Serialize, Deserialize)]`

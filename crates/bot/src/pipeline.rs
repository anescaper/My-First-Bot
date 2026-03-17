//! Main trading pipeline — the heart of the Polymarket bot.
//!
//! This module contains the `Pipeline` struct which orchestrates the entire trading loop:
//! 1. **Data ingestion** — fetches rounds, prices, and candles from the data-hub service
//! 2. **Prediction** — routes to either Rust strategies (baseline) or Python strategy service
//! 3. **Risk evaluation** — dual-layer: `RiskEngine` (Kelly criterion) + composable `RiskGateChain`
//! 4. **Order execution** — submits GTC orders via live CLOB executor or simulates via PaperExecutor
//! 5. **Position management** — monitors exits via stop-loss, round resolution, or hold-to-settlement
//! 6. **Metrics** — tracks PnL, win rate, Sharpe ratio, equity curve, and persists to SQLite
//!
//! The pipeline runs two sub-loops:
//! - **Crypto pipeline** (`run_crypto_cycle`): BTC/ETH/SOL/XRP Up/Down binary rounds (5m/15m/1h)
//! - **General pipeline** (`run_general_cycle`): non-crypto Polymarket event markets
//!
//! Designed for a multi-pipeline deployment where 8 strategy profiles share a single DataHub.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use anyhow::Result;
use polybot_scanner::clob::ClobClient;
use polybot_scanner::crypto::{Asset, CryptoRound};
use polybot_scanner::filter::FilterConfig;
use polybot_scanner::types::Market;
use polybot_risk::checks::RiskEngine;
use polybot_risk::types::{PositionRequest, Direction};
use polybot_risk::gate::GateSignal;
use polybot_risk::chain::RiskGateChain;
use polybot_data::{DataState, DataClient, RoundKey};
use polybot_api::state::{AppState, BotMode, PipelineMode, Signal, SignalAction, Position};
use polybot_executor::cost::CostModel;
use polybot_executor::executor::Executor;
use polybot_executor::order::{ClobOrderRequest, ClobOrderType, ClobSide, GtcOrderStatus};
use crate::strategies::StrategyComposer;
use crate::crypto_strategies::{CryptoComposer, CryptoStrategyConfig, PredictionContext, ComposedCryptoPrediction, CryptoPrediction};
use crate::crypto_strategies_v2::{QuantCryptoComposer, QuantProfile};
use crate::timeframe_intel::TimeframeIntel;

/// Maximum number of closed positions to keep in memory for the API.
/// Hardcoded at 50 to bound memory usage while providing enough history for the dashboard.
const MAX_CLOSED_HISTORY: usize = 50;

/// Close general-pipeline positions that haven't moved after this many minutes.
/// Hardcoded at 5 minutes — stale positions tie up risk budget without generating returns.
const STALE_POSITION_MINUTES: i64 = 5;

/// Minimum absolute price change to consider a position "still moving".
/// Hardcoded at 0.1% — below this, the market is effectively dead.
const STALE_MOVEMENT_THRESHOLD: f64 = 0.001;

/// Cancel GTC orders when round has fewer than this many seconds remaining.
/// Hardcoded at 60s — gives enough time for cancellation to propagate on-chain
/// while avoiding orders filling too close to round resolution.
const GTC_CANCEL_BEFORE_END_SECS: i64 = 60;

/// Policy for handling positions whose round disappears from the scanner.
///
/// This happens when a crypto round resolves and is no longer returned by the Gamma API.
/// In live mode, the on-chain settlement still happens — the position isn't lost.
/// In paper mode, auto-close is simpler since there's no real settlement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VanishedTradePolicy {
    /// Keep the position and wait for on-chain settlement (live mode default).
    /// The position is marked as "pending_settlement" and auto-cleared after 30 minutes.
    HoldToSettlement,
    /// Close immediately at last known price (paper mode default).
    AutoClose,
}

/// A GTC (Good-Til-Cancelled) order that has been placed on the CLOB book but not yet filled.
///
/// GTC orders sit on the order book until matched, cancelled, or the round ends.
/// This is preferred over FOK (Fill-Or-Kill) for hold-to-resolution strategies because
/// the order can wait for a counterparty, improving fill rates in illiquid markets.
///
/// The pipeline checks pending GTC orders each cycle: filled orders become positions,
/// and orders near round expiry are cancelled to avoid accidental fills at resolution.
#[derive(Debug, Clone)]
struct PendingGtcOrder {
    /// CLOB order ID returned by the exchange — used for status checks and cancellation
    order_id: String,
    /// Polymarket condition ID of the round — used to prevent duplicate entries
    condition_id: String,
    /// "Up" or "Down" — which side of the binary outcome we're buying
    direction: String,
    /// USD amount committed to this order
    size_usd: f64,
    /// Edge (signal strength) at time of submission — stored for position tracking
    entry_edge: f64,
    /// When the round ends — used to trigger pre-expiry cancellation
    round_end: chrono::DateTime<chrono::Utc>,
    /// When we submitted the order — used to log wait duration on fill
    submitted_at: chrono::DateTime<chrono::Utc>,
    /// Crypto asset (BTC, ETH, SOL, XRP)
    asset: Asset,
    /// Round timeframe (5m, 15m, 1h)
    timeframe: polybot_scanner::crypto::Timeframe,
    /// Fee rate in basis points — passed through to position on fill
    fee_rate_bps: u64,
    /// Whether this position should be held to on-chain resolution (no early SELL)
    hold_to_resolution: bool,
    /// The limit price submitted to the CLOB — logged for debugging
    limit_price: f64,
}

/// The main trading pipeline that drives the entire bot.
///
/// Pipeline is NOT Send/Sync — it runs on a single tokio task. All cross-task
/// communication goes through `Arc<AppState>` with `std::sync::RwLock`.
///
/// The pipeline has two operating modes:
/// - **Crypto**: trades BTC/ETH/SOL/XRP Up/Down binary rounds on adaptive 1-5s intervals
/// - **General**: trades non-crypto Polymarket event markets on 300s intervals
///
/// ## Position sizing
/// Uses a ladder system for aggressive profiles: bankroll doubles each round
/// until a cap is reached. Circuit breaker demotes to base sizing on large drawdowns.
///
/// ## Prediction routing
/// - `baseline` profile: Rust strategies only (CryptoComposer / QuantCryptoComposer)
/// - All other profiles: Python strategy service only (no Rust fallback)
pub struct Pipeline {
    /// CLOB HTTP client for fetching Polymarket market listings (general pipeline)
    scanner: ClobClient,
    /// Market filter configuration (min liquidity, min volume, etc.)
    filter: FilterConfig,
    /// Traditional risk engine: Kelly criterion sizing, exposure limits, edge thresholds
    risk: RiskEngine,
    /// Shared application state — accessed by API server and all pipelines
    state: Arc<AppState>,
    /// General-market strategy composer (6 strategies: mean reversion, arb, liquidity, etc.)
    composer: StrategyComposer,
    /// Crypto binary round strategy composer (5 Rust strategies: time decay, trend, vol regime, etc.)
    crypto_composer: CryptoComposer,
    /// Initial bankroll set at startup — used as base for position sizing and equity ratio
    bankroll: f64,
    /// Current equity: bankroll + cumulative PnL. Updated on every position close.
    current_equity: f64,
    /// Per-trade returns (pnl/size) — used to compute Sharpe ratio and win rate
    trade_returns: Vec<f64>,
    /// Optional quantitative strategy composer (GARCH, Hurst, Student-t based)
    /// Activated when STRATEGY_PROFILE is garch-t, hurst-hinf, or full-quant
    quant_composer: Option<QuantCryptoComposer>,
    /// Minimum edge to enter a crypto trade. Default 0.12 (12%).
    /// Hardcoded default is high because Polymarket crypto fees are ~2% per side.
    /// Configurable via CRYPTO_EDGE_THRESHOLD env var.
    crypto_edge_threshold: f64,
    /// Stop-loss percentage for crypto positions. Default 0.50 (50%).
    /// Hardcoded high because hold-to-resolution is usually better than early exit
    /// given the 2% round-trip fee. Configurable via CRYPTO_STOP_LOSS_PCT.
    crypto_stop_loss_pct: f64,
    /// Take-profit percentage for general pipeline positions. Default 0.15 (15%).
    /// Configurable via TAKE_PROFIT_PCT.
    take_profit_pct: f64,
    /// Stop-loss percentage for general pipeline positions. Default 0.10 (10%).
    /// Configurable via STOP_LOSS_PCT.
    stop_loss_pct: f64,
    /// Client for fetching cycle data (rounds, prices, candles) from the data-hub service
    data_client: Arc<DataClient>,
    /// Shared DataState: hydrated by DataClient with futures, order books, trade tapes, etc.
    data_state: Arc<DataState>,
    /// Composable risk gate chain — profile-specific safety layer that runs after RiskEngine.
    /// Both RiskEngine AND gate chain must approve for a trade to execute.
    risk_chain: RiskGateChain,
    /// HTTP client for calling the Python strategy service (reused across cycles)
    strategy_client: reqwest::Client,
    /// URL of the Python strategy service. Default "http://localhost:8100".
    /// Hardcoded default for local dev; overridden via STRATEGY_SERVICE_URL in Docker.
    strategy_service_url: String,
    /// Live CLOB executor — created lazily on first Live-mode order.
    /// None in Paper mode. Requires POLYMARKET_PRIVATE_KEY and other secrets.
    live_executor: Option<Box<dyn Executor>>,
    /// Policy for positions whose round disappears from the scanner
    vanished_trade_policy: VanishedTradePolicy,
    /// Fee model for computing trading costs (Polymarket crypto fee formula)
    cost_model: CostModel,
    /// Paper executor for simulating fills (95% fill rate, 20bps slippage, 0ms latency)
    paper_executor: polybot_executor::paper::PaperExecutor,
    /// Equity at the start of the current ladder round — used by aggressive circuit breaker
    round_start_equity: f64,
    /// Whether the aggressive profile has been demoted to base sizing this ladder round
    aggressive_demoted: bool,
    /// Max loss per ladder round before demoting aggressive profiles.
    /// Default $1,000. Configurable via ROUND_LOSS_LIMIT.
    round_loss_limit: f64,
    /// Bankroll used when aggressive profile is demoted. Default $500.
    /// Configurable via DEMOTED_BANKROLL.
    demoted_bankroll: f64,
    /// Optional warmup timer — if set, signals are logged but not executed until this time.
    /// Configurable via WARMUP_MINUTES env var. Used to let data accumulate before trading.
    warmup_until: Option<std::time::Instant>,
    /// RFC3339 timestamp of when this pipeline session started.
    /// On crash recovery (<5min gap), restored from previous session for correct trade_returns.
    session_start: String,
    /// Equity at session start — used for session-scoped PnL in metrics (not lifetime PnL)
    session_start_equity: f64,
    /// GTC orders placed on the CLOB book, waiting for fill confirmation.
    /// Checked every cycle: filled orders become positions, near-expiry orders are cancelled.
    pending_gtc_orders: Vec<PendingGtcOrder>,
}

impl Pipeline {
    /// Create a new Pipeline with the given shared state, initial bankroll, and data client.
    ///
    /// Reads configuration from environment variables with sensible defaults:
    /// - `CRYPTO_EDGE_THRESHOLD`: min edge to trade (default 0.12 = 12%)
    /// - `CRYPTO_STOP_LOSS_PCT`: crypto stop-loss (default 0.50 = 50%)
    /// - `TAKE_PROFIT_PCT`: general take-profit (default 0.15 = 15%)
    /// - `STOP_LOSS_PCT`: general stop-loss (default 0.10 = 10%)
    /// - `STRATEGY_PROFILE`: which quant profile to use (garch-t, hurst-hinf, full-quant)
    /// - `STRATEGY_SERVICE_URL`: Python service URL (default http://localhost:8100)
    /// - `TRADING_FEE_BPS`: trading fee in basis points (default 200 = 2%)
    /// - `VANISHED_TRADE_POLICY`: "close" or "hold" (default: hold for live, close for paper)
    /// - `ROUND_LOSS_LIMIT`: aggressive circuit breaker threshold (default $1,000)
    /// - `DEMOTED_BANKROLL`: sizing after circuit breaker triggers (default $500)
    /// - `WARMUP_MINUTES`: data collection period before first trade (default 0 = disabled)
    ///
    /// Live executor is created eagerly in Live mode but lazily in Paper mode.
    pub fn new(state: Arc<AppState>, bankroll: f64, data_client: Arc<DataClient>) -> Self {
        let config = state.config.read().unwrap().clone();
        let strategy_configs = state.strategies.read().unwrap().clone();
        let crypto_configs = CryptoStrategyConfig::defaults();

        let crypto_edge_threshold: f64 = std::env::var("CRYPTO_EDGE_THRESHOLD")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(0.12);
        let crypto_stop_loss_pct: f64 = std::env::var("CRYPTO_STOP_LOSS_PCT")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(0.50);
        let take_profit_pct: f64 = std::env::var("TAKE_PROFIT_PCT")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(0.15);
        let stop_loss_pct: f64 = std::env::var("STOP_LOSS_PCT")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(0.10);

        let strategy_profile = std::env::var("STRATEGY_PROFILE").unwrap_or_default();
        let quant_composer = QuantProfile::from_str(&strategy_profile).map(|profile| {
            tracing::info!("Using quant profile: {:?}", profile);
            QuantCryptoComposer::new(profile)
        });

        // Build risk gate chain for this profile
        let risk_chain = polybot_risk::build_risk_chain(&strategy_profile, bankroll);
        let strategy_service_url = std::env::var("STRATEGY_SERVICE_URL")
            .unwrap_or_else(|_| "http://localhost:8100".to_string());

        // Create paper executor (always available); live executor created lazily
        let mode = *state.mode.read().unwrap();
        let live_executor: Option<Box<dyn Executor>> = if mode == BotMode::Live {
            match polybot_executor::live::create_live_executor() {
                Ok(e) => {
                    tracing::info!("Live executor pre-created at startup");
                    Some(e)
                }
                Err(e) => {
                    tracing::error!("Failed to create live executor at startup: {}", e);
                    None
                }
            }
        } else {
            None
        };
        tracing::info!(mode = ?mode, "Executor initialized (paper always available, live on demand)");

        // Cost model: configurable via TRADING_FEE_BPS (default 200 = 2% per side)
        let trading_fee_bps: f64 = std::env::var("TRADING_FEE_BPS")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(200.0);
        let cost_model = CostModel {
            trading_fee_bps,
            ..CostModel::default()
        };

        // Vanished trade policy: env override or mode-based default
        let vanished_trade_policy = match std::env::var("VANISHED_TRADE_POLICY").as_deref() {
            Ok("close") => VanishedTradePolicy::AutoClose,
            Ok("hold") => VanishedTradePolicy::HoldToSettlement,
            _ => match mode {
                BotMode::Live => VanishedTradePolicy::HoldToSettlement,
                _ => VanishedTradePolicy::AutoClose,
            },
        };
        tracing::info!(policy = ?vanished_trade_policy, "Vanished trade policy set");

        let data_state = data_client.state();

        // Aggressive circuit breaker config
        let round_loss_limit: f64 = std::env::var("ROUND_LOSS_LIMIT")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(1_000.0);
        let demoted_bankroll: f64 = std::env::var("DEMOTED_BANKROLL")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(500.0);

        tracing::info!(
            profile = %strategy_profile,
            strategy_url = %strategy_service_url,
            round_loss_limit = round_loss_limit,
            demoted_bankroll = demoted_bankroll,
            "Pipeline initialized with DataClient + risk gate chain + strategy service"
        );

        Self {
            scanner: ClobClient::new(),
            filter: FilterConfig::default(),
            risk: RiskEngine::new(config, bankroll),
            composer: StrategyComposer::new(&strategy_configs),
            crypto_composer: CryptoComposer::new(&crypto_configs),
            quant_composer,
            state,
            bankroll,
            current_equity: bankroll,
            trade_returns: Vec::new(),
            crypto_edge_threshold,
            crypto_stop_loss_pct,
            take_profit_pct,
            stop_loss_pct,
            data_client,
            data_state,
            risk_chain,
            strategy_client: reqwest::Client::new(),
            strategy_service_url,
            live_executor,
            vanished_trade_policy,
            cost_model,
            paper_executor: polybot_executor::paper::PaperExecutor {
                fill_rate: 0.95,
                slippage_bps: 20.0,
                latency_ms: 0, // no artificial latency in pipeline hot loop
            },
            round_start_equity: bankroll,
            aggressive_demoted: false,
            round_loss_limit,
            demoted_bankroll,
            warmup_until: {
                let mins: u64 = std::env::var("WARMUP_MINUTES")
                    .ok().and_then(|v| v.parse().ok()).unwrap_or(0);
                if mins > 0 {
                    tracing::info!("Warmup: collecting data for {} minutes before trading", mins);
                    Some(std::time::Instant::now() + std::time::Duration::from_secs(mins * 60))
                } else {
                    None
                }
            },
            session_start: chrono::Utc::now().to_rfc3339(),
            session_start_equity: bankroll,
            pending_gtc_orders: Vec::new(),
        }
    }

    /// Sync the risk engine with any positions recovered from the database.
    /// Prevents underflow when recovered positions close before any new ones open.
    pub fn sync_recovered_positions(&mut self) {
        let sizes: Vec<f64> = self.state.crypto_positions.read().unwrap()
            .iter().map(|p| p.size).chain(
                self.state.positions.read().unwrap().iter().map(|p| p.size)
            ).collect();
        for size in &sizes {
            self.risk.update_position_opened(*size);
        }
        if !sizes.is_empty() {
            tracing::info!(
                "Risk engine synced with {} recovered positions (${:.0} exposure)",
                sizes.len(), sizes.iter().sum::<f64>()
            );
        }
    }

    /// Restore equity from the database after a restart.
    ///
    /// Two modes based on time since last session:
    /// - **Crash recovery** (<5 min gap): restores exact saved equity from DB.
    ///   Uses previous session's start timestamp so trade_returns can be fully restored.
    /// - **New activation** (>5 min gap): recalculates equity as bankroll + sum(all historical PnL).
    ///   This is more accurate than a stale snapshot because PnL records are the source of truth.
    ///
    /// The 5-minute threshold (hardcoded) distinguishes between intentional restarts
    /// (deploy, config change) and unexpected crashes (OOM, panic).
    pub fn restore_equity(&mut self, pipeline_key: &str) {
        if let Some(ref db) = self.state.db {
            // Load previous session start BEFORE overwriting with current
            let prev_session = db.load_session_start(pipeline_key).ok().flatten();

            let is_crash_recovery = match &prev_session {
                Some(prev_ts) => {
                    if let Ok(prev) = chrono::DateTime::parse_from_rfc3339(prev_ts) {
                        let elapsed = chrono::Utc::now() - prev.with_timezone(&chrono::Utc);
                        elapsed.num_seconds() < 300 // < 5 min = crash recovery
                    } else {
                        false
                    }
                }
                None => false,
            };

            // For crash recovery, use previous session's start for trade_returns
            // For new activation, use current (no prior trades to restore)
            if is_crash_recovery {
                if let Some(prev_ts) = prev_session {
                    self.session_start = prev_ts;
                }
            }

            // Now record current session start
            let _ = db.save_session_start(pipeline_key, &chrono::Utc::now().to_rfc3339());

            if is_crash_recovery {
                // Crash recovery: restore saved equity + trade returns from crashed session
                match db.load_equity(pipeline_key) {
                    Ok(Some(equity)) if equity > 0.0 => {
                        tracing::info!("Crash recovery: restored equity ${:.2} (key={})", equity, pipeline_key);
                        self.current_equity = equity;
                    }
                    _ => {}
                }
            } else {
                // New activation: recalculate equity = bankroll + sum(pnl) of ALL historical trades
                match db.sum_pnl(pipeline_key) {
                    Ok(total_pnl) if total_pnl.abs() > 0.001 => {
                        self.current_equity = self.bankroll + total_pnl;
                        tracing::info!(
                            "New activation: equity = bankroll ${:.2} + historical PnL ${:.2} = ${:.2} (key={})",
                            self.bankroll, total_pnl, self.current_equity, pipeline_key
                        );
                    }
                    _ => {
                        tracing::info!("Fresh start: equity = bankroll ${:.2} (key={})", self.bankroll, pipeline_key);
                    }
                }
                let _ = db.save_equity(pipeline_key, self.current_equity);
            }
        }
        // Record session start equity for session-scoped PnL and risk engine
        self.session_start_equity = self.current_equity;
        self.risk.reset_session_equity(self.current_equity);
    }

    /// Rebuild `trade_returns` from persisted position history so that trade count,
    /// win rate, Sharpe, etc. are correct after a container restart.
    /// On new activation: starts fresh (0 trades). On crash recovery: restores from crashed session.
    pub fn restore_trade_returns(&mut self, pipeline_key: &str) {
        if let Some(ref db) = self.state.db {
            // session_start is set to previous session's start on crash recovery,
            // or current time on new activation (so no prior trades match)
            match db.query_positions_since(pipeline_key, &self.session_start) {
                Ok(positions) if !positions.is_empty() => {
                    for pos in &positions {
                        let trade_return = if pos.size > 0.0 { pos.pnl / pos.size } else { 0.0 };
                        self.trade_returns.push(trade_return);
                    }
                    tracing::info!(
                        "Restored {} trade returns from current session (key={})",
                        positions.len(), pipeline_key
                    );
                    if pipeline_key == "crypto" {
                        self.recompute_crypto_metrics();
                    } else {
                        self.recompute_metrics();
                    }
                }
                Ok(_) => {
                    tracing::info!("New session: 0 prior trades to restore (key={})", pipeline_key);
                }
                Err(e) => {
                    tracing::warn!("Failed to restore trade returns (key={}): {}", pipeline_key, e);
                }
            }
        }
    }

    /// Main event loop for the crypto pipeline with adaptive timing.
    ///
    /// Runs in an infinite loop with mode-aware behavior:
    /// - **Stopped**: sleeps 5s, checks mode again
    /// - **Paper/Live**: runs `run_crypto_cycle()` then sleeps adaptively (1-5s)
    ///
    /// The adaptive sleep is computed by `compute_crypto_sleep()`:
    /// shorter when rounds are near expiry (1s when <30s remaining),
    /// longer when all rounds are far from expiry (5s max).
    ///
    /// Reference price recording has been moved to the data-hub service.
    /// The general pipeline runs separately in main.rs on a 300s interval.
    pub async fn run_dual_pipeline(&mut self) {
        loop {
            let mode = *self.state.mode.read().unwrap();
            match mode {
                BotMode::Stopped => {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
                BotMode::Paper | BotMode::Live => {
                    // Respect pipeline mode
                    let pipeline_mode = *self.state.pipeline_mode.read().unwrap();
                    if !matches!(pipeline_mode, PipelineMode::Both | PipelineMode::CryptoOnly) {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        continue;
                    }

                    // Run crypto pipeline (adaptive timing)
                    if let Err(e) = self.run_crypto_cycle().await {
                        tracing::error!("Crypto pipeline error: {}", e);
                    }

                    // Adaptive sleep: shorter when rounds are near expiry
                    let sleep_secs = self.compute_crypto_sleep();
                    tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)).await;
                }
            }
        }
    }

    /// General pipeline cycle for non-crypto Polymarket event markets.
    ///
    /// This is the original pipeline that trades event markets (elections, sports, etc.).
    /// Currently always runs in paper mode — only the crypto pipeline trades live.
    /// This is because general market strategies are less tested and lack a risk gate chain.
    ///
    /// Steps:
    /// 1. Fetch and filter markets from CLOB (exclude crypto up/down)
    /// 2. Update existing positions (check exits)
    /// 3. For each candidate: predict -> risk check -> execute if approved
    pub async fn run_general_cycle(&mut self) -> Result<()> {
        let start = std::time::Instant::now();

        let strategy_configs = self.state.strategies.read().unwrap().clone();
        self.composer.update_configs(&strategy_configs);

        let scan_start = std::time::Instant::now();
        let markets = self.scanner.fetch_markets(100).await?;
        // Filter out crypto up/down markets
        let candidates: Vec<Market> = self.filter.filter(&markets).into_iter()
            .filter(|m| !is_crypto_market(&m.question))
            .collect();
        let scan_ms = scan_start.elapsed().as_millis() as f64;
        tracing::info!("General: scanned {} markets, {} candidates in {:.0}ms", markets.len(), candidates.len(), scan_ms);

        let market_map: HashMap<String, &Market> = candidates.iter()
            .map(|m| (m.condition_id.clone(), m))
            .collect();

        self.update_positions(&market_map).await;

        let open_market_ids: HashSet<String> = self.state.positions.read().unwrap()
            .iter()
            .map(|p| p.market_id.clone())
            .collect();

        for market in &candidates {
            if market.outcome_prices.len() < 2 { continue; }
            if open_market_ids.contains(&market.condition_id) { continue; }

            let prediction = match self.composer.predict(market) {
                Some(p) => p,
                None => continue,
            };

            let p_market = market.outcome_prices[0];
            let edge = prediction.edge;
            let z_score = prediction.z_score;

            let request = PositionRequest {
                market_id: market.condition_id.clone(),
                question: market.question.clone(),
                direction: if edge > 0.0 { Direction::BuyYes } else { Direction::BuyNo },
                p_model: prediction.p_model,
                p_market,
                edge: edge.abs(),
                z_score: z_score.abs(),
            };

            let risk_start = std::time::Instant::now();
            let verdict = self.risk.evaluate(&request);
            let risk_ms = risk_start.elapsed().as_millis() as f64;

            let component_summary: String = prediction.components.iter()
                .map(|c| format!("{}({:.4}@{:.2})", c.strategy_name, c.p_model, c.confidence))
                .collect::<Vec<_>>()
                .join(", ");
            let reason = format!("{} | strategies: [{}]", verdict.reason, component_summary);

            let signal = Signal {
                id: uuid::Uuid::new_v4().to_string(),
                timestamp: chrono::Utc::now(),
                market_id: market.condition_id.clone(),
                question: market.question.clone(),
                edge,
                z_score,
                action: if verdict.approved { SignalAction::Entered } else { SignalAction::Rejected },
                reason,
            };

            {
                let mut signals = self.state.signals.write().unwrap();
                signals.push(signal);
                if signals.len() > 100 { signals.drain(0..50); }
            }

            if verdict.approved {
                // Warmup check
                if let Some(until) = self.warmup_until {
                    if std::time::Instant::now() < until {
                        continue;
                    } else {
                        self.warmup_until = None;
                    }
                }

                tracing::info!(
                    "SIGNAL: {} edge={:.4} kelly={:.4} size=${:.0} p_model={:.4} ({} strategies)",
                    market.question, edge, verdict.kelly_fraction, verdict.position_size,
                    prediction.p_model, prediction.components.len()
                );

                let mode = *self.state.mode.read().unwrap();
                if matches!(mode, BotMode::Paper | BotMode::Live) {
                    // General scanner is ALWAYS paper — only crypto pipeline trades live.
                    // General markets have untested strategies and no risk gate chain.
                    let use_live = false;

                    if use_live {
                        // #30: Submit live order for general pipeline too
                        if self.live_executor.is_none() {
                            match polybot_executor::live::create_live_executor() {
                                Ok(e) => {
                                    tracing::info!("Live executor created on demand (general pipeline)");
                                    self.live_executor = Some(e);
                                }
                                Err(e) => {
                                    tracing::error!("Cannot create live executor: {} — skipping order", e);
                                    continue;
                                }
                            }
                        }

                        // General markets: use Yes token (token_ids[0]) or No token
                        let token_id = if market.token_ids.len() >= 2 {
                            if edge > 0.0 { market.token_ids[0].clone() } else { market.token_ids[1].clone() }
                        } else { continue; };
                        if token_id.is_empty() {
                            tracing::error!("Empty token_id for general market {} — skipping", market.question);
                            continue;
                        }

                        // Use correct price for direction: Yes price for Yes, No price for No
                        let order_price = if edge > 0.0 {
                            p_market // Yes token price
                        } else {
                            market.outcome_prices.get(1).copied().unwrap_or(1.0 - p_market)
                        };
                        let order_request = ClobOrderRequest {
                            token_id,
                            price: order_price,
                            size_usd: verdict.position_size,
                            order_type: ClobOrderType::Fok,
                            fee_rate_bps: market.fee_rate_bps,
                            side: ClobSide::Buy,
                            neg_risk: market.neg_risk,
                        };

                        match self.live_executor.as_ref().unwrap().execute_order(&order_request).await {
                            Ok(resp) if resp.success => {
                                let fill_price = resp.price.unwrap_or(p_market);
                                if fill_price <= 0.0 {
                                    tracing::error!("LIVE ORDER: fill_price=0 for general {} — skipping", market.question);
                                    continue;
                                }
                                tracing::info!(
                                    "LIVE ORDER FILLED (general): {} order_id={:?} fill_price={:.4}",
                                    market.question, resp.order_id, fill_price
                                );
                                self.risk.update_position_opened(verdict.position_size);

                                let position = Position {
                                    id: resp.order_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                                    market_id: market.condition_id.clone(),
                                    question: market.question.clone(),
                                    direction: if edge > 0.0 { "Yes".into() } else { "No".into() },
                                    entry_price: fill_price,
                                    current_price: fill_price,
                                    size: verdict.position_size,
                                    unrealized_pnl: 0.0,
                                    opened_at: chrono::Utc::now(),
                                    entry_edge: edge.abs(),
                                    is_live: true,
                                    fee_rate_bps: market.fee_rate_bps as f64,
                                    hold_to_resolution: false,
                                    closed_at: None,
                                    exit_reason: None,
                                };
                                if let Some(db) = &self.state.db {
                                    let _ = db.save_open_position("general", &position);
                                }
                                self.state.positions.write().unwrap().push(position);

                                let _ = self.state.tx.send(serde_json::json!({
                                    "event": "position_opened",
                                    "market": market.question,
                                    "edge": edge,
                                    "size": verdict.position_size,
                                    "live": true,
                                    "fill_price": fill_price,
                                }).to_string());
                            }
                            Ok(resp) => {
                                tracing::warn!("LIVE ORDER REJECTED (general): {} error={:?}", market.question, resp.error_msg);
                            }
                            Err(e) => {
                                tracing::error!("LIVE ORDER FAILED (general): {} err={}", market.question, e);
                            }
                        }
                    } else {
                        // Paper mode
                        self.risk.update_position_opened(verdict.position_size);

                        let position = Position {
                            id: uuid::Uuid::new_v4().to_string(),
                            market_id: market.condition_id.clone(),
                            question: market.question.clone(),
                            direction: if edge > 0.0 { "Yes".into() } else { "No".into() },
                            entry_price: p_market,
                            current_price: p_market,
                            size: verdict.position_size,
                            unrealized_pnl: 0.0,
                            opened_at: chrono::Utc::now(),
                            entry_edge: edge.abs(),
                            is_live: false,
                            fee_rate_bps: market.fee_rate_bps as f64,
                            hold_to_resolution: false,
                            closed_at: None,
                            exit_reason: None,
                        };
                        self.state.positions.write().unwrap().push(position);

                        let _ = self.state.tx.send(serde_json::json!({
                            "event": "position_opened",
                            "market": market.question,
                            "edge": edge,
                            "p_model": prediction.p_model,
                            "confidence": prediction.confidence,
                            "strategies": prediction.components.len(),
                            "size": verdict.position_size,
                        }).to_string());
                    }
                }
            }

            let mut latency = self.state.latency.write().unwrap();
            latency.scan_avg_ms = scan_ms;
            latency.risk_avg_ms = risk_ms;
            latency.total_avg_ms = start.elapsed().as_millis() as f64;
        }

        Ok(())
    }

    // === Crypto Pipeline ===

    /// Execute one cycle of the crypto trading pipeline.
    ///
    /// This is the core trading loop, called every 1-5 seconds. Steps:
    ///
    /// 1. **Sync configs**: reload strategy weights from AppState (API can update them)
    /// 2. **Fetch data**: get rounds, prices, candles from data-hub in one batch call
    /// 3. **Build context**: cross-timeframe intel, sibling rounds, reference prices
    /// 4. **Evaluate rounds**: for each active round without an existing position:
    ///    a. Route to prediction engine (baseline=Rust, others=Python)
    ///    b. Check progress window hints from strategy meta
    ///    c. Check fee-aware edge threshold (default 12%)
    ///    d. Evaluate through dual risk layers (RiskEngine + gate chain)
    ///    e. Execute via live GTC order or paper FOK fill
    /// 5. **Process GTC orders**: check fills, cancel near-expiry orders
    /// 6. **Write enriched rounds**: annotate rounds with predictions for the API
    /// 7. **Update positions**: check exits (resolution, stop-loss, edge reversal)
    /// 8. **Update metrics**: cycle speed, nearest expiry for dashboard
    async fn run_crypto_cycle(&mut self) -> Result<()> {
        let start = std::time::Instant::now();

        // Sync crypto strategy configs from AppState (updated via API)
        {
            let crypto_configs = self.state.crypto_strategies.read().unwrap();
            let configs: Vec<CryptoStrategyConfig> = crypto_configs.iter().map(|c| {
                CryptoStrategyConfig { name: c.name.clone(), enabled: c.enabled, weight: c.weight }
            }).collect();
            drop(crypto_configs);
            self.crypto_composer.update_configs(&configs);
            if let Some(ref mut qc) = self.quant_composer {
                qc.update_configs(configs);
            }
        }

        // Step 1: Fetch all data from data-hub in one call
        let cycle = match self.data_client.fetch_cycle().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Data-hub fetch failed: {e}, skipping cycle");
                return Ok(());
            }
        };

        let mut rounds = cycle.rounds;
        let prices = self.data_client.get_all_prices();
        if prices.is_empty() {
            tracing::warn!("No prices available, skipping crypto cycle");
            return Ok(());
        }

        if rounds.is_empty() {
            tracing::debug!("No active crypto rounds found");
            // Still process pending GTC orders and auto-clears even with no active rounds
            self.process_pending_gtc_orders().await;
            self.update_crypto_positions(&[], &prices).await;
            return Ok(());
        }

        let ref_count = cycle.reference_prices.len();
        tracing::info!("Crypto: found {} active rounds, {} prices, {} refs in {:.0}ms",
            rounds.len(), prices.len(), ref_count, start.elapsed().as_millis());

        // Enrich rounds with current prices (state update deferred until after predictions)
        for round in &mut rounds {
            if let Some(&price) = prices.get(&round.asset) {
                round.current_price = Some(price);
            }
        }

        // Prediction enrichment map: condition_id -> (p_up, edge)
        let mut prediction_map: HashMap<String, (f64, f64)> = HashMap::new();

        // Build sibling map: asset -> rounds at different timeframes
        let mut sibling_map: HashMap<Asset, Vec<CryptoRound>> = HashMap::new();
        for round in &rounds {
            sibling_map.entry(round.asset).or_default().push(round.clone());
        }

        // Build cross-timeframe intelligence per asset from DataClient intel
        let mut intel_map: HashMap<Asset, TimeframeIntel> = HashMap::new();
        for &asset in &Asset::ALL {
            if let Some(intel) = self.data_client.get_intel(asset) {
                intel_map.insert(asset, TimeframeIntel {
                    asset,
                    // Scale log-returns to probability-like [0, 1] range.
                    // Typical crypto log-returns: [-0.02, +0.02] for 5m–1h windows.
                    // tanh(trend * 50) maps ±0.02 → ±0.76, giving useful spread.
                    market_bias: intel.candle_trend.iter()
                        .map(|(tf, trend)| {
                            let scaled = 0.5 + 0.5 * (trend * 50.0).tanh();
                            (tf.clone(), scaled)
                        })
                        .collect(),
                    agreement_score: intel.trend_agreement,
                });
            } else {
                // Fallback: build from round data like before
                intel_map.insert(asset, TimeframeIntel::build(asset, &rounds));
            }
        }

        // Collect open crypto position IDs + pending GTC order condition IDs
        let open_crypto_ids: HashSet<String> = self.state.crypto_positions.read().unwrap()
            .iter()
            .map(|p| p.market_id.clone())
            .collect();
        let pending_gtc_ids: HashSet<String> = self.pending_gtc_orders.iter()
            .map(|o| o.condition_id.clone())
            .collect();

        // All reference prices from DataClient
        let all_reference_prices = self.data_client.get_all_reference_prices();

        // Step 3: Evaluate each round
        for round in &rounds {
            if open_crypto_ids.contains(&round.condition_id) || pending_gtc_ids.contains(&round.condition_id) {
                continue;
            }

            // Skip rounds with <30s remaining (too late to enter)
            if round.seconds_remaining() < 30 { continue; }

            let asset_price = match prices.get(&round.asset) {
                Some(&p) => p,
                None => continue,
            };

            let reference = match self.data_client.get_reference(round.asset, round.timeframe) {
                Some(r) => r,
                None => {
                    tracing::warn!(
                        asset = ?round.asset, tf = ?round.timeframe,
                        "No reference price from data-hub, skipping round (cycle had {} refs)",
                        ref_count
                    );
                    continue;
                }
            };

            // Get candles from DataClient (cached from cycle fetch)
            let candles = self.data_client.get_candles_for_timeframe(round.asset, round.timeframe);

            let siblings = sibling_map.get(&round.asset).cloned().unwrap_or_default();

            let ctx = PredictionContext {
                round: round.clone(),
                reference_price: reference,
                current_price: asset_price,
                candles,
                all_prices: prices.clone(),
                all_reference_prices: all_reference_prices.clone(),
                sibling_rounds: siblings,
                timeframe_intel: intel_map.get(&round.asset).cloned(),
            };

            // Prediction routing: baseline uses Rust strategies directly,
            // all other profiles use Python only (no Rust fallback).
            let is_baseline = self.state.strategy_profile == "baseline";
            let prediction = if is_baseline {
                // Baseline: Rust strategies only (no Python call)
                let rust_pred = self.quant_composer.as_ref()
                    .and_then(|qc| qc.predict(&ctx))
                    .or_else(|| self.crypto_composer.predict(&ctx));
                match rust_pred {
                    Some(p) => p,
                    None => continue,
                }
            } else {
                // All other profiles: Python strategy service only — skip means skip
                match self.try_python_prediction(round, &intel_map, &all_reference_prices, &cycle.resolved_rounds).await {
                    Some(py) => {
                        tracing::info!(
                            asset = ?round.asset, tf = ?round.timeframe,
                            profile = %self.state.strategy_profile,
                            "Python prediction: p_up={:.3} edge={:.4} dir={}",
                            py.p_up, py.edge, py.direction
                        );
                        py
                    }
                    None => {
                        // No Rust fallback — if Python skips, we skip
                        continue;
                    }
                }
            };

            // Strategy pipeline hints: check progress window
            let progress = round.progress_pct();
            if let Some(min_p) = prediction.min_progress {
                if progress < min_p {
                    tracing::debug!(
                        asset = ?round.asset, tf = ?round.timeframe,
                        "Strategy hint: progress {:.1}% < min {:.1}%, skipping",
                        progress * 100.0, min_p * 100.0
                    );
                    continue;
                }
            }
            if let Some(max_p) = prediction.max_progress {
                if progress > max_p {
                    tracing::debug!(
                        asset = ?round.asset, tf = ?round.timeframe,
                        "Strategy hint: progress {:.1}% > max {:.1}%, skipping",
                        progress * 100.0, max_p * 100.0
                    );
                    continue;
                }
            }

            // Store prediction for round enrichment
            prediction_map.insert(round.condition_id.clone(), (prediction.p_up, prediction.edge));

            {
                let mut metrics = self.state.crypto_metrics.write().unwrap();
                if prediction.direction == "Up" {
                    metrics.up_predictions += 1;
                } else {
                    metrics.down_predictions += 1;
                }
            }

            // Fee-aware edge check
            if prediction.edge < self.crypto_edge_threshold {
                // Log as rejected signal
                let signal = Signal {
                    id: uuid::Uuid::new_v4().to_string(),
                    timestamp: chrono::Utc::now(),
                    market_id: round.condition_id.clone(),
                    question: format!("{:?} {:?} round", round.asset, round.timeframe),
                    edge: prediction.edge,
                    z_score: 0.0,
                    action: SignalAction::Rejected,
                    reason: format!("Edge {:.4} below crypto threshold {:.4}", prediction.edge, self.crypto_edge_threshold),
                };
                let mut signals = self.state.crypto_signals.write().unwrap();
                signals.push(signal);
                if signals.len() > 100 { signals.drain(0..50); }
                continue;
            }

            // Risk check using CLOB WS best-ask price (real market price, not stale Gamma)
            // For Down trades, p_model = 1 - p_up (probability that Down wins)
            let (clob_ask, clob_bid) = self.clob_ws_prices(&round, &prediction.direction);

            // Strategy hint: max entry price override
            if let Some(max_ep) = prediction.max_entry_price {
                if clob_ask > max_ep {
                    tracing::info!(
                        asset = ?round.asset, tf = ?round.timeframe,
                        direction = %prediction.direction,
                        edge = prediction.edge,
                        "Max entry price filter: ask {:.4} > max_entry {:.4}, skipping (edge {:.1}%)",
                        clob_ask, max_ep, prediction.edge * 100.0
                    );
                    continue;
                }
            }

            let (p_model, p_market) = if prediction.direction == "Up" {
                (prediction.p_up, clob_ask)
            } else {
                (1.0 - prediction.p_up, clob_ask)
            };
            let request = PositionRequest {
                market_id: round.condition_id.clone(),
                question: format!("{:?} {:?} {}", round.asset, round.timeframe, prediction.direction),
                direction: if prediction.direction == "Up" { Direction::BuyYes } else { Direction::BuyNo },
                p_model,
                p_market,
                edge: prediction.edge,
                z_score: 0.0,
            };

            let mut verdict = self.risk.evaluate(&request);

            // Scale position size for compounding profiles
            if matches!(self.state.strategy_profile.as_str(), "garch-t-aggressive" | "garch-t-options") && self.bankroll > 0.0 {
                let scale = self.effective_bankroll() / self.bankroll;
                verdict.position_size *= scale;
            }

            // Phase 1: Also evaluate through composable risk gate chain
            let (gate_approved, gate_reason, _gate_multiplier) =
                self.evaluate_risk_chain(round, &prediction, intel_map.get(&round.asset));

            // Both must approve — gate chain is an additional safety layer
            let final_approved = verdict.approved && gate_approved;
            let final_reason = if !gate_approved && verdict.approved {
                gate_reason.clone()
            } else {
                verdict.reason.clone()
            };

            let component_summary: String = prediction.components.iter()
                .map(|c| format!("{}({:.2}@{:.2})", c.strategy_name, c.p_up, c.confidence))
                .collect::<Vec<_>>()
                .join(", ");

            let signal = Signal {
                id: uuid::Uuid::new_v4().to_string(),
                timestamp: chrono::Utc::now(),
                market_id: round.condition_id.clone(),
                question: format!("{:?} {:?} round", round.asset, round.timeframe),
                edge: prediction.edge,
                z_score: 0.0,
                action: if final_approved { SignalAction::Entered } else { SignalAction::Rejected },
                reason: format!("{} | [{}]", final_reason, component_summary),
            };

            // Persist signal to database
            if let Some(ref db) = self.state.db {
                let hist_sig = polybot_api::db::HistorySignal {
                    id: signal.id.clone(),
                    pipeline: "crypto".into(),
                    strategy: component_summary.clone(),
                    asset: format!("{:?} {:?}", round.asset, round.timeframe),
                    direction: prediction.direction.clone(),
                    edge: signal.edge,
                    confidence: prediction.confidence,
                    action: format!("{:?}", signal.action),
                    created_at: signal.timestamp.to_rfc3339(),
                };
                let _ = db.insert_signal(&hist_sig);
            }

            {
                let mut signals = self.state.crypto_signals.write().unwrap();
                signals.push(signal);
                if signals.len() > 100 { signals.drain(0..50); }
            }

            if final_approved {
                // Warmup check: skip execution during warmup period
                if let Some(until) = self.warmup_until {
                    if std::time::Instant::now() < until {
                        let remaining = until.duration_since(std::time::Instant::now()).as_secs();
                        tracing::info!(
                            "WARMUP: {:?}/{:?} {} edge={:.4} — skipping ({:.0}m remaining)",
                            round.asset, round.timeframe, prediction.direction,
                            prediction.edge, remaining as f64 / 60.0
                        );
                        continue;
                    } else {
                        tracing::info!("Warmup complete — trading enabled");
                        self.warmup_until = None;
                    }
                }

                tracing::info!(
                    "CRYPTO SIGNAL: {:?}/{:?} {} edge={:.4} size=${:.0} ({} strategies)",
                    round.asset, round.timeframe, prediction.direction,
                    prediction.edge, verdict.position_size, prediction.components.len()
                );

                let mode = *self.state.mode.read().unwrap();
                if matches!(mode, BotMode::Paper | BotMode::Live) {
                    // For live mode: submit order via executor, only create position on success
                    let use_live = mode == BotMode::Live;
                    if use_live {
                        // Lazily create live executor on first live order
                        if self.live_executor.is_none() {
                            match polybot_executor::live::create_live_executor() {
                                Ok(e) => {
                                    tracing::info!("Live executor created on demand");
                                    self.live_executor = Some(e);
                                }
                                Err(e) => {
                                    tracing::error!("Cannot create live executor: {} — skipping order", e);
                                    continue;
                                }
                            }
                            // Check wallet balance (informational — Polymarket settles on-chain, CLOB escrow may be $0)
                            if let Some(ref executor) = self.live_executor {
                                match executor.get_balance().await {
                                    Ok(balance) if balance < f64::MAX => {
                                        tracing::info!("CLOB reported balance: ${:.2}", balance);
                                        if balance < verdict.position_size {
                                            tracing::warn!("CLOB balance ${:.2} < ${:.0} — proceeding (on-chain settlement)", balance, verdict.position_size);
                                        }
                                    }
                                    Err(e) => tracing::warn!("Balance check failed: {} — proceeding anyway", e),
                                    _ => {}
                                }
                            }
                        }
                    }
                    if use_live && self.live_executor.is_some() {
                        // Always BUY — direction is determined by which token we buy
                        let token_id = if prediction.direction == "Up" {
                            round.token_id_up.clone()
                        } else {
                            round.token_id_down.clone()
                        };
                        if token_id.is_empty() {
                            tracing::error!("Empty token_id for {:?}/{:?} {} — skipping", round.asset, round.timeframe, prediction.direction);
                            continue;
                        }
                        // Use CLOB WS price (already computed as clob_ask above)
                        tracing::info!("CLOB WS ask={:.4} bid={:.4} (Gamma={:.4})",
                            clob_ask, clob_bid,
                            if prediction.direction == "Up" { round.price_up } else { round.price_down });
                        // GTC: order sits on the book until filled or cancelled.
                        // Better fill rate than FOK for hold-to-resolution strategy.
                        let order_request = ClobOrderRequest {
                            token_id,
                            price: clob_ask,
                            size_usd: verdict.position_size,
                            order_type: ClobOrderType::Gtc,
                            fee_rate_bps: round.fee_rate_bps,
                            side: ClobSide::Buy,
                            neg_risk: false, // Crypto Up/Down markets use standard CTF Exchange
                        };

                        match self.live_executor.as_ref().unwrap().execute_order(&order_request).await {
                            Ok(resp) if resp.success => {
                                // GTC orders: status "MATCHED" = filled, "LIVE" = on the book.
                                // Use status field (not transact_hash which may arrive later via batched tx).
                                let order_status = resp.status.as_deref().unwrap_or("");
                                if order_status.eq_ignore_ascii_case("MATCHED") || order_status.eq_ignore_ascii_case("filled") {
                                    // Immediately filled — create position now
                                    let fill_price = resp.price.unwrap_or(p_market);
                                    if fill_price <= 0.0 {
                                        tracing::error!(
                                            "LIVE ORDER: fill_price=0 for {:?}/{:?} — cannot track position safely, skipping",
                                            round.asset, round.timeframe
                                        );
                                        continue;
                                    }
                                    tracing::info!(
                                        "LIVE GTC FILLED IMMEDIATELY: {:?}/{:?} {} order_id={:?} fill_price={:.4}",
                                        round.asset, round.timeframe, prediction.direction,
                                        resp.order_id, fill_price
                                    );
                                    self.risk.update_position_opened(verdict.position_size);

                                    let position = Position {
                                        id: resp.order_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                                        market_id: round.condition_id.clone(),
                                        question: format!("{:?} {:?} {}", round.asset, round.timeframe, prediction.direction),
                                        direction: prediction.direction.clone(),
                                        entry_price: fill_price,
                                        current_price: fill_price,
                                        size: verdict.position_size,
                                        unrealized_pnl: 0.0,
                                        opened_at: chrono::Utc::now(),
                                        entry_edge: prediction.edge,
                                        is_live: true,
                                        fee_rate_bps: round.fee_rate_bps as f64,
                                        hold_to_resolution: prediction.hold_to_resolution,
                                        closed_at: None,
                                        exit_reason: None,
                                    };
                                    if let Some(db) = &self.state.db {
                                        let _ = db.save_open_position("crypto", &position);
                                    }
                                    self.state.crypto_positions.write().unwrap().push(position);

                                    let _ = self.state.tx.send(serde_json::json!({
                                        "event": "crypto_position_opened",
                                        "asset": format!("{:?}", round.asset),
                                        "timeframe": format!("{:?}", round.timeframe),
                                        "direction": prediction.direction,
                                        "edge": prediction.edge,
                                        "size": verdict.position_size,
                                        "live": true,
                                        "fill_price": fill_price,
                                    }).to_string());
                                } else {
                                    // Order placed on the book — track as pending GTC
                                    let order_id = match resp.order_id {
                                        Some(ref id) if !id.is_empty() => id.clone(),
                                        _ => {
                                            tracing::error!(
                                                "GTC placed but no order_id returned for {:?}/{:?} {} — cannot track",
                                                round.asset, round.timeframe, prediction.direction
                                            );
                                            continue;
                                        }
                                    };
                                    tracing::info!(
                                        "LIVE GTC PLACED ON BOOK: {:?}/{:?} {} order_id={} limit={:.4} size=${:.0}",
                                        round.asset, round.timeframe, prediction.direction,
                                        order_id, clob_ask, verdict.position_size
                                    );
                                    self.pending_gtc_orders.push(PendingGtcOrder {
                                        order_id,
                                        condition_id: round.condition_id.clone(),
                                        direction: prediction.direction.clone(),
                                        size_usd: verdict.position_size,
                                        entry_edge: prediction.edge,
                                        round_end: round.round_end,
                                        submitted_at: chrono::Utc::now(),
                                        asset: round.asset,
                                        timeframe: round.timeframe,
                                        fee_rate_bps: round.fee_rate_bps,
                                        hold_to_resolution: prediction.hold_to_resolution,
                                        limit_price: clob_ask,
                                    });
                                }
                            }
                            Ok(resp) => {
                                tracing::warn!(
                                    "LIVE GTC ORDER REJECTED: {:?}/{:?} {} error={:?}",
                                    round.asset, round.timeframe, prediction.direction,
                                    resp.error_msg
                                );
                            }
                            Err(e) => {
                                tracing::error!(
                                    "LIVE GTC ORDER FAILED: {:?}/{:?} {} err={}",
                                    round.asset, round.timeframe, prediction.direction, e
                                );
                            }
                        }
                    } else {
                        // Paper mode: simulate fill through PaperExecutor (95% fill, 20bps slippage)
                        let paper_token_id = if prediction.direction == "Up" {
                            &round.token_id_up
                        } else {
                            &round.token_id_down
                        };
                        let paper_request = ClobOrderRequest {
                            token_id: paper_token_id.clone(),
                            price: clob_ask,
                            size_usd: verdict.position_size,
                            side: ClobSide::Buy,
                            order_type: ClobOrderType::Fok,
                            fee_rate_bps: round.fee_rate_bps,
                            neg_risk: false,
                        };
                        match self.paper_executor.execute(&paper_request).await {
                            Ok(resp) if resp.success => {
                                let fill_price = resp.price.unwrap_or(clob_ask);
                                tracing::info!("PAPER FILL: {:?}/{:?} {} at CLOB ask={:.4} fill={:.4} (Gamma={:.4}) fee_bps={}",
                                    round.asset, round.timeframe, prediction.direction, clob_ask, fill_price,
                                    if prediction.direction == "Up" { round.price_up } else { round.price_down },
                                    round.fee_rate_bps);
                                self.risk.update_position_opened(verdict.position_size);

                                let position = Position {
                                    id: uuid::Uuid::new_v4().to_string(),
                                    market_id: round.condition_id.clone(),
                                    question: format!("{:?} {:?} {}", round.asset, round.timeframe, prediction.direction),
                                    direction: prediction.direction.clone(),
                                    entry_price: fill_price,
                                    current_price: fill_price,
                                    size: verdict.position_size,
                                    unrealized_pnl: 0.0,
                                    opened_at: chrono::Utc::now(),
                                    entry_edge: prediction.edge,
                                    is_live: false,
                                    hold_to_resolution: prediction.hold_to_resolution,
                                    closed_at: None,
                                    exit_reason: None,
                                    fee_rate_bps: round.fee_rate_bps as f64,
                                };
                                if let Some(db) = &self.state.db {
                                    let _ = db.save_open_position("crypto", &position);
                                }
                                self.state.crypto_positions.write().unwrap().push(position);

                                let _ = self.state.tx.send(serde_json::json!({
                                    "event": "crypto_position_opened",
                                    "asset": format!("{:?}", round.asset),
                                    "timeframe": format!("{:?}", round.timeframe),
                                    "direction": prediction.direction,
                                    "edge": prediction.edge,
                                    "size": verdict.position_size,
                                    "fill_price": fill_price,
                                }).to_string());
                            }
                            Ok(_) => {
                                tracing::info!("PAPER NO-FILL: {:?}/{:?} {} (simulated rejection)",
                                    round.asset, round.timeframe, prediction.direction);
                            }
                            Err(e) => {
                                tracing::error!("Paper executor error: {}", e);
                            }
                        }
                    }
                }
            }
        }

        // Step 3.5: Process pending GTC orders (check fills, cancel near round end)
        self.process_pending_gtc_orders().await;

        // Write enriched rounds to state (with predictions + position markers)
        {
            let open_ids: HashSet<String> = self.state.crypto_positions.read().unwrap()
                .iter().map(|p| p.market_id.clone()).collect();
            let pending_ids: HashSet<String> = self.pending_gtc_orders.iter()
                .map(|o| o.condition_id.clone()).collect();
            for round in &mut rounds {
                if let Some(&(p_up, edge)) = prediction_map.get(&round.condition_id) {
                    round.our_p_up = Some(p_up);
                    round.edge = Some(edge);
                }
                round.has_position = open_ids.contains(&round.condition_id)
                    || pending_ids.contains(&round.condition_id);
            }
            let mut crypto_rounds = self.state.crypto_rounds.write().unwrap();
            *crypto_rounds = rounds.clone();
        }

        // Step 4: Update crypto positions / check exits
        self.update_crypto_positions(&rounds, &prices).await;

        // Step 5: Update cycle speed and nearest expiry for API
        let sleep_secs = self.compute_crypto_sleep();
        *self.state.crypto_cycle_speed_ms.write().unwrap() = sleep_secs * 1000;
        let nearest = rounds.iter()
            .map(|r| r.seconds_remaining())
            .filter(|&s| s > 0)
            .min()
            .unwrap_or(0);
        *self.state.crypto_nearest_expiry.write().unwrap() = nearest;

        Ok(())
    }

    /// Process pending GTC orders: check for fills, cancel orders near round end.
    async fn process_pending_gtc_orders(&mut self) {
        if self.pending_gtc_orders.is_empty() { return; }

        let executor = match self.live_executor.as_ref() {
            Some(e) => e,
            None => return,
        };

        let now = chrono::Utc::now();
        let mut filled_indices: Vec<(usize, f64, f64)> = Vec::new(); // (index, avg_price, size_matched)
        let mut cancel_indices = Vec::new();

        for (i, pending) in self.pending_gtc_orders.iter().enumerate() {
            let secs_remaining = (pending.round_end - now).num_seconds();

            // Cancel orders for rounds about to end
            if secs_remaining < GTC_CANCEL_BEFORE_END_SECS {
                match executor.cancel_order(&pending.order_id).await {
                    Ok(true) => {
                        tracing::info!(
                            "GTC CANCELLED (round ending): {:?}/{:?} {} order_id={} ({}s remaining)",
                            pending.asset, pending.timeframe, pending.direction,
                            pending.order_id, secs_remaining
                        );
                    }
                    Ok(false) => {
                        // Cancel failed — order may have already been filled
                        tracing::info!(
                            "GTC cancel returned false for {:?}/{:?} {} — checking if filled",
                            pending.asset, pending.timeframe, pending.direction
                        );
                    }
                    Err(e) => {
                        tracing::warn!("GTC cancel error for {}: {}", pending.order_id, e);
                    }
                }
                // Check status regardless — might have been filled just before cancel
                match executor.get_order_status(&pending.order_id).await {
                    Ok(GtcOrderStatus::Filled { avg_price, size_matched }) if avg_price > 0.0 => {
                        tracing::info!(
                            "GTC FILLED (discovered at cancel): {:?}/{:?} {} fill_price={:.4} size_matched={:.2}",
                            pending.asset, pending.timeframe, pending.direction, avg_price, size_matched
                        );
                        filled_indices.push((i, avg_price, size_matched));
                    }
                    _ => {
                        cancel_indices.push(i);
                    }
                }
                continue;
            }

            // For orders still within the round window, check if filled
            match executor.get_order_status(&pending.order_id).await {
                Ok(GtcOrderStatus::Filled { avg_price, size_matched }) if avg_price > 0.0 => {
                    tracing::info!(
                        "GTC FILLED: {:?}/{:?} {} order_id={} fill_price={:.4} size_matched={:.2} (waited {:.0}s)",
                        pending.asset, pending.timeframe, pending.direction,
                        pending.order_id, avg_price, size_matched,
                        (now - pending.submitted_at).num_seconds()
                    );
                    filled_indices.push((i, avg_price, size_matched));
                }
                Ok(GtcOrderStatus::Cancelled) => {
                    tracing::info!(
                        "GTC was cancelled externally: {:?}/{:?} {} order_id={}",
                        pending.asset, pending.timeframe, pending.direction, pending.order_id
                    );
                    cancel_indices.push(i);
                }
                Ok(GtcOrderStatus::Open) | Ok(GtcOrderStatus::Filled { .. }) => {
                    // Open: still on the book. Filled with avg_price=0: bad data, retry next cycle.
                }
                Ok(GtcOrderStatus::Unknown) | Err(_) => {
                    // Can't determine status — leave pending, will check again next cycle
                }
            }
        }

        // Create positions for filled GTC orders
        for (idx, fill_price, size_matched) in &filled_indices {
            let pending = &self.pending_gtc_orders[*idx];
            // Use actual filled size (shares * price = USD), fallback to requested size
            let filled_usd = if *size_matched > 0.0 && *fill_price > 0.0 {
                *size_matched * *fill_price
            } else {
                pending.size_usd
            };
            self.risk.update_position_opened(filled_usd);

            let position = Position {
                id: pending.order_id.clone(),
                market_id: pending.condition_id.clone(),
                question: format!("{:?} {:?} {}", pending.asset, pending.timeframe, pending.direction),
                direction: pending.direction.clone(),
                entry_price: *fill_price,
                current_price: *fill_price,
                size: filled_usd,
                unrealized_pnl: 0.0,
                opened_at: pending.submitted_at,
                entry_edge: pending.entry_edge,
                is_live: true,
                fee_rate_bps: pending.fee_rate_bps as f64,
                hold_to_resolution: pending.hold_to_resolution,
                closed_at: None,
                exit_reason: None,
            };
            if let Some(db) = &self.state.db {
                let _ = db.save_open_position("crypto", &position);
            }
            self.state.crypto_positions.write().unwrap().push(position);

            let _ = self.state.tx.send(serde_json::json!({
                "event": "crypto_position_opened",
                "asset": format!("{:?}", pending.asset),
                "timeframe": format!("{:?}", pending.timeframe),
                "direction": pending.direction,
                "edge": pending.entry_edge,
                "size": filled_usd,
                "live": true,
                "fill_price": fill_price,
                "gtc_wait_secs": (chrono::Utc::now() - pending.submitted_at).num_seconds(),
            }).to_string());
        }

        // Remove processed orders (filled + cancelled) — iterate in reverse to preserve indices
        let mut remove_indices: Vec<usize> = filled_indices.iter().map(|(i, _, _)| *i)
            .chain(cancel_indices.iter().copied())
            .collect();
        remove_indices.sort_unstable();
        remove_indices.dedup();
        for idx in remove_indices.into_iter().rev() {
            self.pending_gtc_orders.swap_remove(idx);
        }
    }

    /// Manage crypto position exits — the most complex part of position management.
    ///
    /// Handles multiple exit paths:
    /// - **Round ended (non-hold)**: resolve via asset price vs reference price (binary outcome)
    /// - **Round ended (hold-to-resolution)**: mark as pending_settlement, let on-chain settle
    /// - **Pending settlement resolved**: price converged to 0 or 1 on CLOB
    /// - **Auto-clear**: pending_settlement positions cleared after 30 minutes (hardcoded)
    /// - **Edge reversal**: only for non-hold positions with >25% profit (hardcoded threshold)
    /// - **Stop loss**: non-hold positions only, at crypto_stop_loss_pct (default 50%)
    /// - **Round vanished**: depends on VanishedTradePolicy (hold or auto-close)
    ///
    /// For live positions, SELL orders are submitted BEFORE removing from tracking.
    /// If the SELL fails, the position stays tracked to avoid orphaning real tokens.
    ///
    /// PnL calculation uses the real Polymarket crypto fee formula:
    /// - Resolution exits: entry fee only (no SELL needed, on-chain payout)
    /// - Early exits: round-trip fee (entry + exit)
    async fn update_crypto_positions(&mut self, rounds: &[CryptoRound], prices: &HashMap<Asset, f64>) {
        let positions: Vec<Position> = self.state.crypto_positions.read().unwrap().clone();
        if positions.is_empty() { return; }

        // Crash recovery: if any position is live but executor is None, try creating it.
        // This handles restart in Paper mode with live positions from DB.
        if self.live_executor.is_none() && positions.iter().any(|p| p.is_live) {
            match polybot_executor::live::create_live_executor() {
                Ok(e) => {
                    tracing::info!("Live executor created for crash-recovered live positions");
                    self.live_executor = Some(e);
                }
                Err(e) => {
                    tracing::warn!("Cannot create live executor for recovered positions: {} — live SELLs will be skipped", e);
                }
            }
        }

        let round_map: HashMap<String, &CryptoRound> = rounds.iter()
            .map(|r| (r.condition_id.clone(), r))
            .collect();

        let mut to_close: Vec<(usize, String, f64)> = Vec::new();
        let mut to_mark_pending: Vec<usize> = Vec::new();

        for (idx, pos) in positions.iter().enumerate() {
            // #32: Check pending_settlement positions for resolution
            if pos.exit_reason.as_deref() == Some("pending_settlement") {
                // If the round reappeared in the scanner, check if it's settled
                if let Some(r) = round_map.get(&pos.market_id) {
                    let (_, bid) = self.clob_ws_prices(r, &pos.direction);
                    let current_price = if bid > 0.0 { bid } else if pos.direction == "Up" { r.price_up } else { r.price_down };
                    // Settled: price converged to 0 or 1 (binary outcome resolved)
                    if current_price <= 0.01 || current_price >= 0.99 {
                        to_close.push((idx, "Settlement resolved".into(), current_price));
                    }
                }
                // Auto-clear after 30 minutes — round is gone from scanner, on-chain settlement
                // already happened (5m/15m rounds settle within minutes). Use last known price.
                let age_minutes = (chrono::Utc::now() - pos.opened_at).num_minutes();
                if age_minutes >= 30 {
                    let resolution_price = if pos.current_price >= 0.5 { 1.0 } else { 0.0 };
                    tracing::info!(
                        "AUTO-CLEAR: {} pending_settlement for {}m — assuming {} (last price {:.3})",
                        pos.question, age_minutes,
                        if resolution_price > 0.5 { "WON" } else { "LOST" },
                        pos.current_price
                    );
                    to_close.push((idx, format!("Auto-cleared after {}m pending", age_minutes), resolution_price));
                }
                continue;
            }

            let round = round_map.get(&pos.market_id);

            match round {
                Some(r) => {
                    // Use CLOB WS mid-price for position monitoring (realistic)
                    let (ask, bid) = self.clob_ws_prices(r, &pos.direction);
                    // Mark-to-market: mid-price; exit price: bid (selling side)
                    let current_price = if ask > 0.0 && bid > 0.0 {
                        (ask + bid) / 2.0
                    } else if pos.direction == "Up" { r.price_up } else { r.price_down };
                    let exit_price = if bid > 0.0 { bid } else { current_price };

                    // Exit: round ended — hold-to-resolution positions settle on-chain,
                    // no SELL needed. Non-hold positions use binary resolution price.
                    if r.seconds_remaining() <= 0 {
                        if pos.hold_to_resolution {
                            // Let Polymarket settle on-chain — mark as pending_settlement
                            if pos.exit_reason.as_deref() != Some("pending_settlement") {
                                tracing::info!(
                                    "ROUND ENDED: {} — holding to on-chain settlement (hold_to_resolution=true)",
                                    pos.question
                                );
                                to_mark_pending.push(idx);
                            }
                            continue;
                        }
                        let asset_price = prices.get(&r.asset).copied().unwrap_or(0.0);
                        let ref_price = self.data_client.get_reference(r.asset, r.timeframe).unwrap_or(0.0);
                        let resolution_price = if ref_price > 0.0 && asset_price > 0.0 {
                            let asset_went_up = asset_price >= ref_price;
                            let our_side_won = (pos.direction == "Up" && asset_went_up)
                                || (pos.direction == "Down" && !asset_went_up);
                            if our_side_won { 1.0 } else { 0.0 }
                        } else {
                            exit_price // fallback to CLOB if no price data
                        };
                        tracing::info!("RESOLUTION: {} asset={:.2} ref={:.2} → {} (CLOB bid was {:.4})",
                            pos.question, asset_price, ref_price,
                            if resolution_price > 0.5 { "WON" } else { "LOST" }, exit_price);
                        to_close.push((idx, "Round ended".into(), resolution_price));
                        continue;
                    }

                    // With 10% exit fee, early exit is almost never worth it.
                    // Hold to resolution (entry-only fee) unless loss is extreme.
                    let pnl_pct = (current_price - pos.entry_price) / pos.entry_price;

                    // Skip early exits for hold_to_resolution positions
                    if !pos.hold_to_resolution {
                        // Exit: edge reversed + substantially profitable (>25%)
                        // Only exit early if profit exceeds the round-trip fee penalty (~10%)
                        if pnl_pct > 0.25 {
                            // Re-evaluate to see if edge reversed
                            let asset_price = prices.get(&r.asset).copied().unwrap_or(0.0);
                            if let Some(ref_price) = self.data_client.get_reference(r.asset, r.timeframe) {
                                let price_change = (asset_price - ref_price) / ref_price;
                                let direction_agrees = (pos.direction == "Up" && price_change > 0.0) ||
                                                       (pos.direction == "Down" && price_change < 0.0);
                                if !direction_agrees {
                                    to_close.push((idx, format!("Edge reversed with profit {:.2}%", pnl_pct * 100.0), exit_price));
                                    continue;
                                }
                            }
                        }

                        // Exit: stop loss
                        if pnl_pct <= -self.crypto_stop_loss_pct {
                            to_close.push((idx, format!("Stop loss: {:.2}%", pnl_pct * 100.0), exit_price));
                            continue;
                        }
                    }
                }
                None => {
                    // Round vanished from scanner
                    match self.vanished_trade_policy {
                        VanishedTradePolicy::AutoClose => {
                            to_close.push((idx, "Round no longer found (auto-close)".into(), pos.current_price));
                        }
                        VanishedTradePolicy::HoldToSettlement => {
                            // Keep position, mark as pending settlement via exit_reason field
                            // Don't close — it will settle on-chain
                            if pos.exit_reason.as_deref() != Some("pending_settlement") {
                                tracing::info!(
                                    "VANISHED: {} — holding to settlement (policy=hold)",
                                    pos.question
                                );
                                to_mark_pending.push(idx);
                            }
                        }
                    }
                }
            }
        }

        // Mark vanished positions as pending settlement
        if !to_mark_pending.is_empty() {
            let mut positions = self.state.crypto_positions.write().unwrap();
            for &idx in &to_mark_pending {
                if idx < positions.len() {
                    positions[idx].exit_reason = Some("pending_settlement".into());
                }
            }
        }

        // Update current prices on remaining positions
        {
            let mut positions = self.state.crypto_positions.write().unwrap();
            for pos in positions.iter_mut() {
                if let Some(r) = round_map.get(&pos.market_id) {
                    let price = if pos.direction == "Up" { r.price_up } else { r.price_down };
                    pos.current_price = price;
                    let shares = if pos.entry_price > 0.0 { pos.size / pos.entry_price } else { 0.0 };
                    pos.unrealized_pnl = (price - pos.entry_price) * shares;
                }
            }
        }

        // Close positions
        if to_close.is_empty() { return; }

        let mut close_indices: Vec<usize> = to_close.iter().map(|(idx, _, _)| *idx).collect();
        close_indices.sort_unstable();
        close_indices.dedup();

        let close_map: HashMap<usize, (String, f64)> = to_close.into_iter()
            .map(|(idx, reason, price)| (idx, (reason, price)))
            .collect();

        // #23: For live positions, submit SELL orders BEFORE removing from tracking.
        // If SELL fails, keep the position (don't orphan real tokens).
        let mut sell_failed_indices: HashSet<usize> = HashSet::new();
        // Track which failed sells were for ended rounds → mark pending_settlement
        let mut sell_failed_round_ended: HashSet<usize> = HashSet::new();

        if self.live_executor.is_some() {
            // Collect sell data while holding read lock.
            // CRITICAL: if a live position can't build a SELL (round vanished, bad data),
            // it MUST go into sell_failed_indices so it's NOT removed from tracking.
            let sell_data: Vec<(usize, String, String, ClobOrderRequest)>; // (idx, question, reason, request)
            {
                let positions = self.state.crypto_positions.read().unwrap();
                let mut data = Vec::new();
                for &idx in &close_indices {
                    if idx >= positions.len() { continue; }
                    let pos = &positions[idx];
                    if !pos.is_live { continue; }
                    let (reason, exit_price) = match close_map.get(&idx) {
                        Some(v) => v,
                        None => continue,
                    };
                    // On-chain payout positions: no SELL needed (Polymarket settles automatically)
                    if reason == "Settlement resolved"
                        || reason.starts_with("Auto-cleared")
                        || reason == "Round ended"
                    { continue; }

                    // Try to build SELL order; if can't, mark as failed (keep tracked)
                    let r = match round_map.get(&pos.market_id) {
                        Some(r) => r,
                        None => {
                            tracing::warn!("Cannot SELL live position {}: round vanished — marking pending_settlement", pos.question);
                            sell_failed_indices.insert(idx);
                            continue;
                        }
                    };
                    let token_id = if pos.direction == "Up" { r.token_id_up.clone() } else { r.token_id_down.clone() };
                    let shares = if pos.entry_price > 0.0 { pos.size / pos.entry_price } else { 0.0 };
                    if shares <= 0.0 {
                        sell_failed_indices.insert(idx);
                        continue;
                    }
                    data.push((idx, pos.question.clone(), reason.clone(), ClobOrderRequest {
                        token_id,
                        price: *exit_price,
                        size_usd: shares,
                        order_type: ClobOrderType::Fok,
                        fee_rate_bps: r.fee_rate_bps,
                        side: ClobSide::Sell,
                        neg_risk: false,
                    }));
                }
                sell_data = data;
            } // read lock dropped

            let executor = self.live_executor.as_ref().unwrap();
            for (idx, question, reason, sell_request) in &sell_data {
                match executor.execute_order(sell_request).await {
                    Ok(resp) if resp.success => {
                        tracing::info!("LIVE SELL FILLED: {} order_id={:?}", question, resp.order_id);
                    }
                    Ok(resp) => {
                        tracing::warn!("LIVE SELL REJECTED: {} error={:?} — keeping tracked", question, resp.error_msg);
                        sell_failed_indices.insert(*idx);
                        // If round ended and SELL was rejected (CLOB likely closed),
                        // mark as pending_settlement to avoid infinite retry
                        if reason.starts_with("Round ended") {
                            sell_failed_round_ended.insert(*idx);
                        }
                    }
                    Err(e) => {
                        tracing::error!("LIVE SELL FAILED: {} err={} — keeping tracked", question, e);
                        sell_failed_indices.insert(*idx);
                        if reason.starts_with("Round ended") {
                            sell_failed_round_ended.insert(*idx);
                        }
                    }
                }
            }
        }

        // Mark expired-round SELL failures as pending_settlement (prevents infinite retry)
        if !sell_failed_round_ended.is_empty() {
            let mut positions = self.state.crypto_positions.write().unwrap();
            for &idx in &sell_failed_round_ended {
                if idx < positions.len() {
                    tracing::info!(
                        "Marking {} as pending_settlement (SELL failed after round ended — will settle on-chain)",
                        positions[idx].question
                    );
                    positions[idx].exit_reason = Some("pending_settlement".into());
                }
            }
        }

        // Remove failed-sell positions from close list (keep them tracked)
        close_indices.retain(|idx| !sell_failed_indices.contains(idx));

        {
            let mut positions = self.state.crypto_positions.write().unwrap();
            for &idx in close_indices.iter().rev() {
                if idx < positions.len() {
                    let mut pos = positions.remove(idx);
                    let (reason, exit_price) = close_map.get(&idx).unwrap().clone();
                    pos.current_price = exit_price;
                    pos.closed_at = Some(chrono::Utc::now());
                    pos.exit_reason = Some(reason.clone());
                    // PnL: shares * (exit - entry), where shares = USD_spent / entry_price
                    let shares = if pos.entry_price > 0.0 { pos.size / pos.entry_price } else { 0.0 };
                    let raw_pnl = (exit_price - pos.entry_price) * shares;
                    // Resolution exits ("Round ended") pay entry fee only — no CLOB sell needed.
                    // Early exits (stop loss, edge reversed) require CLOB sell → round-trip fee.
                    // Uses real Polymarket crypto fee formula: fee = shares × p × 0.25 × (p(1-p))²
                    // (max ~1.56% at p=0.50, NOT the flat fee_rate_bps=1000 which is for order signing)
                    let is_resolution = reason.starts_with("Round ended");
                    let fee = if is_resolution {
                        self.cost_model.crypto_entry_cost(pos.size, pos.entry_price)
                    } else {
                        self.cost_model.crypto_round_trip_cost(pos.size, pos.entry_price, exit_price)
                    };
                    let pnl = raw_pnl - fee;
                    pos.unrealized_pnl = pnl;

                    let trade_return = if pos.size > 0.0 { pnl / pos.size } else { 0.0 };

                    self.risk.update_position_closed(pos.size, pnl);
                    self.current_equity += pnl;
                    if let Some(ref db) = self.state.db {
                        let _ = db.save_equity("crypto", self.current_equity);
                    }
                    self.trade_returns.push(trade_return);

                    tracing::info!("CRYPTO EXIT: {} pnl=${:.2} fee=${:.2} fee_type={} reason={}", pos.question, pnl, fee,
                        if is_resolution { "entry-only" } else { "round-trip" }, reason);

                    // Persist to database
                    if let Some(ref db) = self.state.db {
                        // Parse asset/timeframe from question (e.g. "BTC FiveMin Up")
                        let parts: Vec<&str> = pos.question.split_whitespace().collect();
                        let asset_str = parts.first().copied().unwrap_or("");
                        let tf_str = parts.get(1).copied().unwrap_or("");

                        let hist_pos = polybot_api::db::HistoryPosition {
                            id: pos.id.clone(),
                            pipeline: "crypto".into(),
                            asset: asset_str.to_string(),
                            timeframe: tf_str.to_string(),
                            direction: pos.direction.clone(),
                            entry_price: pos.entry_price,
                            exit_price,
                            size: pos.size,
                            pnl,
                            opened_at: pos.opened_at.to_rfc3339(),
                            closed_at: chrono::Utc::now().to_rfc3339(),
                            exit_reason: reason.clone(),
                        };
                        if let Err(e) = db.insert_position(&hist_pos) {
                            tracing::warn!("Failed to persist position: {}", e);
                        }
                        let _ = db.delete_open_position(&pos.id);

                        // Also persist round history if we have the round data
                        if let Some(r) = round_map.get(&pos.market_id) {
                            // Resolved direction = which side WON (not our position direction)
                            // If Up token price rose (exit >= entry), Up won
                            // If Up token price fell, Down won
                            // For Down positions, exit_price is the Down token price, so invert
                            let resolved = if pos.direction == "Up" {
                                if exit_price >= pos.entry_price { "Up" } else { "Down" }
                            } else {
                                // Down position: exit_price is Down token. If it rose, Down won
                                if exit_price >= pos.entry_price { "Down" } else { "Up" }
                            };
                            let round_hist = polybot_api::db::RoundHistory {
                                id: 0,
                                asset: asset_str.to_string(),
                                timeframe: tf_str.to_string(),
                                reference_price: r.reference_price.unwrap_or(0.0),
                                close_price: r.current_price.unwrap_or(0.0),
                                our_p_up: r.our_p_up.unwrap_or(r.price_up),
                                market_p_up: r.price_up,
                                edge: r.edge.unwrap_or(0.0),
                                resolved_direction: resolved.to_string(),
                                round_start: r.round_start.to_rfc3339(),
                                round_end: r.round_end.to_rfc3339(),
                            };
                            if let Err(e) = db.insert_round_history(&round_hist) {
                                tracing::warn!("Failed to persist round history: {}", e);
                            }
                        }
                    }

                    let _ = self.state.tx.send(serde_json::json!({
                        "event": "crypto_position_closed",
                        "market": pos.question,
                        "direction": pos.direction,
                        "pnl": pnl,
                        "reason": reason,
                    }).to_string());

                    // Store in crypto-specific closed list (not shared)
                    let mut closed = self.state.crypto_closed.write().unwrap();
                    closed.push(pos);
                    if closed.len() > MAX_CLOSED_HISTORY {
                        let drain_count = closed.len() - MAX_CLOSED_HISTORY;
                        closed.drain(0..drain_count);
                    }
                }
            }
        }

        self.recompute_crypto_metrics();
        self.check_aggressive_circuit_breaker();
        self.check_ladder_milestone();
        self.check_daily_loss_limit();
    }

    /// Check if the ladder system should advance to the next round.
    ///
    /// The ladder system progressively increases bankroll as the bot proves profitable.
    /// Each round has a profit target and trade count minimum. When both are met,
    /// the bot advances to the next round with a larger available bankroll.
    ///
    /// On round advancement, resets the aggressive circuit breaker tracking
    /// (round_start_equity, aggressive_demoted) since the new round starts fresh.
    fn check_ladder_milestone(&mut self) {
        let metrics = self.state.crypto_metrics.read().unwrap();
        let total_pnl = metrics.total_pnl;
        let total_trades = metrics.total_trades;
        drop(metrics);

        let mut ladder = self.state.ladder.write().unwrap();
        if ladder.check_milestone(total_pnl, total_trades) {
            let r = ladder.current_round - 1;
            if let Some(completed) = ladder.rounds.iter().find(|rd| rd.round == r) {
                let duration_hrs = completed.duration_secs.unwrap_or(0) as f64 / 3600.0;
                tracing::info!(
                    "LADDER ROUND {} COMPLETE: locked=${} available=${} total_profit=${:.2} trades={} duration={:.1}h",
                    r, completed.locked, completed.available, total_pnl,
                    completed.trades_in_round, duration_hrs
                );
            }
            // New round: reset circuit breaker tracking
            self.round_start_equity = self.current_equity;
            if self.aggressive_demoted {
                tracing::info!("New ladder round — resetting aggressive demotion (was demoted)");
                self.aggressive_demoted = false;
            }
        } else {
            // Just update profit tracking
            ladder.total_profit = total_pnl;
        }
    }

    /// Circuit breaker: if round PnL drops below -round_loss_limit, demote aggressive to base.
    fn check_aggressive_circuit_breaker(&mut self) {
        if self.aggressive_demoted { return; }
        if !matches!(self.state.strategy_profile.as_str(), "garch-t-aggressive" | "garch-t-options") {
            return;
        }

        let round_pnl = self.current_equity - self.round_start_equity;
        if round_pnl < -self.round_loss_limit {
            self.aggressive_demoted = true;
            tracing::error!(
                "CIRCUIT BREAKER: round loss ${:.2} exceeds -${:.0} limit. \
                 Demoting to base sizing (${:.0}). equity={:.2} round_start={:.2}",
                round_pnl, self.round_loss_limit, self.demoted_bankroll,
                self.current_equity, self.round_start_equity
            );
            let _ = self.state.tx.send(serde_json::json!({
                "event": "aggressive_demoted",
                "round_pnl": round_pnl,
                "limit": self.round_loss_limit,
                "demoted_bankroll": self.demoted_bankroll,
            }).to_string());
        }
    }

    /// Auto-stop the bot if daily loss exceeds MAX_DAILY_LOSS_USD.
    fn check_daily_loss_limit(&self) {
        let max_loss: f64 = std::env::var("MAX_DAILY_LOSS_USD")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0); // 0 = disabled
        if max_loss <= 0.0 {
            return;
        }
        let pnl = self.state.crypto_metrics.read().unwrap().total_pnl;
        if pnl < -max_loss {
            let mode = *self.state.mode.read().unwrap();
            if mode != BotMode::Stopped {
                tracing::error!(
                    "DAILY LOSS LIMIT BREACHED: P&L ${:.2} exceeds -${:.2} limit. AUTO-STOPPING.",
                    pnl, max_loss
                );
                *self.state.mode.write().unwrap() = BotMode::Stopped;
                let _ = self.state.tx.send(serde_json::json!({
                    "event": "daily_loss_limit",
                    "pnl": pnl,
                    "limit": max_loss,
                }).to_string());
            }
        }
    }

    /// Compute adaptive sleep duration for the crypto pipeline.
    ///
    /// Returns 1-5 seconds based on the nearest round expiry:
    /// - <30s remaining: 1s (need rapid updates near resolution)
    /// - 31-60s: 2s
    /// - 61-120s: 3s
    /// - >120s or no rounds: 5s (conserve resources)
    ///
    /// These thresholds are hardcoded based on empirical observation that
    /// CLOB prices move fastest in the last 30 seconds of a round.
    fn compute_crypto_sleep(&self) -> u64 {
        let rounds = self.state.crypto_rounds.read().unwrap();
        if rounds.is_empty() { return 5; }

        let min_secs = rounds.iter()
            .map(|r| r.seconds_remaining())
            .filter(|&s| s > 0)
            .min()
            .unwrap_or(300);

        // Adaptive: 1s when <30s to expiry, 2s when <60s, 3s when <120s, else 5s
        match min_secs {
            0..=30 => 1,
            31..=60 => 2,
            61..=120 => 3,
            _ => 5,
        }
    }

    // === Phase 1: Python Strategy Service + DataState enrichment ===

    /// Call the Python strategy service for a prediction on a crypto round.
    ///
    /// Assembles a rich JSON payload with all available data:
    /// - Round metadata (asset, timeframe, progress, prices)
    /// - 1-minute candles (up to 120 from DataClient — 1m used instead of timeframe-specific
    ///   because strategies need 20-30 samples, and 15m candles only give ~8 per round)
    /// - Futures data (funding rate, open interest, liquidations, taker ratio)
    /// - Options data (IV, skew, put/call ratio, DVOL)
    /// - CLOB order book snapshots (bids/asks for Up and Down tokens)
    /// - Trade tapes (last 100 CLOB fills)
    /// - Token price trajectory (time series of p_up/p_down from CLOB WS)
    /// - Coinbase premium (spot price divergence indicator)
    /// - Cross-timeframe intel (agreement score, parent bias)
    /// - **Parent-child Brownian Bridge context** (see below)
    /// - All asset prices and reference prices (for cross-asset analysis)
    ///
    /// ## Brownian Bridge (parent_child field)
    ///
    /// For 5m rounds, the parent is 15m (3 children per parent).
    /// For 15m rounds, the parent is 1h (4 children per parent).
    ///
    /// The payload includes:
    /// - `child_index`: which child we are (0-indexed within parent window)
    /// - `parent_direction`: estimated from parent round's CLOB price (Up/Down/Unknown)
    /// - `parent_confidence`: how certain the parent direction is (0.5 = uncertain)
    /// - `prior_children`: resolved outcomes of earlier children in the same parent window
    ///
    /// This enables the Python Brownian Bridge strategy to compute path-conditioned probabilities:
    /// e.g., if parent is UP and child 1,2 were DOWN, child 3 has very high UP probability.
    ///
    /// ## Response parsing
    /// Expects JSON with: p_up, confidence, edge, direction, components[], meta.strategy_meta
    /// Strategy hints (hold_to_resolution, min/max_progress, max_entry_price) are extracted
    /// from meta.strategy_meta and passed through to the pipeline's entry logic.
    ///
    /// Returns None if the service is unreachable, times out (5s), or returns "Skip".
    async fn try_python_prediction(
        &self,
        round: &CryptoRound,
        intel_map: &HashMap<Asset, TimeframeIntel>,
        all_reference_prices: &HashMap<Asset, f64>,
        resolved_rounds: &[polybot_data::ResolvedRoundSnapshot],
    ) -> Option<ComposedCryptoPrediction> {
        let reference_price = self.data_client.get_reference(round.asset, round.timeframe).unwrap_or(0.0);
        let current_price = self.data_client.get_price(round.asset).unwrap_or(0.0);
        let intel = intel_map.get(&round.asset);

        // Send 1-minute candles to Python (120 available per asset).
        // Timeframe-specific candles are too few (8×15m, 1×1h) for strategies needing 20-30 samples.
        let candles = self.data_client.get_candles(round.asset, "1m");
        let candles_json: Vec<serde_json::Value> = candles.iter().map(|c| serde_json::json!({
            "o": c.open, "h": c.high, "l": c.low, "c": c.close,
            "v": c.volume, "t": c.open_time
        })).collect();

        // Build futures data from DataState (hydrated by DataClient)
        let futures_json = {
            let fs = self.data_state.futures_state.read().unwrap();
            fs.get(&round.asset).map(|f| {
                let liqs: Vec<serde_json::Value> = f.recent_liquidations.iter().map(|l| {
                    serde_json::json!({
                        "side": l.side,
                        "price": l.price,
                        "quantity": l.quantity,
                        "notional": l.price * l.quantity,
                        "timestamp": l.timestamp.to_rfc3339(),
                    })
                }).collect();
                serde_json::json!({
                    "funding_rate": f.funding_rate,
                    "open_interest": f.open_interest,
                    "taker_buy_sell_ratio": f.taker_buy_sell_ratio,
                    "oi_change_5m": f.oi_change_5m,
                    "liquidations": liqs,
                })
            })
        };

        // Build all_prices from DataState (hydrated by DataClient)
        let all_prices_json: serde_json::Map<String, serde_json::Value> = {
            let prices = self.data_state.latest_prices.read().unwrap();
            prices.iter().map(|(asset, (price, _))| {
                (format!("{:?}", asset), serde_json::json!(price))
            }).collect()
        };

        // Build token_trajectory from DataState (hydrated by DataClient)
        let round_key = RoundKey { condition_id: round.condition_id.clone() };
        let token_trajectory_json: Vec<serde_json::Value> = {
            let prices = self.data_state.token_prices.read().unwrap();
            prices.get(&round_key)
                .map(|deque| deque.iter().map(|t| serde_json::json!({
                    "p_up": t.p_up,
                    "p_down": t.p_down,
                    "t": t.timestamp.timestamp(),
                })).collect())
                .unwrap_or_default()
        };

        // Build order_book from DataState (hydrated by DataClient)
        let order_book_json = {
            let books = self.data_state.order_books.read().unwrap();
            books.get(&round_key).map(|ob| serde_json::json!({
                "bids_up": ob.bids_up,
                "asks_up": ob.asks_up,
                "bids_down": ob.bids_down,
                "asks_down": ob.asks_down,
                "updated": ob.updated.timestamp(),
            }))
        };

        // Build options data from DataState
        let options_json = {
            let opts = self.data_state.options_state.read().unwrap();
            opts.get(&round.asset).map(|o| serde_json::json!({
                "iv_atm": o.iv_atm,
                "skew": o.skew,
                "put_call_ratio": o.put_call_ratio,
                "dvol": o.dvol,
            }))
        };

        // Build trade_tapes from DataState
        let trade_tapes_json: Vec<serde_json::Value> = {
            let tapes = self.data_state.trade_tapes.read().unwrap();
            tapes.get(&round_key)
                .map(|deque| deque.iter().rev().take(100).rev().map(|f| serde_json::json!({
                    "side": format!("{:?}", f.side),
                    "price": f.price,
                    "size": f.size,
                    "is_buyer_maker": f.is_buyer_maker,
                    "t": f.timestamp.timestamp(),
                })).collect())
                .unwrap_or_default()
        };

        // Build coinbase_premium from DataState
        let coinbase_premium = {
            let premiums = self.data_state.coinbase_premium.read().unwrap();
            premiums.get(&round.asset).copied().unwrap_or(0.0)
        };

        // === Brownian Bridge: parent-child round context ===
        // For 5m rounds, the parent is 15m; for 15m, the parent is 1h.
        // Compute child_index, parent direction estimate, and prior child outcomes.
        let parent_child_json = {
            let parents = round.timeframe.parents();
            if let Some(parent_tf) = parents.first() {
                let parent_secs = parent_tf.seconds() as i64;
                let child_secs = round.timeframe.seconds() as i64;
                let round_start_ts = round.round_start.timestamp();
                // Which child are we? (0-indexed within parent window)
                let parent_window_start = round_start_ts - (round_start_ts % parent_secs);
                let child_index = ((round_start_ts - parent_window_start) / child_secs) as usize;

                // Estimate parent direction from parent round's CLOB price
                let parent_bias = intel.map(|i| i.parent_bias_for(&round.timeframe)).unwrap_or(0.5);
                let (parent_dir, parent_conf) = if parent_bias > 0.60 {
                    ("Up", parent_bias)
                } else if parent_bias < 0.40 {
                    ("Down", 1.0 - parent_bias)
                } else {
                    ("Unknown", 0.5)
                };

                // Find resolved prior children in the same parent window + asset
                let asset_str = format!("{:?}", round.asset);
                let child_tf_slug = round.timeframe.slug_str();
                let mut prior_children: Vec<serde_json::Value> = Vec::new();
                for rr in resolved_rounds {
                    if rr.asset != asset_str || rr.timeframe != child_tf_slug {
                        continue;
                    }
                    // Use round_start (not resolved_at) for correct temporal placement
                    let start_str = if rr.round_start.is_empty() { &rr.resolved_at } else { &rr.round_start };
                    if let Ok(rs) = chrono::DateTime::parse_from_rfc3339(start_str) {
                        let rs_ts = rs.timestamp();
                        let rr_parent_start = rs_ts - (rs_ts % parent_secs);
                        if rr_parent_start == parent_window_start {
                            let rr_child_idx = ((rs_ts - rr_parent_start) / child_secs) as usize;
                            if rr_child_idx < child_index {
                                prior_children.push(serde_json::json!({
                                    "child_index": rr_child_idx,
                                    "direction": rr.resolved_direction,
                                    "timeframe": child_tf_slug,
                                }));
                            }
                        }
                    }
                }
                // Sort by child_index
                prior_children.sort_by_key(|c| c["child_index"].as_u64().unwrap_or(0));

                Some(serde_json::json!({
                    "child_index": child_index,
                    "parent_direction": parent_dir,
                    "parent_confidence": parent_conf,
                    "prior_children": prior_children,
                }))
            } else {
                None // No parent timeframe (e.g., 1h has no parent)
            }
        };

        // Use live CLOB mid-price for UP token instead of stale Gamma scan price.
        // This ensures Python's direction decision (p_up vs market_price_up) uses
        // the same price the Rust risk gate chain will evaluate against.
        let live_price_up = {
            let (ask, bid) = self.clob_ws_prices(round, "Up");
            let mid = if ask > 0.0 && bid > 0.0 { (ask + bid) / 2.0 } else { 0.0 };
            if mid > 0.01 { mid } else { round.price_up }
        };
        let live_price_down = 1.0 - live_price_up;

        let payload = serde_json::json!({
            "round": {
                "condition_id": round.condition_id,
                "asset": format!("{:?}", round.asset),
                "timeframe": round.timeframe.slug_str(),
                "price_up": live_price_up,
                "price_down": live_price_down,
                "seconds_remaining": round.seconds_remaining(),
                "progress_pct": round.progress_pct(),
                "timeframe_seconds": round.timeframe.seconds(),
            },
            "reference_price": reference_price,
            "current_price": current_price,
            "micro_candles": candles_json,
            "futures": futures_json,
            "options": options_json,
            "all_prices": all_prices_json,
            "all_reference_prices": all_reference_prices.iter()
                .map(|(a, p)| (format!("{:?}", a), serde_json::json!(p)))
                .collect::<serde_json::Map<String, serde_json::Value>>(),
            "token_trajectory": token_trajectory_json,
            "order_book": order_book_json,
            "trade_tapes": trade_tapes_json,
            "coinbase_premium": coinbase_premium,
            "strategy_profile": &self.state.strategy_profile,
            "timeframe_intel": intel.map(|i| serde_json::json!({
                "agreement_score": i.agreement_score,
                "market_bias": &i.market_bias,
                "parent_bias": i.parent_bias_for(&round.timeframe),
                "direction_agreement_up": i.direction_agreement(true),
                "direction_agreement_down": i.direction_agreement(false),
            })),
            "parent_child": parent_child_json,
        });

        let resp = crate::data_bridge::call_strategy_service(
            &self.strategy_client,
            &self.strategy_service_url,
            &payload,
        ).await;

        let resp = match resp {
            Some(r) => r,
            None => {
                tracing::debug!(
                    asset = ?round.asset, tf = ?round.timeframe,
                    "Python strategy: no response (service error or timeout)"
                );
                return None;
            }
        };

        let p_up = resp.get("p_up")?.as_f64()?;
        let confidence = resp.get("confidence")?.as_f64()?;
        let edge = resp.get("edge")?.as_f64()?;
        let raw_direction = resp.get("direction")?.as_str()?.to_string();
        // Normalize direction: Python may return "up"/"UP"/"Up" — canonicalize
        let direction = match raw_direction.to_lowercase().as_str() {
            "up" => "Up".to_string(),
            "down" => "Down".to_string(),
            "skip" => "Skip".to_string(),
            _ => raw_direction,
        };

        if direction == "Skip" || confidence < 0.15 {
            tracing::debug!(
                asset = ?round.asset, tf = ?round.timeframe,
                candles = candles_json.len(),
                profile = %self.state.strategy_profile,
                "Python strategy: Skip (dir={direction}, conf={confidence:.3})"
            );
            return None;
        }

        let components = resp.get("components")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|c| {
                Some(CryptoPrediction {
                    strategy_name: c.get("name")?.as_str()?.to_string(),
                    p_up: c.get("p_up")?.as_f64()?,
                    confidence: c.get("confidence")?.as_f64()?,
                    reasoning: String::new(),
                })
            }).collect())
            .unwrap_or_default();

        // Parse strategy pipeline hints from Python response meta
        let meta = resp.get("meta");
        let strategy_meta = meta
            .and_then(|m| m.get("strategy_meta"));
        let hold_to_resolution = strategy_meta
            .and_then(|m| m.get("hold_to_resolution"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let min_progress = strategy_meta
            .and_then(|m| m.get("min_progress"))
            .and_then(|v| v.as_f64());
        let max_progress = strategy_meta
            .and_then(|m| m.get("max_progress"))
            .and_then(|v| v.as_f64());
        let max_entry_price = strategy_meta
            .and_then(|m| m.get("max_entry_price"))
            .and_then(|v| v.as_f64());

        Some(ComposedCryptoPrediction {
            p_up,
            confidence,
            edge,
            direction,
            components,
            hold_to_resolution,
            min_progress,
            max_progress,
            max_entry_price,
        })
    }

    /// Effective bankroll for position sizing:
    /// - garch-t-aggressive/options: uses ladder.available (fixed per round).
    ///   R0: seed ($500, base mode). R1+: available doubles each round, cap $10k.
    ///   Seed is ALWAYS locked. Circuit breaker: round loss >$1k → demoted ($500).
    /// - momentum-combo: same ladder.available
    /// - all others: fixed initial bankroll
    fn effective_bankroll(&self) -> f64 {
        if matches!(self.state.strategy_profile.as_str(), "garch-t-aggressive" | "garch-t-options" | "fair-value")
            || self.state.strategy_profile.contains("momentum-combo")
        {
            if self.aggressive_demoted {
                return self.demoted_bankroll;
            }
            let ladder = self.state.ladder.read().unwrap();
            ladder.available
        } else {
            self.bankroll
        }
    }

    /// Get CLOB WebSocket prices for a crypto round from DataState.
    /// Returns (best_ask, best_bid) for the relevant token direction.
    /// Falls back to Gamma price if CLOB data unavailable.
    fn clob_ws_prices(&self, round: &CryptoRound, direction: &str) -> (f64, f64) {
        // Try primary index (exact condition_id match)
        let book = {
            let books = self.data_state.order_books.read().unwrap();
            let round_key = RoundKey { condition_id: round.condition_id.clone() };
            books.get(&round_key).cloned()
        };
        // Fallback to secondary index (asset + timeframe)
        let book = book.or_else(|| {
            let at_books = self.data_state.order_books_by_at.read().unwrap();
            at_books.get(&(round.asset, round.timeframe)).cloned()
        });
        if let Some(book) = book {
            let (asks, bids) = if direction == "Up" {
                (&book.asks_up, &book.bids_up)
            } else {
                (&book.asks_down, &book.bids_down)
            };
            let best_ask = asks.iter().map(|(p, _)| *p)
                .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                .unwrap_or(0.0);
            let best_bid = bids.iter().map(|(p, _)| *p)
                .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                .unwrap_or(0.0);
            if best_ask > 0.0 && best_bid > 0.0 {
                return (best_ask, best_bid);
            }
        }
        // Fallback to Gamma price
        let gamma = if direction == "Up" { round.price_up } else { round.price_down };
        (gamma, gamma)
    }

    /// Evaluate a signal through the composable risk gate chain.
    ///
    /// The gate chain is the second layer of risk management (after RiskEngine).
    /// Both layers must approve for a trade to execute. Gates are profile-specific:
    /// - Exposure limit gate (total USD exposure vs bankroll)
    /// - Per-asset position limit gate (max N positions per asset)
    /// - Drawdown gate (session drawdown vs threshold)
    /// - Correlated exposure gate (directional concentration)
    /// - Timeframe agreement gate (cross-timeframe confirmation)
    ///
    /// Includes pending GTC orders in all exposure/count calculations because
    /// GTC orders represent committed capital on the CLOB book.
    ///
    /// Returns `(approved, reason, resize_multiplier)`:
    /// - `approved`: whether the signal passed all gates
    /// - `reason`: human-readable explanation of the decision
    /// - `resize_multiplier`: position size scaling factor (1.0 = full size)
    fn evaluate_risk_chain(
        &self,
        round: &CryptoRound,
        prediction: &ComposedCryptoPrediction,
        intel: Option<&TimeframeIntel>,
    ) -> (bool, String, f64) {
        let (entry_ask, _) = self.clob_ws_prices(round, &prediction.direction);
        let entry_price = entry_ask;

        // Exclude pending_settlement positions from active counts
        let positions_guard = self.state.crypto_positions.read().unwrap();
        let asset_str = format!("{:?}", round.asset);
        let confirmed_count = positions_guard.iter()
            .filter(|p| p.exit_reason.as_deref() != Some("pending_settlement"))
            .count();
        let confirmed_exposure: f64 = positions_guard.iter()
            .filter(|p| p.exit_reason.as_deref() != Some("pending_settlement"))
            .map(|p| p.size).sum();
        let confirmed_asset_positions: usize = positions_guard.iter()
            .filter(|p| p.exit_reason.as_deref() != Some("pending_settlement"))
            .filter(|p| p.question.starts_with(&asset_str))
            .count();
        // Correlated exposure: count and sum positions in the same direction
        let active_positions: Vec<_> = positions_guard.iter()
            .filter(|p| p.exit_reason.as_deref() != Some("pending_settlement"))
            .collect();
        let confirmed_same_dir_count = active_positions.iter()
            .filter(|p| p.direction == prediction.direction)
            .count();
        let confirmed_same_dir_exposure: f64 = active_positions.iter()
            .filter(|p| p.direction == prediction.direction)
            .map(|p| p.size)
            .sum();
        drop(positions_guard);

        // Include pending GTC orders in exposure calculations — these are
        // committed USDC on the CLOB book, must count against bankroll limits
        let pending_count = self.pending_gtc_orders.len();
        let pending_exposure: f64 = self.pending_gtc_orders.iter().map(|o| o.size_usd).sum();
        let pending_asset_positions: usize = self.pending_gtc_orders.iter()
            .filter(|o| format!("{:?}", o.asset) == asset_str)
            .count();
        let pending_same_dir_count = self.pending_gtc_orders.iter()
            .filter(|o| o.direction == prediction.direction)
            .count();
        let pending_same_dir_exposure: f64 = self.pending_gtc_orders.iter()
            .filter(|o| o.direction == prediction.direction)
            .map(|o| o.size_usd)
            .sum();

        let position_count = confirmed_count + pending_count;
        let total_exposure = confirmed_exposure + pending_exposure;
        let current_asset_positions = confirmed_asset_positions + pending_asset_positions;
        let same_direction_count = confirmed_same_dir_count + pending_same_dir_count;
        let same_direction_exposure = confirmed_same_dir_exposure + pending_same_dir_exposure;
        let drawdown_pct = if self.session_start_equity > 0.0 {
            (self.session_start_equity - self.current_equity).max(0.0) / self.session_start_equity
        } else {
            0.0
        };

        let signal = GateSignal {
            market_id: round.condition_id.clone(),
            asset: format!("{:?}", round.asset),
            timeframe: round.timeframe.slug_str().to_string(),
            direction: prediction.direction.clone(),
            p_up: prediction.p_up,
            confidence: prediction.confidence,
            edge: prediction.edge,
            entry_price,
            proposed_size_usd: self.effective_bankroll() * 0.25, // scaled to bankroll, Kelly gate may resize
            current_total_exposure: total_exposure,
            current_drawdown_pct: drawdown_pct,
            current_position_count: position_count,
            bankroll: self.effective_bankroll(),
            current_net_exposure: 0.0,
            current_volatility: 0.0,
            avg_volatility: 0.0,
            market_p_up: self.clob_ws_prices(round, "Up").0,
            market_p_down: self.clob_ws_prices(round, "Down").0,
            current_asset_positions,
            timeframe_agreement: intel
                .map(|i| i.direction_agreement(prediction.direction == "Up"))
                .unwrap_or(0.5),
            same_direction_count,
            same_direction_exposure,
        };

        use polybot_risk::gate::GateResult;
        match self.risk_chain.evaluate(&signal) {
            GateResult::Pass => (true, "Gate chain: all passed".into(), 1.0),
            GateResult::Reject(reason) => (false, format!("Gate chain rejected: {}", reason), 0.0),
            GateResult::Resize { multiplier, reason } => {
                (true, format!("Gate chain resized: {}", reason), multiplier)
            }
        }
    }

    // === Position management (general pipeline) ===

    /// Update and manage exits for general-pipeline (non-crypto) positions.
    ///
    /// Exit conditions checked in order:
    /// 1. Market closed/resolved or no longer active
    /// 2. Edge reversal (model now favors the opposite direction)
    /// 3. Edge disappeared (|edge| dropped below half the entry threshold)
    /// 4. Take profit (price moved up by take_profit_pct, default 15%)
    /// 5. Stop loss (price moved down by stop_loss_pct, default 10%)
    /// 6. Stale position (no movement > 0.1% after 5 minutes)
    ///
    /// For live positions, submits SELL orders before removing from tracking.
    /// Failed SELLs keep the position tracked to avoid orphaning tokens.
    async fn update_positions(&mut self, market_map: &HashMap<String, &Market>) {
        let open_positions: Vec<Position> = self.state.positions.read().unwrap().clone();

        if open_positions.is_empty() {
            return;
        }

        // Crash recovery: if any position is live but executor is None, try creating it.
        if self.live_executor.is_none() && open_positions.iter().any(|p| p.is_live) {
            match polybot_executor::live::create_live_executor() {
                Ok(e) => {
                    tracing::info!("Live executor created for crash-recovered general live positions");
                    self.live_executor = Some(e);
                }
                Err(e) => {
                    tracing::warn!("Cannot create live executor for recovered general positions: {}", e);
                }
            }
        }

        let edge_threshold = self.risk.config().edge_threshold;
        let mut to_close: Vec<(usize, String, f64)> = Vec::new();

        for (idx, pos) in open_positions.iter().enumerate() {
            let market = market_map.get(&pos.market_id);

            let (current_price, market_active) = match market {
                Some(m) => {
                    // Use correct price for position direction:
                    // Yes → outcome_prices[0], No → outcome_prices[1] or (1 - prices[0])
                    let yes_price = m.outcome_prices.first().copied().unwrap_or(pos.current_price);
                    let price = if pos.direction == "No" {
                        m.outcome_prices.get(1).copied().unwrap_or(1.0 - yes_price)
                    } else {
                        yes_price
                    };
                    (price, m.active)
                }
                None => {
                    (pos.current_price, false)
                }
            };

            if !market_active {
                to_close.push((idx, "Market closed/resolved or no longer active".into(), current_price));
                continue;
            }

            if let Some(m) = market {
                if let Some(prediction) = self.composer.predict(m) {
                    let current_edge = prediction.edge;
                    let is_yes = pos.direction == "Yes";

                    let edge_reversed = if is_yes {
                        current_edge < 0.0
                    } else {
                        current_edge > 0.0
                    };

                    if edge_reversed {
                        to_close.push((idx, format!(
                            "Edge reversal: current edge {:.4} opposes {} position",
                            current_edge, pos.direction
                        ), current_price));
                        continue;
                    }

                    let edge_floor = edge_threshold / 2.0;
                    if current_edge.abs() < edge_floor {
                        to_close.push((idx, format!(
                            "Edge disappeared: |{:.4}| < floor {:.4}",
                            current_edge, edge_floor
                        ), current_price));
                        continue;
                    }
                }
            }

            // current_price is already the correct token price for this direction
            let price_move = (current_price - pos.entry_price) / pos.entry_price;

            if price_move >= self.take_profit_pct {
                to_close.push((idx, format!(
                    "Take profit: {:.1}% (threshold {:.0}%)",
                    price_move * 100.0, self.take_profit_pct * 100.0
                ), current_price));
                continue;
            }

            if price_move <= -self.stop_loss_pct {
                to_close.push((idx, format!(
                    "Stop loss: {:.1}% (threshold {:.0}%)",
                    price_move.abs() * 100.0, self.stop_loss_pct * 100.0
                ), current_price));
                continue;
            }

            let age_minutes = (chrono::Utc::now() - pos.opened_at).num_minutes();
            let abs_price_change = (current_price - pos.entry_price).abs();
            if age_minutes >= STALE_POSITION_MINUTES && abs_price_change < STALE_MOVEMENT_THRESHOLD {
                to_close.push((idx, format!(
                    "Stale: {}m with {:.4} movement",
                    age_minutes, abs_price_change
                ), current_price));
                continue;
            }
        }

        // Update prices on open positions
        {
            let mut positions = self.state.positions.write().unwrap();
            for pos in positions.iter_mut() {
                if let Some(m) = market_map.get(&pos.market_id) {
                    if let Some(&yes_price) = m.outcome_prices.first() {
                        // Use correct price for position direction
                        let price = if pos.direction == "No" {
                            m.outcome_prices.get(1).copied().unwrap_or(1.0 - yes_price)
                        } else {
                            yes_price
                        };
                        pos.current_price = price;
                        let shares = if pos.entry_price > 0.0 { pos.size / pos.entry_price } else { 0.0 };
                        pos.unrealized_pnl = (price - pos.entry_price) * shares;
                    }
                }
            }
        }

        // Close positions
        let mut close_indices: Vec<usize> = to_close.iter().map(|(idx, _, _)| *idx).collect();
        close_indices.sort_unstable();
        close_indices.dedup();

        let close_map: HashMap<usize, (String, f64)> = to_close.into_iter()
            .map(|(idx, reason, price)| (idx, (reason, price)))
            .collect();

        // Submit SELL orders for live general positions before removing
        let mut general_sell_failed: HashSet<usize> = HashSet::new();
        if self.live_executor.is_some() {
            let sell_data: Vec<(usize, String, ClobOrderRequest)>;
            {
                let positions = self.state.positions.read().unwrap();
                let mut data = Vec::new();
                for &idx in &close_indices {
                    if idx >= positions.len() { continue; }
                    let pos = &positions[idx];
                    if !pos.is_live { continue; }
                    let (_, exit_price) = match close_map.get(&idx) {
                        Some(v) => v,
                        None => continue,
                    };
                    let market = match market_map.get(&pos.market_id) {
                        Some(m) => m,
                        None => {
                            // Market vanished — keep position tracked, don't orphan tokens
                            tracing::warn!("Cannot SELL general live position {}: market vanished — keeping tracked", pos.question);
                            general_sell_failed.insert(idx);
                            continue;
                        }
                    };
                    if market.token_ids.len() < 2 {
                        general_sell_failed.insert(idx);
                        continue;
                    }
                    let token_id = if pos.direction == "Yes" { market.token_ids[0].clone() } else { market.token_ids[1].clone() };
                    let shares = if pos.entry_price > 0.0 { pos.size / pos.entry_price } else { 0.0 };
                    if shares <= 0.0 {
                        general_sell_failed.insert(idx);
                        continue;
                    }
                    data.push((idx, pos.question.clone(), ClobOrderRequest {
                        token_id,
                        price: *exit_price,
                        size_usd: shares,
                        order_type: ClobOrderType::Fok,
                        fee_rate_bps: market.fee_rate_bps,
                        side: ClobSide::Sell,
                        neg_risk: market.neg_risk,
                    }));
                }
                sell_data = data;
            };

            let executor = self.live_executor.as_ref().unwrap();
            for (idx, question, sell_request) in &sell_data {
                match executor.execute_order(sell_request).await {
                    Ok(resp) if resp.success => {
                        tracing::info!("LIVE SELL FILLED (general): {} order_id={:?}", question, resp.order_id);
                    }
                    Ok(resp) => {
                        tracing::warn!("LIVE SELL REJECTED (general): {} error={:?} — keeping tracked", question, resp.error_msg);
                        general_sell_failed.insert(*idx);
                    }
                    Err(e) => {
                        tracing::error!("LIVE SELL FAILED (general): {} err={} — keeping tracked", question, e);
                        general_sell_failed.insert(*idx);
                    }
                }
            }
        }
        close_indices.retain(|idx| !general_sell_failed.contains(idx));

        let mut closed_this_cycle: Vec<(Position, String)> = Vec::new();
        {
            let mut positions = self.state.positions.write().unwrap();
            for &idx in close_indices.iter().rev() {
                if idx < positions.len() {
                    let mut pos = positions.remove(idx);
                    let (reason, exit_price) = close_map.get(&idx).unwrap().clone();
                    pos.current_price = exit_price;
                    pos.closed_at = Some(chrono::Utc::now());
                    pos.exit_reason = Some(reason.clone());
                    let shares = if pos.entry_price > 0.0 { pos.size / pos.entry_price } else { 0.0 };
                    // Both Yes and No tokens: PnL = (exit - entry) * shares
                    // exit_price is already the correct token price for this direction
                    pos.unrealized_pnl = (exit_price - pos.entry_price) * shares;
                    closed_this_cycle.push((pos, reason));
                }
            }
        }

        for (pos, reason) in &closed_this_cycle {
            // Use real Polymarket fee formula (not flat fee_rate_bps which is for order signing)
            let is_resolution = reason.starts_with("Round ended") || reason.starts_with("Settlement");
            let fee = if is_resolution {
                self.cost_model.crypto_entry_cost(pos.size, pos.entry_price)
            } else {
                self.cost_model.crypto_round_trip_cost(pos.size, pos.entry_price, pos.current_price)
            };
            let pnl = pos.unrealized_pnl - fee;
            let trade_return = if pos.size > 0.0 { pnl / pos.size } else { 0.0 };

            self.risk.update_position_closed(pos.size, pnl);
            self.current_equity += pnl;
            if let Some(ref db) = self.state.db {
                let _ = db.save_equity("general", self.current_equity);
            }
            self.trade_returns.push(trade_return);

            let signal = Signal {
                id: uuid::Uuid::new_v4().to_string(),
                timestamp: chrono::Utc::now(),
                market_id: pos.market_id.clone(),
                question: pos.question.clone(),
                edge: pnl,
                z_score: 0.0,
                action: SignalAction::Exited,
                reason: format!("{} | PnL: ${:.2}", reason, pnl),
            };

            let mut signals = self.state.signals.write().unwrap();
            signals.push(signal);
            if signals.len() > 100 { signals.drain(0..50); }
            drop(signals);

            tracing::info!(
                "EXIT: {} dir={} entry={:.4} exit={:.4} pnl=${:.2} reason={}",
                pos.question, pos.direction, pos.entry_price, pos.current_price, pnl, reason
            );

            let _ = self.state.tx.send(serde_json::json!({
                "event": "position_closed",
                "market": pos.question,
                "direction": pos.direction,
                "entry_price": pos.entry_price,
                "exit_price": pos.current_price,
                "pnl": pnl,
                "reason": reason,
            }).to_string());
        }

        if !closed_this_cycle.is_empty() {
            let mut closed = self.state.closed_positions.write().unwrap();
            for (pos, _) in closed_this_cycle {
                closed.push(pos);
            }
            if closed.len() > MAX_CLOSED_HISTORY {
                let drain_count = closed.len() - MAX_CLOSED_HISTORY;
                closed.drain(0..drain_count);
            }
            drop(closed);

            self.recompute_metrics();
        }
    }

    /// Recompute general-pipeline performance metrics from trade_returns.
    ///
    /// Computed metrics:
    /// - Total trades, wins, losses, win rate
    /// - Total PnL (session-scoped: current_equity - session_start_equity)
    /// - Per-trade Sharpe ratio (mean/std, NOT annualized — correct for fleet comparison)
    /// - Profit factor (gross_profit / gross_loss)
    /// - Max drawdown (peak-to-trough of equity curve)
    /// - Equity curve (normalized to initial bankroll)
    fn recompute_metrics(&self) {
        let returns = &self.trade_returns;
        if returns.is_empty() { return; }

        let total_trades = returns.len() as u64;
        let wins = returns.iter().filter(|&&r| r > 0.0).count() as u64;
        let losses = total_trades - wins;
        let win_rate = if total_trades > 0 { wins as f64 / total_trades as f64 } else { 0.0 };

        let total_pnl = self.current_equity - self.session_start_equity;

        let mean_return = returns.iter().sum::<f64>() / returns.len() as f64;
        let variance = if returns.len() > 1 {
            returns.iter().map(|r| (r - mean_return).powi(2)).sum::<f64>() / (returns.len() - 1) as f64
        } else {
            0.0
        };
        let std_return = variance.sqrt();
        // Per-trade Sharpe: mean/std without annualization (correct for fleet comparison)
        let sharpe_ratio = if std_return > 0.0 {
            mean_return / std_return
        } else if mean_return > 0.0 {
            999.0
        } else {
            0.0
        };

        let gross_profit: f64 = returns.iter().filter(|&&r| r > 0.0).sum();
        let gross_loss: f64 = returns.iter().filter(|&&r| r < 0.0).map(|r| r.abs()).sum();
        let profit_factor = if gross_loss > 0.0 {
            gross_profit / gross_loss
        } else if gross_profit > 0.0 {
            999.0
        } else {
            0.0
        };

        let equity_ratio = self.current_equity / self.bankroll;
        let mut equity_curve = self.state.metrics.read().unwrap().equity_curve.clone();
        equity_curve.push(equity_ratio);

        let max_drawdown = {
            let mut peak = equity_curve[0];
            let mut max_dd = 0.0;
            for &val in &equity_curve {
                if val > peak { peak = val; }
                let dd = (peak - val) / peak;
                if dd > max_dd { max_dd = dd; }
            }
            max_dd
        };

        let mut metrics = self.state.metrics.write().unwrap();
        metrics.total_trades = total_trades;
        metrics.wins = wins;
        metrics.losses = losses;
        metrics.win_rate = win_rate;
        metrics.sharpe_ratio = sharpe_ratio;
        metrics.max_drawdown = max_drawdown;
        metrics.profit_factor = profit_factor;
        metrics.total_pnl = total_pnl;
        metrics.equity_curve = equity_curve;
    }

    /// Recompute crypto-specific metrics (separate from general pipeline)
    fn recompute_crypto_metrics(&self) {
        let returns = &self.trade_returns;
        if returns.is_empty() { return; }

        let total_trades = returns.len() as u64;
        let wins = returns.iter().filter(|&&r| r > 0.0).count() as u64;
        let losses = total_trades - wins;
        let win_rate = if total_trades > 0 { wins as f64 / total_trades as f64 } else { 0.0 };

        let total_pnl = self.current_equity - self.session_start_equity;

        let mean_return = returns.iter().sum::<f64>() / returns.len() as f64;
        let variance = if returns.len() > 1 {
            returns.iter().map(|r| (r - mean_return).powi(2)).sum::<f64>() / (returns.len() - 1) as f64
        } else { 0.0 };
        let std_return = variance.sqrt();
        // Per-trade Sharpe: mean/std without annualization (correct for fleet comparison)
        let sharpe_ratio = if std_return > 0.0 {
            mean_return / std_return
        } else if mean_return > 0.0 { 999.0 } else { 0.0 };

        let gross_profit: f64 = returns.iter().filter(|&&r| r > 0.0).sum();
        let gross_loss: f64 = returns.iter().filter(|&&r| r < 0.0).map(|r| r.abs()).sum();
        let profit_factor = if gross_loss > 0.0 {
            gross_profit / gross_loss
        } else if gross_profit > 0.0 { 999.0 } else { 0.0 };

        let equity_ratio = self.current_equity / self.bankroll;
        let mut equity_curve = self.state.crypto_metrics.read().unwrap().equity_curve.clone();
        equity_curve.push(equity_ratio);

        let max_drawdown = {
            let mut peak = equity_curve.first().copied().unwrap_or(1.0);
            let mut max_dd = 0.0;
            for &val in &equity_curve {
                if val > peak { peak = val; }
                let dd = (peak - val) / peak;
                if dd > max_dd { max_dd = dd; }
            }
            max_dd
        };

        let mut metrics = self.state.crypto_metrics.write().unwrap();
        metrics.total_trades = total_trades;
        metrics.wins = wins;
        metrics.losses = losses;
        metrics.win_rate = win_rate;
        metrics.sharpe_ratio = sharpe_ratio;
        metrics.max_drawdown = max_drawdown;
        metrics.profit_factor = profit_factor;
        metrics.total_pnl = total_pnl;
        metrics.equity_curve = equity_curve;
        let up_preds = metrics.up_predictions;
        let down_preds = metrics.down_predictions;
        drop(metrics);

        if (up_preds + down_preds) % 100 == 0 && up_preds + down_preds > 0 {
            let total = up_preds + down_preds;
            tracing::info!(
                "Prediction distribution: Up={} ({:.1}%) Down={} ({:.1}%)",
                up_preds, up_preds as f64 / total as f64 * 100.0,
                down_preds, down_preds as f64 / total as f64 * 100.0,
            );
        }

        // Persist metrics snapshot to database
        if let Some(ref db) = self.state.db {
            let _ = db.insert_metrics_snapshot("crypto", total_pnl, win_rate, sharpe_ratio, total_trades as i64);
        }
    }
}

/// Detect whether a Polymarket market question is about crypto Up/Down binary rounds.
///
/// Used by the general pipeline to exclude crypto markets (handled by crypto pipeline).
/// Matches on: BTC/ETH/SOL/XRP keywords AND "up or down"/"updown"/"higher or lower".
/// This is a simple heuristic — Polymarket question format is fairly consistent.
fn is_crypto_market(question: &str) -> bool {
    let q = question.to_lowercase();
    let crypto_keywords = ["btc", "eth", "sol", "xrp", "bitcoin", "ethereum", "solana", "ripple"];
    let updown_keywords = ["up or down", "updown", "higher or lower"];
    crypto_keywords.iter().any(|k| q.contains(k)) && updown_keywords.iter().any(|k| q.contains(k))
}

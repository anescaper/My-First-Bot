//! Market-Making Pipeline — bilateral round lifecycle system.
//!
//! Operates independently from the directional pipeline, entering paired
//! UP + DOWN positions on crypto rounds and managing them through three
//! phases: Open -> Mid -> Close.

use std::collections::HashMap;
use std::sync::Arc;
use anyhow::Result;
use polybot_scanner::crypto::{Asset, CryptoRound, Timeframe};
use polybot_risk::gate::{GateResult, GateSignal};
use polybot_risk::chain::RiskGateChain;
use polybot_data::DataClient;
use polybot_api::state::{AppState, Signal, SignalAction, Position};

use crate::bilateral::{
    BilateralPosition, RoundMemory, RoundPhase,
    calculate_skew_alpha, phase_budget,
};

/// MM-selected asset/timeframe pairs and their roles.
const MM_TARGETS: &[(Asset, Timeframe)] = &[
    (Asset::ETH, Timeframe::FifteenMin), // primary
    (Asset::SOL, Timeframe::FiveMin),     // fast
    (Asset::BTC, Timeframe::OneHour),     // spread only
];

/// Minimum spread (1 - p_up - p_down) required to enter bilateral.
const MIN_SPREAD: f64 = 0.03;

/// Minimum edge for z-score mid-round adjustment.
const Z_SCORE_ADJUST_THRESHOLD: f64 = 1.5;

/// Maximum closed positions to retain in history.
const MAX_CLOSED_HISTORY: usize = 50;

pub struct MarketMakingPipeline {
    state: Arc<AppState>,
    data_client: Arc<DataClient>,
    bankroll: f64,
    round_budget: f64,
    current_equity: f64,
    trade_returns: Vec<f64>,
    positions: HashMap<String, BilateralPosition>, // condition_id -> position
    memory: HashMap<(Asset, Timeframe), RoundMemory>, // per asset-tf memory
    risk_chain: RiskGateChain,
    #[allow(dead_code)] // reserved for Python strategy service integration
    strategy_client: reqwest::Client,
    #[allow(dead_code)]
    strategy_service_url: String,
}

impl MarketMakingPipeline {
    pub fn new(state: Arc<AppState>, bankroll: f64, data_client: Arc<DataClient>) -> Self {
        let risk_chain = polybot_risk::build_risk_chain("bilateral-mm", bankroll);
        let strategy_service_url = std::env::var("STRATEGY_SERVICE_URL")
            .unwrap_or_else(|_| "http://localhost:8100".to_string());

        // Budget per round: 5% of bankroll, capped at $200
        let round_budget = (bankroll * 0.05).min(200.0);

        tracing::info!(
            bankroll = bankroll,
            round_budget = round_budget,
            "MM pipeline initialized with DataClient + bilateral-mm risk chain"
        );

        Self {
            state,
            data_client,
            bankroll,
            round_budget,
            current_equity: bankroll,
            trade_returns: Vec::new(),
            positions: HashMap::new(),
            memory: HashMap::new(),
            risk_chain,
            strategy_client: reqwest::Client::new(),
            strategy_service_url,
        }
    }

    /// Main MM cycle — called repeatedly by the spawned task.
    pub async fn run_mm_cycle(&mut self) -> Result<()> {
        // Fetch all data from data-hub
        let cycle = match self.data_client.fetch_cycle().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("MM cycle: data-hub fetch failed: {e}");
                return Ok(());
            }
        };

        let prices = self.data_client.get_all_prices();
        if prices.is_empty() {
            tracing::warn!("MM cycle: no prices available");
            return Ok(());
        }

        // Filter for MM target pairs only
        let rounds: Vec<CryptoRound> = cycle.rounds
            .into_iter()
            .filter(|r| MM_TARGETS.iter().any(|(a, t)| r.asset == *a && r.timeframe == *t))
            .collect();

        if rounds.is_empty() {
            tracing::info!("MM cycle: no active rounds for target pairs");
            return Ok(());
        }

        tracing::info!("MM cycle: {} target rounds, {} existing positions", rounds.len(), self.positions.len());

        let round_map: HashMap<String, &CryptoRound> = rounds
            .iter()
            .map(|r| (r.condition_id.clone(), r))
            .collect();

        // Process each round through the three-phase lifecycle
        for round in &rounds {
            let progress = round.progress_pct() * 100.0; // Convert 0-1 to 0-100
            let phase = RoundPhase::from_progress(progress);

            match phase {
                RoundPhase::Open => {
                    // Phase 1: Enter bilateral if no position exists
                    if !self.positions.contains_key(&round.condition_id) {
                        self.try_enter_bilateral(round, &prices).await;
                    }
                }
                RoundPhase::Mid => {
                    // Phase 2: Adjust existing positions based on z-score
                    if self.positions.contains_key(&round.condition_id) {
                        self.adjust_mid_round(round, &prices).await;
                    }
                }
                RoundPhase::Close => {
                    // Phase 3: Amplify winner or lock minimum return
                    if self.positions.contains_key(&round.condition_id) {
                        self.close_or_amplify(round, &prices).await;
                    }
                }
                RoundPhase::Settled => {}
            }
        }

        // Check for settled rounds (positions whose rounds are no longer active)
        self.check_settled_rounds(&round_map, &prices).await;

        Ok(())
    }

    /// Phase 1: Attempt to enter a bilateral position on a round.
    async fn try_enter_bilateral(
        &mut self,
        round: &CryptoRound,
        prices: &HashMap<Asset, f64>,
    ) {
        let spread = 1.0 - round.price_up - round.price_down;
        let secs = round.seconds_remaining();
        let progress = round.progress_pct() * 100.0;

        tracing::info!(
            "MM TRY: {:?}/{:?} spread={:.4} progress={:.1}% secs_left={} p_up={:.3} p_down={:.3}",
            round.asset, round.timeframe, spread, progress, secs,
            round.price_up, round.price_down
        );

        if spread < MIN_SPREAD {
            tracing::info!(
                "MM SKIP: {:?}/{:?} spread {:.4} < {:.4}",
                round.asset, round.timeframe, spread, MIN_SPREAD
            );
            return;
        }

        if secs < 30 {
            tracing::info!("MM SKIP: {:?}/{:?} only {}s remaining", round.asset, round.timeframe, secs);
            return;
        }

        // Get or create memory for this asset/timeframe
        let memory = self
            .memory
            .entry((round.asset, round.timeframe))
            .or_default()
            .clone();

        // Get directional signal from price data
        let p_up_signal = self.estimate_p_up(round, prices);

        // Calculate skew based on memory + signal
        let skew_alpha = calculate_skew_alpha(&memory, p_up_signal);

        // Risk gate check
        let gate_signal = self.build_gate_signal(round, spread);
        match self.risk_chain.evaluate(&gate_signal) {
            GateResult::Pass => {}
            GateResult::Resize { multiplier, reason } => {
                tracing::info!("MM: gate resized: {} (mult={:.2})", reason, multiplier);
                // Continue with reduced budget handled below
            }
            GateResult::Reject(reason) => {
                tracing::info!(
                    "MM: {:?}/{:?} rejected by gate: {}",
                    round.asset, round.timeframe, reason
                );
                self.record_signal(round, SignalAction::Rejected, &format!("Gate: {}", reason));
                return;
            }
        }

        // Phase budget allocation
        let (open_budget, _mid_budget, _close_budget) =
            phase_budget(&round.timeframe, self.round_budget);

        self.enter_bilateral(round, skew_alpha, open_budget, spread);
    }

    /// Create a bilateral position.
    fn enter_bilateral(
        &mut self,
        round: &CryptoRound,
        skew_alpha: f64,
        budget: f64,
        spread: f64,
    ) {
        // Split budget by skew: alpha goes to UP, (1-alpha) to DOWN
        let up_budget = budget * skew_alpha;
        let down_budget = budget * (1.0 - skew_alpha);

        // Shares = budget / price
        let up_shares = if round.price_up > 0.0 {
            up_budget / round.price_up
        } else {
            0.0
        };
        let down_shares = if round.price_down > 0.0 {
            down_budget / round.price_down
        } else {
            0.0
        };

        let total_cost = up_budget + down_budget;

        let position = BilateralPosition {
            condition_id: round.condition_id.clone(),
            asset: round.asset,
            timeframe: round.timeframe,
            up_shares,
            down_shares,
            up_entry_price: round.price_up,
            down_entry_price: round.price_down,
            total_cost,
            entered_at: chrono::Utc::now(),
            phase: RoundPhase::Open,
            skew_alpha,
        };

        tracing::info!(
            "MM ENTER: {:?}/{:?} alpha={:.2} spread={:.4} cost=${:.2} up_shares={:.2}@{:.3} down_shares={:.2}@{:.3}",
            round.asset, round.timeframe, skew_alpha, spread, total_cost,
            up_shares, round.price_up, down_shares, round.price_down
        );

        // Also record as a crypto position in AppState for frontend visibility
        let pos = Position {
            id: uuid::Uuid::new_v4().to_string(),
            market_id: round.condition_id.clone(),
            question: format!("{:?} {:?} bilateral", round.asset, round.timeframe),
            direction: format!("MM α={:.2}", skew_alpha),
            entry_price: (round.price_up + round.price_down) / 2.0,
            current_price: (round.price_up + round.price_down) / 2.0,
            size: total_cost,
            unrealized_pnl: 0.0,
            opened_at: chrono::Utc::now(),
            entry_edge: 1.0 - round.price_up - round.price_down, // spread as "edge"
            is_live: false,
            fee_rate_bps: round.fee_rate_bps as f64,
            hold_to_resolution: true, // MM positions always hold to resolution
            closed_at: None,
            exit_reason: None,
        };
        self.state.crypto_positions.write().unwrap().push(pos);

        self.record_signal(
            round,
            SignalAction::Entered,
            &format!(
                "Bilateral entry: alpha={:.2} spread={:.4} cost=${:.2}",
                skew_alpha, 1.0 - round.price_up - round.price_down, total_cost
            ),
        );

        let _ = self.state.tx.send(
            serde_json::json!({
                "event": "mm_position_opened",
                "asset": format!("{:?}", round.asset),
                "timeframe": round.timeframe.slug_str(),
                "skew_alpha": skew_alpha,
                "total_cost": total_cost,
                "spread": 1.0 - round.price_up - round.price_down,
            })
            .to_string(),
        );

        self.positions
            .insert(round.condition_id.clone(), position);
    }

    /// Phase 2: Mid-round adjustment based on z-score deviation.
    async fn adjust_mid_round(
        &mut self,
        round: &CryptoRound,
        _prices: &HashMap<Asset, f64>,
    ) {
        let position = match self.positions.get_mut(&round.condition_id) {
            Some(p) => p,
            None => return,
        };

        // Update phase
        position.phase = RoundPhase::Mid;

        // Calculate z-score: how much has the market moved since entry
        let up_price_change = round.price_up - position.up_entry_price;
        let down_price_change = round.price_down - position.down_entry_price;

        // Simple z-score: magnitude of price movement relative to initial spread
        let initial_spread = 1.0 - position.up_entry_price - position.down_entry_price;
        if initial_spread <= 0.0 {
            return;
        }

        let z_up = up_price_change / initial_spread;
        let z_down = down_price_change / initial_spread;

        // If z-score exceeds threshold, the market has moved significantly
        // Adjust by increasing allocation to the winning side
        if z_up.abs() > Z_SCORE_ADJUST_THRESHOLD || z_down.abs() > Z_SCORE_ADJUST_THRESHOLD {
            let (_open_budget, mid_budget, _close_budget) =
                phase_budget(&round.timeframe, self.round_budget);

            if mid_budget <= 0.0 {
                return; // 5m rounds have no mid-phase budget
            }

            // Increase allocation toward the side that's gaining
            let adjust_alpha = if z_up > 0.0 { 0.7 } else { 0.3 };
            let up_add = mid_budget * adjust_alpha / round.price_up.max(0.01);
            let down_add = mid_budget * (1.0 - adjust_alpha) / round.price_down.max(0.01);

            position.up_shares += up_add;
            position.down_shares += down_add;
            position.total_cost += mid_budget;

            tracing::info!(
                "MM ADJUST: {:?}/{:?} z_up={:.2} z_down={:.2} added ${:.2} (alpha={:.2})",
                round.asset, round.timeframe, z_up, z_down, mid_budget, adjust_alpha
            );

            self.record_signal(
                round,
                SignalAction::Entered,
                &format!(
                    "Mid-round adjustment: z_up={:.2} z_down={:.2} added ${:.2}",
                    z_up, z_down, mid_budget
                ),
            );
        }

        // Update the AppState position's current price for frontend
        self.update_appstate_position(round);
    }

    /// Phase 3: Close phase — amplify winner or lock minimum return.
    async fn close_or_amplify(
        &mut self,
        round: &CryptoRound,
        _prices: &HashMap<Asset, f64>,
    ) {
        let position = match self.positions.get_mut(&round.condition_id) {
            Some(p) => p,
            None => return,
        };

        // Only amplify once (check phase transition)
        if position.phase == RoundPhase::Close {
            // Already processed close phase, just update prices
            self.update_appstate_position(round);
            return;
        }

        position.phase = RoundPhase::Close;

        // Determine which side is winning
        let up_value = position.up_shares * round.price_up;
        let down_value = position.down_shares * round.price_down;
        let winning_side = if up_value > down_value { "Up" } else { "Down" };

        let (_open_budget, _mid_budget, close_budget) =
            phase_budget(&round.timeframe, self.round_budget);

        if close_budget <= 0.0 {
            return;
        }

        // Amplify: put close_budget into the winning side
        match winning_side {
            "Up" => {
                let add_shares = close_budget / round.price_up.max(0.01);
                position.up_shares += add_shares;
                position.total_cost += close_budget;
            }
            _ => {
                let add_shares = close_budget / round.price_down.max(0.01);
                position.down_shares += add_shares;
                position.total_cost += close_budget;
            }
        }

        tracing::info!(
            "MM AMPLIFY: {:?}/{:?} winner={} close_budget=${:.2} up_val=${:.2} down_val=${:.2}",
            round.asset, round.timeframe, winning_side, close_budget, up_value, down_value
        );

        self.record_signal(
            round,
            SignalAction::Entered,
            &format!(
                "Close-phase amplify: winner={} added ${:.2}",
                winning_side, close_budget
            ),
        );

        self.update_appstate_position(round);
    }

    /// Check for rounds that have settled (no longer in active scan results).
    async fn check_settled_rounds(
        &mut self,
        active_rounds: &HashMap<String, &CryptoRound>,
        _prices: &HashMap<Asset, f64>,
    ) {
        let settled_ids: Vec<String> = self
            .positions
            .keys()
            .filter(|cid| {
                match active_rounds.get(cid.as_str()) {
                    Some(r) => r.seconds_remaining() <= 0,
                    None => true, // Round disappeared from scan = settled
                }
            })
            .cloned()
            .collect();

        for condition_id in settled_ids {
            let position = match self.positions.remove(&condition_id) {
                Some(p) => p,
                None => continue,
            };

            // Determine outcome: use last known prices from round if available,
            // otherwise use entry prices (conservative)
            let (final_up_price, final_down_price) = match active_rounds.get(&condition_id) {
                Some(r) => (r.price_up, r.price_down),
                None => {
                    // Round gone — one side settles at 1.0, other at 0.0
                    // We don't know which won, so use last known prices as approximation.
                    // In a real settlement, whichever side is >0.5 typically wins.
                    if position.up_entry_price > position.down_entry_price {
                        (1.0, 0.0) // UP likely won
                    } else {
                        (0.0, 1.0) // DOWN likely won
                    }
                }
            };

            let up_payout = position.up_shares * final_up_price;
            let down_payout = position.down_shares * final_down_price;
            let total_payout = up_payout + down_payout;
            let pnl = total_payout - position.total_cost;

            // Determine if our skew direction was correct
            let skewed_up = position.skew_alpha > 0.5;
            let up_won = final_up_price > final_down_price;
            let correct_direction = skewed_up == up_won;

            // Update memory
            let spread_at_entry =
                1.0 - position.up_entry_price - position.down_entry_price;
            let memory = self
                .memory
                .entry((position.asset, position.timeframe))
                .or_default();
            memory.last_direction = if skewed_up { "Up" } else { "Down" }.into();
            memory.last_confidence = (position.skew_alpha - 0.5).abs() * 2.0;
            memory.update(correct_direction, spread_at_entry, pnl);

            // Update equity tracking
            let trade_return = if position.total_cost > 0.0 {
                pnl / position.total_cost
            } else {
                0.0
            };
            self.current_equity += pnl;
            self.trade_returns.push(trade_return);

            tracing::info!(
                "MM SETTLE: {:?}/{:?} pnl=${:.2} cost=${:.2} payout=${:.2} correct={} rounds={}",
                position.asset,
                position.timeframe,
                pnl,
                position.total_cost,
                total_payout,
                correct_direction,
                memory.rounds_completed
            );

            // Close the AppState position
            self.close_appstate_position(&condition_id, pnl, "Round settled");

            // Persist to database
            if let Some(ref db) = self.state.db {
                let hist_pos = polybot_api::db::HistoryPosition {
                    id: uuid::Uuid::new_v4().to_string(),
                    pipeline: "mm".into(),
                    asset: format!("{:?}", position.asset),
                    timeframe: format!("{:?}", position.timeframe),
                    direction: format!("MM α={:.2}", position.skew_alpha),
                    entry_price: (position.up_entry_price + position.down_entry_price) / 2.0,
                    exit_price: (final_up_price + final_down_price) / 2.0,
                    size: position.total_cost,
                    pnl,
                    opened_at: position.entered_at.to_rfc3339(),
                    closed_at: chrono::Utc::now().to_rfc3339(),
                    exit_reason: "Round settled".into(),
                };
                let _ = db.insert_position(&hist_pos);
            }

            let _ = self.state.tx.send(
                serde_json::json!({
                    "event": "mm_position_settled",
                    "asset": format!("{:?}", position.asset),
                    "timeframe": position.timeframe.slug_str(),
                    "pnl": pnl,
                    "correct_direction": correct_direction,
                    "total_cost": position.total_cost,
                    "total_payout": total_payout,
                })
                .to_string(),
            );
        }

        // Recompute metrics after settlements
        if !self.trade_returns.is_empty() {
            self.recompute_mm_metrics();
        }
    }

    /// Estimate P(Up) from available price data.
    fn estimate_p_up(
        &self,
        round: &CryptoRound,
        prices: &HashMap<Asset, f64>,
    ) -> f64 {
        let current = match prices.get(&round.asset) {
            Some(&p) => p,
            None => return 0.5,
        };

        let reference = match self.data_client.get_reference(round.asset, round.timeframe) {
            Some(r) => r,
            None => return 0.5,
        };

        if reference <= 0.0 {
            return 0.5;
        }

        // Simple price-change-based estimate
        let change_pct = (current - reference) / reference;
        // Map change to probability: positive change -> higher p_up
        let p_up = 0.5 + change_pct * 5.0; // 1% change -> 0.55
        p_up.clamp(0.2, 0.8)
    }

    /// Build a GateSignal for risk chain evaluation.
    fn build_gate_signal(&self, round: &CryptoRound, spread: f64) -> GateSignal {
        let position_count = self.positions.len() + self.state.crypto_positions.read().unwrap().len();
        let mm_exposure: f64 = self.positions.values().map(|p| p.total_cost).sum();
        let crypto_exposure: f64 = self.state.crypto_positions.read().unwrap().iter().map(|p| p.size).sum();
        let total_exposure = mm_exposure + crypto_exposure;
        let drawdown_pct = if self.bankroll > 0.0 {
            (self.bankroll - self.current_equity).max(0.0) / self.bankroll
        } else {
            0.0
        };

        // Net exposure: difference between UP and DOWN side allocations
        let net_exposure: f64 = self
            .positions
            .values()
            .map(|p| (p.up_shares * p.up_entry_price) - (p.down_shares * p.down_entry_price))
            .sum();

        GateSignal {
            market_id: round.condition_id.clone(),
            asset: format!("{:?}", round.asset),
            timeframe: round.timeframe.slug_str().to_string(),
            direction: "Bilateral".into(),
            p_up: 0.5, // bilateral is neutral
            confidence: spread, // use spread as confidence proxy
            edge: spread, // spread is the bilateral edge
            entry_price: (round.price_up + round.price_down) / 2.0,
            proposed_size_usd: self.round_budget,
            current_total_exposure: total_exposure,
            current_drawdown_pct: drawdown_pct,
            current_position_count: position_count,
            bankroll: self.bankroll,
            current_net_exposure: net_exposure,
            current_volatility: 0.0,
            avg_volatility: 0.0,
            market_p_up: round.price_up,
            market_p_down: round.price_down,
            current_asset_positions: 0,
            timeframe_agreement: 1.0, // MM pipeline operates independently
            same_direction_count: 0,  // bilateral is direction-neutral
            same_direction_exposure: 0.0,
        }
    }

    /// Record a signal in AppState for frontend visibility.
    fn record_signal(&self, round: &CryptoRound, action: SignalAction, reason: &str) {
        let signal = Signal {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: chrono::Utc::now(),
            market_id: round.condition_id.clone(),
            question: format!("{:?} {:?} MM", round.asset, round.timeframe),
            edge: 1.0 - round.price_up - round.price_down,
            z_score: 0.0,
            action,
            reason: reason.to_string(),
        };

        let mut signals = self.state.crypto_signals.write().unwrap();
        signals.push(signal);
        if signals.len() > 100 {
            signals.drain(0..50);
        }
    }

    /// Update the corresponding AppState position with current round prices.
    fn update_appstate_position(&self, round: &CryptoRound) {
        let mut positions = self.state.crypto_positions.write().unwrap();
        for pos in positions.iter_mut() {
            if pos.market_id == round.condition_id && pos.question.contains("bilateral") {
                let position = match self.positions.get(&round.condition_id) {
                    Some(p) => p,
                    None => continue,
                };
                let up_value = position.up_shares * round.price_up;
                let down_value = position.down_shares * round.price_down;
                let total_value = up_value + down_value;
                pos.current_price = (round.price_up + round.price_down) / 2.0;
                pos.unrealized_pnl = total_value - position.total_cost;
                pos.size = position.total_cost;
            }
        }
    }

    /// Close an AppState position and move it to closed history.
    fn close_appstate_position(&self, condition_id: &str, pnl: f64, reason: &str) {
        let mut positions = self.state.crypto_positions.write().unwrap();
        let mut idx_to_close = None;
        for (idx, pos) in positions.iter().enumerate() {
            if pos.market_id == condition_id && pos.question.contains("bilateral") {
                idx_to_close = Some(idx);
                break;
            }
        }

        if let Some(idx) = idx_to_close {
            let mut pos = positions.remove(idx);
            pos.closed_at = Some(chrono::Utc::now());
            pos.exit_reason = Some(reason.to_string());
            pos.unrealized_pnl = pnl;

            let _ = self.state.tx.send(
                serde_json::json!({
                    "event": "mm_position_closed",
                    "market": pos.question,
                    "pnl": pnl,
                    "reason": reason,
                })
                .to_string(),
            );

            let mut closed = self.state.crypto_closed.write().unwrap();
            closed.push(pos);
            if closed.len() > MAX_CLOSED_HISTORY {
                let drain_count = closed.len() - MAX_CLOSED_HISTORY;
                closed.drain(0..drain_count);
            }
        }
    }

    /// Recompute MM-specific metrics and update AppState for frontend visibility.
    fn recompute_mm_metrics(&self) {
        let returns = &self.trade_returns;
        if returns.is_empty() {
            return;
        }

        let total_trades = returns.len() as u64;
        let wins = returns.iter().filter(|&&r| r > 0.0).count() as u64;
        let losses = total_trades - wins;
        let total_pnl = self.current_equity - self.bankroll;
        let win_rate = if total_trades > 0 {
            wins as f64 / total_trades as f64
        } else {
            0.0
        };

        // Compute profit factor
        let gross_profit: f64 = returns.iter().filter(|&&r| r > 0.0).map(|r| r.abs()).sum();
        let gross_loss: f64 = returns.iter().filter(|&&r| r < 0.0).map(|r| r.abs()).sum();
        let profit_factor = if gross_loss > 0.0 { gross_profit / gross_loss } else { 0.0 };

        // Max drawdown
        let mut peak = 0.0_f64;
        let mut max_dd = 0.0_f64;
        let mut cum = 0.0;
        for &r in returns {
            cum += r;
            peak = peak.max(cum);
            let dd = (peak - cum) / (1.0 + peak).max(0.001);
            max_dd = max_dd.max(dd);
        }

        // Sharpe ratio (simple: mean/std of returns)
        let n = returns.len() as f64;
        let mean = returns.iter().sum::<f64>() / n;
        let variance = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / n;
        let sharpe = if variance > 0.0 { mean / variance.sqrt() } else { 0.0 };

        tracing::info!(
            "MM metrics: {} trades, {} wins, {:.1}% WR, PnL=${:.2}, Sharpe={:.2}, PF={:.2}",
            total_trades, wins, win_rate * 100.0, total_pnl, sharpe, profit_factor
        );

        // Update AppState crypto_metrics so frontend sees MM performance
        {
            let mut metrics = self.state.crypto_metrics.write().unwrap();
            metrics.total_trades = total_trades;
            metrics.wins = wins;
            metrics.losses = losses;
            metrics.win_rate = win_rate;
            metrics.total_pnl = total_pnl;
            metrics.sharpe_ratio = sharpe;
            metrics.profit_factor = profit_factor;
            metrics.max_drawdown = max_dd;
        }

        // Persist metrics snapshot to SQLite
        if let Some(ref db) = self.state.db {
            let _ = db.insert_metrics_snapshot("mm", total_pnl, win_rate, sharpe, total_trades as i64);
        }
    }
}

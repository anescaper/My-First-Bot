//! Backtest engine for replaying historical rounds through strategy pipeline

use crate::db::RoundHistory;
use serde::{Deserialize, Serialize};

const BACKTEST_EDGE_THRESHOLD: f64 = 0.035;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestTrade {
    pub asset: String,
    pub timeframe: String,
    pub direction: String,
    pub entry_edge: f64,
    pub pnl: f64,
    pub won: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestResult {
    pub total_trades: u64,
    pub wins: u64,
    pub losses: u64,
    pub win_rate: f64,
    pub total_pnl: f64,
    pub max_drawdown: f64,
    pub sharpe_ratio: f64,
    pub trades: Vec<BacktestTrade>,
    pub equity_curve: Vec<f64>,
}

pub struct BacktestEngine;

impl BacktestEngine {
    pub fn new() -> Self {
        Self
    }

    pub fn run(&self, rounds: &[RoundHistory]) -> BacktestResult {
        let mut trades = Vec::new();
        let mut equity = 0.0_f64;
        let mut equity_curve = vec![0.0];
        let mut peak = 0.0_f64;
        let mut max_drawdown = 0.0_f64;

        for round in rounds {
            if round.reference_price <= 0.0 {
                continue;
            }

            let actual_up = round.close_price >= round.reference_price;
            let model_direction = if round.our_p_up > round.market_p_up { "Up" } else { "Down" };
            let edge = round.edge;

            if edge < BACKTEST_EDGE_THRESHOLD {
                continue;
            }

            let won = (model_direction == "Up" && actual_up) || (model_direction == "Down" && !actual_up);
            let pnl = if won { edge } else { -edge };

            equity += pnl;
            equity_curve.push(equity);
            if equity > peak { peak = equity; }
            let dd = if peak > 0.0 { (peak - equity) / peak } else { 0.0 };
            if dd > max_drawdown { max_drawdown = dd; }

            trades.push(BacktestTrade {
                asset: round.asset.clone(),
                timeframe: round.timeframe.clone(),
                direction: model_direction.to_string(),
                entry_edge: edge,
                pnl,
                won,
            });
        }

        let total_trades = trades.len() as u64;
        let wins = trades.iter().filter(|t| t.won).count() as u64;
        let losses = total_trades - wins;
        let win_rate = if total_trades > 0 { wins as f64 / total_trades as f64 } else { 0.0 };
        let total_pnl = equity;

        let returns: Vec<f64> = trades.iter().map(|t| t.pnl).collect();
        let mean = if !returns.is_empty() { returns.iter().sum::<f64>() / returns.len() as f64 } else { 0.0 };
        let variance = if returns.len() > 1 {
            returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (returns.len() - 1) as f64
        } else { 0.0 };
        let std_dev = variance.sqrt();
        let sharpe_ratio = if std_dev > 0.0 { mean / std_dev } else { 0.0 };

        BacktestResult {
            total_trades,
            wins,
            losses,
            win_rate,
            total_pnl,
            max_drawdown,
            sharpe_ratio,
            trades,
            equity_curve,
        }
    }
}

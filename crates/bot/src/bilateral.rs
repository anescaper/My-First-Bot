use serde::{Deserialize, Serialize};
use polybot_scanner::crypto::{Asset, Timeframe};

/// A bilateral position: paired UP + DOWN orders on the same round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BilateralPosition {
    pub condition_id: String,
    pub asset: Asset,
    pub timeframe: Timeframe,
    pub up_shares: f64,
    pub down_shares: f64,
    pub up_entry_price: f64,
    pub down_entry_price: f64,
    pub total_cost: f64,
    pub entered_at: chrono::DateTime<chrono::Utc>,
    pub phase: RoundPhase,
    pub skew_alpha: f64, // 0.5 = neutral, >0.5 = skewed to UP
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum RoundPhase {
    Open,    // First 10% of round
    Mid,     // 10-85% of round
    Close,   // Last 15% of round
    Settled,
}

impl RoundPhase {
    pub fn from_progress(progress_pct: f64) -> Self {
        if progress_pct < 25.0 {
            RoundPhase::Open
        } else if progress_pct < 85.0 {
            RoundPhase::Mid
        } else {
            RoundPhase::Close
        }
    }
}

/// Cross-round learning state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundMemory {
    pub direction_accuracy: f64,     // EMA of correct directional calls
    pub avg_spread_captured: f64,    // EMA of spread at entry
    pub adverse_selection_rate: f64, // EMA of rounds where skew was wrong
    pub last_direction: String,      // "Up" or "Down"
    pub last_confidence: f64,
    pub last_pnl: f64,
    pub rounds_completed: u64,
}

impl Default for RoundMemory {
    fn default() -> Self {
        Self {
            direction_accuracy: 0.5,
            avg_spread_captured: 0.03,
            adverse_selection_rate: 0.3,
            last_direction: "Up".into(),
            last_confidence: 0.5,
            last_pnl: 0.0,
            rounds_completed: 0,
        }
    }
}

impl RoundMemory {
    const EMA_ALPHA: f64 = 0.1;

    /// Update memory after a round settles
    pub fn update(&mut self, correct_direction: bool, spread_at_entry: f64, pnl: f64) {
        let correct = if correct_direction { 1.0 } else { 0.0 };
        self.direction_accuracy =
            Self::EMA_ALPHA * correct + (1.0 - Self::EMA_ALPHA) * self.direction_accuracy;
        self.avg_spread_captured =
            Self::EMA_ALPHA * spread_at_entry + (1.0 - Self::EMA_ALPHA) * self.avg_spread_captured;
        self.adverse_selection_rate =
            Self::EMA_ALPHA * (1.0 - correct) + (1.0 - Self::EMA_ALPHA) * self.adverse_selection_rate;
        self.last_pnl = pnl;
        self.rounds_completed += 1;
    }
}

/// Calculate skew alpha based on round memory and current signal
pub fn calculate_skew_alpha(memory: &RoundMemory, p_up_signal: f64) -> f64 {
    // Base: slightly biased toward signal direction
    let signal_bias = (p_up_signal - 0.5) * 0.3; // maps [0,1] -> [-0.15, 0.15]

    // Dampen skew if adverse selection is high
    let dampener = 1.0 - memory.adverse_selection_rate;

    let alpha = 0.5 + signal_bias * dampener;
    alpha.clamp(0.35, 0.65) // never extreme
}

/// Calculate budget allocation for each phase based on timeframe
pub fn phase_budget(timeframe: &Timeframe, total_budget: f64) -> (f64, f64, f64) {
    match timeframe.slug_str() {
        "5m" => (total_budget * 0.60, 0.0, total_budget * 0.40),
        "15m" => (total_budget * 0.40, total_budget * 0.20, total_budget * 0.40),
        _ => (total_budget * 0.30, total_budget * 0.30, total_budget * 0.40), // 1h, 1d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round_phase_from_progress() {
        assert_eq!(RoundPhase::from_progress(0.0), RoundPhase::Open);
        assert_eq!(RoundPhase::from_progress(12.0), RoundPhase::Open);
        assert_eq!(RoundPhase::from_progress(24.9), RoundPhase::Open);
        assert_eq!(RoundPhase::from_progress(25.0), RoundPhase::Mid);
        assert_eq!(RoundPhase::from_progress(50.0), RoundPhase::Mid);
        assert_eq!(RoundPhase::from_progress(84.9), RoundPhase::Mid);
        assert_eq!(RoundPhase::from_progress(85.0), RoundPhase::Close);
        assert_eq!(RoundPhase::from_progress(100.0), RoundPhase::Close);
    }

    #[test]
    fn test_skew_alpha_neutral() {
        let memory = RoundMemory::default();
        let alpha = calculate_skew_alpha(&memory, 0.5);
        assert!((alpha - 0.5).abs() < 1e-10, "Neutral signal should give alpha=0.5");
    }

    #[test]
    fn test_skew_alpha_clamped() {
        let memory = RoundMemory {
            adverse_selection_rate: 0.0, // no dampening
            ..Default::default()
        };
        // Extreme up signal
        let alpha = calculate_skew_alpha(&memory, 1.0);
        assert!(alpha <= 0.65, "Alpha should be clamped to 0.65");
        // Extreme down signal
        let alpha = calculate_skew_alpha(&memory, 0.0);
        assert!(alpha >= 0.35, "Alpha should be clamped to 0.35");
    }

    #[test]
    fn test_skew_dampened_by_adverse_selection() {
        let high_adverse = RoundMemory {
            adverse_selection_rate: 0.9,
            ..Default::default()
        };
        let low_adverse = RoundMemory {
            adverse_selection_rate: 0.1,
            ..Default::default()
        };
        let alpha_high = calculate_skew_alpha(&high_adverse, 0.7);
        let alpha_low = calculate_skew_alpha(&low_adverse, 0.7);
        // Higher adverse selection should result in less skew (closer to 0.5)
        assert!(
            (alpha_high - 0.5).abs() < (alpha_low - 0.5).abs(),
            "High adverse selection should dampen skew"
        );
    }

    #[test]
    fn test_phase_budget_5m() {
        let (open, mid, close) = phase_budget(&Timeframe::FiveMin, 100.0);
        assert!((open - 60.0).abs() < 1e-10);
        assert!((mid - 0.0).abs() < 1e-10);
        assert!((close - 40.0).abs() < 1e-10);
    }

    #[test]
    fn test_phase_budget_15m() {
        let (open, mid, close) = phase_budget(&Timeframe::FifteenMin, 100.0);
        assert!((open - 40.0).abs() < 1e-10);
        assert!((mid - 20.0).abs() < 1e-10);
        assert!((close - 40.0).abs() < 1e-10);
    }

    #[test]
    fn test_phase_budget_1h() {
        let (open, mid, close) = phase_budget(&Timeframe::OneHour, 100.0);
        assert!((open - 30.0).abs() < 1e-10);
        assert!((mid - 30.0).abs() < 1e-10);
        assert!((close - 40.0).abs() < 1e-10);
    }

    #[test]
    fn test_round_memory_update() {
        let mut mem = RoundMemory::default();
        mem.update(true, 0.05, 2.0);
        assert!(mem.direction_accuracy > 0.5);
        assert!(mem.rounds_completed == 1);
        assert!((mem.last_pnl - 2.0).abs() < 1e-10);

        mem.update(false, 0.01, -1.0);
        assert!(mem.rounds_completed == 2);
        assert!((mem.last_pnl - (-1.0)).abs() < 1e-10);
    }
}

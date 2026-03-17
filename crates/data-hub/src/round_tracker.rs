//! Round lifecycle tracker: active round management, expiry detection, and resolution.
//!
//! The RoundTracker is the central state machine for Polymarket prediction rounds.
//! It receives round lists from the scanner (every 5s), detects when rounds
//! disappear (expire), resolves them by comparing spot price to reference price,
//! and maintains a bounded history of resolved rounds for accuracy analysis.
//!
//! Thread safety: wrapped in `Arc<RwLock<>>` and accessed from the scanner task
//! (writer), reference recorder task (writer), and API handlers (readers).

use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use polybot_scanner::crypto::{Asset, CryptoRound, Timeframe};

/// A prediction round that has been settled (expired and resolved).
///
/// Created when a round disappears from the scanner's active round list,
/// indicating it has expired on Polymarket. The `resolved_direction` is
/// determined by comparing `close_price` (spot at expiry) to `reference_price`
/// (spot at round start).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedRound {
    /// Polymarket condition_id uniquely identifying this round.
    pub condition_id: String,
    /// Which crypto asset this round tracks (BTC, ETH, SOL, XRP).
    pub asset: Asset,
    /// The duration/timeframe of the round (5m, 15m, 1h).
    pub timeframe: Timeframe,
    /// Spot price at round start (the binary option strike price).
    pub reference_price: f64,
    /// Spot price at round expiry (determines settlement).
    pub close_price: f64,
    /// "Up" if close_price >= reference_price, "Down" otherwise.
    pub resolved_direction: String,
    /// When the round was detected as expired.
    pub resolved_at: DateTime<Utc>,
    /// When the round originally started on Polymarket.
    pub round_start: DateTime<Utc>,
}

/// Maximum number of resolved rounds to retain in memory.
/// Hardcoded to 200 as a balance between historical depth for accuracy tracking
/// and memory usage. Older rounds are evicted FIFO when the limit is exceeded.
/// This value also serves as the default DB load limit on startup.
const MAX_RESOLVED: usize = 200;

/// Tracks the lifecycle of Polymarket crypto prediction rounds.
///
/// Maintains three collections:
/// - `active`: currently live rounds, keyed by condition_id.
/// - `references`: spot prices recorded at round start, keyed by (asset, timeframe).
/// - `resolved`: FIFO deque of settled rounds, bounded to `MAX_RESOLVED`.
///
/// The tracker does not own any async tasks -- it is a passive data structure
/// updated by the scanner and reference recorder background tasks in `main.rs`.
pub struct RoundTracker {
    /// Currently active (unexpired) rounds, keyed by Polymarket condition_id.
    active: HashMap<String, CryptoRound>,
    /// Reference (strike) prices per asset/timeframe pair.
    /// Updated by the reference recorder task every 1 second.
    references: HashMap<(Asset, Timeframe), f64>,
    /// Bounded FIFO deque of resolved rounds, most recent at the back.
    resolved: VecDeque<ResolvedRound>,
}

impl RoundTracker {
    /// Create an empty RoundTracker with no active rounds, references, or history.
    pub fn new() -> Self {
        Self {
            active: HashMap::new(),
            references: HashMap::new(),
            resolved: VecDeque::new(),
        }
    }

    /// Update active rounds from a fresh scan. Detect disappeared rounds and resolve them.
    pub fn update_rounds(&mut self, rounds: &[CryptoRound], prices: &HashMap<Asset, f64>) {
        let new_ids: std::collections::HashSet<String> =
            rounds.iter().map(|r| r.condition_id.clone()).collect();

        // Find rounds that disappeared (expired)
        let expired: Vec<CryptoRound> = self
            .active
            .values()
            .filter(|r| !new_ids.contains(&r.condition_id))
            .cloned()
            .collect();

        for round in expired {
            let ref_price = self
                .references
                .get(&(round.asset, round.timeframe))
                .copied()
                .or(round.reference_price);

            let close_price = prices.get(&round.asset).copied().unwrap_or(0.0);

            if let Some(rp) = ref_price {
                let direction = if close_price >= rp { "Up" } else { "Down" };
                let resolved = ResolvedRound {
                    condition_id: round.condition_id.clone(),
                    asset: round.asset,
                    timeframe: round.timeframe,
                    reference_price: rp,
                    close_price,
                    resolved_direction: direction.to_string(),
                    resolved_at: Utc::now(),
                    round_start: round.round_start,
                };
                tracing::info!(
                    "Round resolved: {:?}/{:?} {} ref={:.2} close={:.2} -> {}",
                    round.asset, round.timeframe, round.condition_id,
                    rp, close_price, direction
                );
                self.resolved.push_back(resolved);
                while self.resolved.len() > MAX_RESOLVED {
                    self.resolved.pop_front();
                }
            } else {
                tracing::warn!(
                    "Round expired without reference price: {:?}/{:?} {}",
                    round.asset, round.timeframe, round.condition_id
                );
            }

            self.active.remove(&round.condition_id);
        }

        // Insert/update active rounds
        for round in rounds {
            self.active
                .insert(round.condition_id.clone(), round.clone());
        }
    }

    /// Record a reference (strike) price for an asset/timeframe pair.
    ///
    /// Called by the reference recorder task every second after checking
    /// timeframe boundaries. Overwrites any previous reference for this pair.
    pub fn record_reference(&mut self, asset: Asset, timeframe: Timeframe, price: f64) {
        self.references.insert((asset, timeframe), price);
    }

    /// Look up the current reference price for an asset/timeframe pair.
    /// Returns `None` if no reference has been recorded yet (e.g. on cold start).
    pub fn get_reference(&self, asset: Asset, timeframe: Timeframe) -> Option<f64> {
        self.references.get(&(asset, timeframe)).copied()
    }

    /// Query resolved rounds with optional asset and timeframe filters.
    ///
    /// Returns up to `limit` most recent resolved rounds (newest first in the
    /// returned Vec). Both `asset` and `tf` are optional filters; passing `None`
    /// for either means "match all".
    ///
    /// # Arguments
    /// * `asset` - Optional asset filter (e.g. `Some(Asset::BTC)`).
    /// * `tf` - Optional timeframe filter (e.g. `Some(Timeframe::FiveMin)`).
    /// * `limit` - Maximum number of results to return.
    pub fn get_resolved(
        &self,
        asset: Option<Asset>,
        tf: Option<Timeframe>,
        limit: usize,
    ) -> Vec<ResolvedRound> {
        self.resolved
            .iter()
            .rev()
            .filter(|r| {
                asset.map_or(true, |a| r.asset == a) && tf.map_or(true, |t| r.timeframe == t)
            })
            .take(limit)
            .cloned()
            .collect()
    }

    /// Bulk load resolved rounds from DB on startup.
    pub fn load_resolved(&mut self, rounds: Vec<ResolvedRound>) {
        for r in rounds {
            self.resolved.push_back(r);
        }
        while self.resolved.len() > MAX_RESOLVED {
            self.resolved.pop_front();
        }
    }

    /// Return a snapshot of all currently active (unexpired) rounds.
    ///
    /// The returned Vec is a clone of the internal HashMap values, so it is
    /// safe to iterate without holding the RwLock.
    pub fn active_rounds(&self) -> Vec<CryptoRound> {
        self.active.values().cloned().collect()
    }
}

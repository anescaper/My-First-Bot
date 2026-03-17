use chrono::{DateTime, Duration, Utc};
use polybot_scanner::crypto::{Asset, Timeframe};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::RwLock;

/// Unique key for a prediction market round.
#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoundKey {
    pub condition_id: String,
}

/// 5-second OHLCV candle built from raw ticks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MicroCandle {
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub trade_count: u32,
    pub open_time: DateTime<Utc>,
    pub close_time: DateTime<Utc>,
}

/// Binance futures market state for one asset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FuturesState {
    pub funding_rate: f64,
    pub funding_updated: DateTime<Utc>,
    pub open_interest: f64,
    pub oi_change_5m: f64,
    pub taker_buy_sell_ratio: f64,
    pub recent_liquidations: VecDeque<LiquidationEvent>,
}

impl Default for FuturesState {
    fn default() -> Self {
        Self {
            funding_rate: 0.0,
            funding_updated: Utc::now(),
            open_interest: 0.0,
            oi_change_5m: 0.0,
            taker_buy_sell_ratio: 1.0,
            recent_liquidations: VecDeque::new(),
        }
    }
}

/// A single liquidation event from Binance futures.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiquidationEvent {
    pub asset: Asset,
    pub side: String,
    pub price: f64,
    pub quantity: f64,
    pub timestamp: DateTime<Utc>,
}

/// Polymarket token price snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenTick {
    pub p_up: f64,
    pub p_down: f64,
    pub timestamp: DateTime<Utc>,
}

/// Side of a Polymarket token.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TokenSide {
    Up,
    Down,
}

/// Polymarket order book snapshot for a round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBookSnapshot {
    pub bids_up: Vec<(f64, f64)>,
    pub asks_up: Vec<(f64, f64)>,
    pub bids_down: Vec<(f64, f64)>,
    pub asks_down: Vec<(f64, f64)>,
    pub updated: DateTime<Utc>,
}

/// Individual fill on Polymarket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolyFill {
    pub side: TokenSide,
    pub price: f64,
    pub size: f64,
    pub timestamp: DateTime<Utc>,
    pub is_buyer_maker: bool,
}

/// Options market state for one asset (from Deribit).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptionsState {
    /// ATM implied volatility (annualized, decimal).
    pub iv_atm: f64,
    /// 25-delta skew: put IV - call IV (positive = fear).
    pub skew: f64,
    /// Put/call volume ratio over recent window.
    pub put_call_ratio: f64,
    /// Deribit volatility index (DVOL) for this asset.
    pub dvol: f64,
    pub updated: DateTime<Utc>,
}

impl Default for OptionsState {
    fn default() -> Self {
        Self {
            iv_atm: 0.0,
            skew: 0.0,
            put_call_ratio: 1.0,
            dvol: 0.0,
            updated: Utc::now(),
        }
    }
}

/// Correlation matrix across assets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorrelationMatrix {
    pub assets: Vec<Asset>,
    pub matrix: Vec<Vec<f64>>,
    pub updated: DateTime<Utc>,
}

/// Shared state that all adapters write into and all consumers read from.
/// Uses std::sync::RwLock (not tokio) for compatibility with sync code.
#[derive(Default)]
pub struct DataState {
    // === Stream 1: Underlying Asset ===
    pub micro_candles: RwLock<HashMap<Asset, VecDeque<MicroCandle>>>,
    pub latest_prices: RwLock<HashMap<Asset, (f64, DateTime<Utc>)>>,
    pub futures_state: RwLock<HashMap<Asset, FuturesState>>,
    pub coinbase_premium: RwLock<HashMap<Asset, f64>>,

    // === Stream 2: Prediction Market ===
    pub token_prices: RwLock<HashMap<RoundKey, VecDeque<TokenTick>>>,
    pub order_books: RwLock<HashMap<RoundKey, OrderBookSnapshot>>,
    /// Secondary index: (Asset, Timeframe) -> OrderBookSnapshot for cross-scanner lookup.
    pub order_books_by_at: RwLock<HashMap<(Asset, Timeframe), OrderBookSnapshot>>,
    pub trade_tapes: RwLock<HashMap<RoundKey, VecDeque<PolyFill>>>,

    // === Stream 3: Options / Derivatives ===
    pub options_state: RwLock<HashMap<Asset, OptionsState>>,

    // === Derived ===
    pub spread_history: RwLock<HashMap<RoundKey, VecDeque<(DateTime<Utc>, f64)>>>,
    pub correlation_matrix: RwLock<Option<CorrelationMatrix>>,
}

/// Hard cap on any single deque to prevent unbounded growth even if timestamps are wrong.
const HARD_CAP: usize = 10_000;

impl DataState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Remove entries older than `max_age` from all time-series data.
    /// Keeps a hard cap as fallback safety.
    pub fn gc(&self, max_age: Duration) {
        let cutoff = Utc::now() - max_age;

        // micro_candles: keyed by Asset, entries have close_time
        if let Ok(mut candles) = self.micro_candles.write() {
            for deque in candles.values_mut() {
                while deque.front().map_or(false, |c| c.close_time < cutoff) {
                    deque.pop_front();
                }
                while deque.len() > HARD_CAP {
                    deque.pop_front();
                }
            }
        }

        // latest_prices: keyed by Asset, value is (price, timestamp) — remove stale entries
        if let Ok(mut prices) = self.latest_prices.write() {
            prices.retain(|_, (_, ts)| *ts >= cutoff);
        }

        // token_prices: keyed by RoundKey, entries have timestamp
        if let Ok(mut ticks) = self.token_prices.write() {
            for deque in ticks.values_mut() {
                while deque.front().map_or(false, |t| t.timestamp < cutoff) {
                    deque.pop_front();
                }
                while deque.len() > HARD_CAP {
                    deque.pop_front();
                }
            }
            // Remove empty deques entirely
            ticks.retain(|_, deque| !deque.is_empty());
        }

        // trade_tapes: keyed by RoundKey, entries have timestamp
        if let Ok(mut tapes) = self.trade_tapes.write() {
            for deque in tapes.values_mut() {
                while deque.front().map_or(false, |f| f.timestamp < cutoff) {
                    deque.pop_front();
                }
                while deque.len() > HARD_CAP {
                    deque.pop_front();
                }
            }
            tapes.retain(|_, deque| !deque.is_empty());
        }

        // order_books: keyed by RoundKey, single snapshot with updated timestamp
        if let Ok(mut books) = self.order_books.write() {
            books.retain(|_, snap| snap.updated >= cutoff);
        }

        // spread_history: keyed by RoundKey, entries are (timestamp, f64)
        if let Ok(mut spreads) = self.spread_history.write() {
            for deque in spreads.values_mut() {
                while deque.front().map_or(false, |(ts, _)| *ts < cutoff) {
                    deque.pop_front();
                }
                while deque.len() > HARD_CAP {
                    deque.pop_front();
                }
            }
            spreads.retain(|_, deque| !deque.is_empty());
        }

        // options_state: keyed by Asset, single snapshot with updated timestamp
        if let Ok(mut opts) = self.options_state.write() {
            opts.retain(|_, o| o.updated >= cutoff);
        }

        // futures_state: keyed by Asset, contains recent_liquidations with timestamps
        if let Ok(mut fs) = self.futures_state.write() {
            for state in fs.values_mut() {
                while state.recent_liquidations.front().map_or(false, |e| e.timestamp < cutoff) {
                    state.recent_liquidations.pop_front();
                }
            }
        }
    }
}

//! Data types for the DataHub's enriched API layer.
//!
//! These structs form the serialization contract between the data-hub HTTP API
//! and the bot pipelines (Rust) or strategy service (Python). Every struct here
//! is `Serialize + Deserialize` so it can travel as JSON over the `/cycle-data`
//! endpoint or be stored in SQLite.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use polybot_scanner::crypto::CryptoRound;
use polybot_scanner::price_feed::Candle;

/// Everything a bot needs per cycle -- the main `/cycle-data` response.
///
/// Designed as a single-request "fat payload" so each bot pipeline can make
/// exactly one HTTP call per cycle and receive all market data it needs.
/// This avoids N+1 request patterns and keeps cycle latency low (<50ms).
///
/// # Fields
/// * `rounds` - Active Polymarket crypto prediction rounds from the scanner.
/// * `prices` - Latest Binance spot prices keyed by asset slug (e.g. "BTC").
/// * `reference_prices` - Snapshot prices at each round's start, keyed "BTC_5m".
/// * `candles` - Aggregated candle histories keyed "BTC_1m", "ETH_5m", etc.
/// * `futures` - Binance futures state (funding, OI, liquidations) per asset.
/// * `token_prices` - Polymarket CLOB token price trajectories per condition_id.
/// * `order_books` - Polymarket CLOB order book snapshots per condition_id.
/// * `intel` - Computed data-driven intelligence (trends, vol, displacement) per asset.
/// * `options` - Deribit DVOL / IV / skew per asset (when enabled).
/// * `resolved_rounds` - Recently settled rounds for accuracy tracking.
/// * `trade_tapes` - Polymarket CLOB fill-level data per condition_id.
/// * `coinbase_premiums` - Cross-exchange premium (Coinbase - Binance) / Binance.
/// * `timestamp` - ISO 8601 timestamp of when this response was assembled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CycleDataResponse {
    pub rounds: Vec<CryptoRound>,
    /// "BTC" -> latest spot price
    pub prices: HashMap<String, f64>,
    /// "BTC_5m" -> reference price at round start
    pub reference_prices: HashMap<String, f64>,
    /// "BTC_1m" -> [candles]
    pub candles: HashMap<String, Vec<Candle>>,
    pub futures: HashMap<String, FuturesSnapshot>,
    pub token_prices: HashMap<String, Vec<TokenTickSnapshot>>,
    pub order_books: HashMap<String, OrderBookSnapshotData>,
    /// "BTC" -> data-driven intel
    pub intel: HashMap<String, DataDrivenIntel>,
    /// "BTC" -> options market state (Deribit DVOL, IV, skew)
    #[serde(default)]
    pub options: HashMap<String, OptionsSnapshot>,
    pub resolved_rounds: Vec<ResolvedRoundData>,
    /// Polymarket CLOB trade tapes (fill-level data per round)
    #[serde(default)]
    pub trade_tapes: HashMap<String, Vec<TradeSnapshot>>,
    /// Cross-exchange premium: (coinbase - binance) / binance per asset
    #[serde(default)]
    pub coinbase_premiums: HashMap<String, f64>,
    /// ISO 8601 timestamp
    pub timestamp: String,
}

/// Snapshot of Binance perpetual futures state for a single asset.
///
/// Captures the key futures market microstructure signals that strategies use:
/// funding rate (crowding indicator), open interest (conviction), taker ratio
/// (aggression), and recent liquidations (cascade/squeeze detection).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FuturesSnapshot {
    /// Current 8-hour funding rate. Positive = longs pay shorts (crowded long).
    /// Extreme values (>0.03%) are contrarian signals per `futures_positioning` strategy.
    pub funding_rate: f64,
    /// Total open interest in USD notional.
    pub open_interest: f64,
    /// Ratio of taker buy volume to taker sell volume over the last period.
    /// >1.0 = net buying pressure, <1.0 = net selling pressure.
    pub taker_buy_sell_ratio: f64,
    /// Percentage change in open interest over the last 5 minutes.
    /// Rising OI + directional taker flow = new position entry (confirms trend).
    /// Falling OI = position closure (mean-reversion signal).
    pub oi_change_5m: f64,
    /// Recent forced liquidation events, used for squeeze detection.
    pub recent_liquidations: Vec<LiquidationSnapshot>,
}

/// A single forced liquidation event from Binance Futures.
///
/// Used by the `futures_positioning` strategy to detect liquidation cascades.
/// When sell liquidations (short squeezes) dominate, it is a bullish signal;
/// when buy liquidations (long squeezes) dominate, it is bearish.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiquidationSnapshot {
    /// "BUY" or "SELL" -- the side that was forcibly closed.
    pub side: String,
    /// Liquidation execution price.
    pub price: f64,
    /// Liquidation size in base asset units.
    pub quantity: f64,
    /// ISO 8601 timestamp of the liquidation event.
    pub timestamp: String,
}

/// A single tick from the Polymarket CLOB token price trajectory.
///
/// Captures the market-implied probability of UP and DOWN outcomes at a point
/// in time. The `token_flow_divergence` strategy uses the velocity and
/// acceleration of p_up over time to detect informed flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenTickSnapshot {
    /// Market-implied probability of the UP outcome (0.0 to 1.0).
    pub p_up: f64,
    /// Market-implied probability of the DOWN outcome (typically 1.0 - p_up).
    pub p_down: f64,
    /// ISO 8601 timestamp of this tick.
    pub timestamp: String,
}

/// Polymarket CLOB order book snapshot for a single prediction market.
///
/// Contains bid/ask levels for both the UP token and DOWN token sides.
/// Each level is a (price, size) tuple. Used by the `clob_microstructure`
/// and `lmsr_liquidity_filter` strategies for order book imbalance and
/// liquidity assessment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBookSnapshotData {
    /// Bid levels for the UP token: Vec<(price, size)>, best bid first.
    pub bids_up: Vec<(f64, f64)>,
    /// Ask levels for the UP token: Vec<(price, size)>, best ask first.
    pub asks_up: Vec<(f64, f64)>,
    /// Bid levels for the DOWN token: Vec<(price, size)>.
    pub bids_down: Vec<(f64, f64)>,
    /// Ask levels for the DOWN token: Vec<(price, size)>.
    pub asks_down: Vec<(f64, f64)>,
    /// ISO 8601 timestamp of when this snapshot was captured.
    pub updated: String,
}

/// API-friendly representation of a settled prediction round.
///
/// This is the serialized form of `round_tracker::ResolvedRound` with string
/// keys and ISO 8601 timestamps (instead of `Asset`/`Timeframe` enums and
/// `DateTime<Utc>`). Used in the `/resolved` endpoint and the `resolved_rounds`
/// field of `CycleDataResponse` so the Python strategy service and frontend
/// can consume it without Rust-specific type knowledge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedRoundData {
    /// Polymarket condition_id uniquely identifying this round.
    pub condition_id: String,
    /// Asset slug in uppercase, e.g. "BTC", "ETH".
    pub asset: String,
    /// Timeframe slug, e.g. "5m", "15m", "1h".
    pub timeframe: String,
    /// Spot price at the start of the round (the strike).
    pub reference_price: f64,
    /// Spot price at round expiry (used to determine settlement).
    pub close_price: f64,
    /// Settlement direction: "Up" if close >= reference, "Down" otherwise.
    pub resolved_direction: String,
    /// ISO 8601 timestamp when the round was resolved (detected as expired).
    pub resolved_at: String,
    /// ISO 8601 timestamp when the round originally started.
    pub round_start: String,
}

/// Computed market intelligence for a single asset, derived from candle data
/// and round tracker state.
///
/// Produced by `intel::compute_intel()` and served in the `/cycle-data` and
/// `/intel/{asset}` endpoints. Provides the Python strategy service with
/// pre-computed technical signals so it does not need to re-derive them.
///
/// Design rationale: computing these in Rust (close to the data source) avoids
/// serializing raw candle arrays to Python for each of the 8 bot pipelines.
/// Each pipeline gets the same intel; only strategy logic differs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataDrivenIntel {
    /// Candle-based trend per timeframe: log-return of close/open, clamped to [-1, +1].
    /// Positive = bullish trend over that timeframe's window.
    pub candle_trend: HashMap<String, f64>,
    /// Realized volatility (sample stddev of per-candle log-returns) per timeframe.
    /// Used as a vol estimate when GARCH or stochastic vol models are not available.
    pub realized_vol: HashMap<String, f64>,
    /// Magnitude-weighted cross-timeframe trend agreement.
    /// -1.0 = all timeframes bearish, +1.0 = all bullish, 0.0 = mixed/flat.
    /// Computed as sum(trends) / sum(|trends|) so stronger trends count more.
    pub trend_agreement: f64,
    /// Log-return displacement from reference price per timeframe.
    /// Computed as ln(current_price / reference_price) for each active round.
    pub displacement: HashMap<String, f64>,
    /// Child round accuracy: fraction of resolved child rounds matching parent trend.
    /// Currently a placeholder (populated by future research phases).
    pub child_accuracy: HashMap<String, f64>,
}

/// Options market state from Deribit for a single asset (BTC or ETH).
///
/// Used by the `options_flow` strategy which substitutes DVOL (forward implied
/// volatility) for realized vol in the displacement z-score calculation.
/// DVOL captures event risk (FOMC, CPI) that backward-looking vol measures miss.
///
/// Note: `skew` and `put_call_ratio` are defined but not yet populated by the
/// Deribit WS adapter -- only `dvol` is live as of 2026-03-15.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptionsSnapshot {
    /// At-the-money implied volatility (annualized, decimal).
    pub iv_atm: f64,
    /// 25-delta put-call skew. Positive = puts more expensive (fear).
    pub skew: f64,
    /// Put/call open interest ratio. >1.0 = bearish hedging demand.
    pub put_call_ratio: f64,
    /// Deribit Volatility Index (annualized %, e.g. 45 = 45%).
    /// This is the primary field used by the `options_flow` strategy.
    pub dvol: f64,
    /// ISO 8601 timestamp of the last update from Deribit.
    pub updated: String,
}

/// A single fill (trade) from the Polymarket CLOB.
///
/// Used by the `clob_microstructure` strategy to compute trade flow imbalance
/// (TFI). When the taker side is buyer-initiated on the Up token, it signals
/// informed bullish flow; when buyer-initiated on the Down token, it signals
/// informed bearish flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeSnapshot {
    /// Which outcome token was traded: "Up" or "Down".
    pub side: String,
    /// Execution price of the fill (0.0 to 1.0 for binary outcome tokens).
    pub price: f64,
    /// Fill size in token units (shares).
    pub size: f64,
    /// True if the buyer was the passive (maker) side.
    /// When false, the buyer was the aggressive taker -- this is the
    /// "informed flow" signal used by TFI computation.
    pub is_buyer_maker: bool,
    /// ISO 8601 timestamp of the fill.
    pub timestamp: String,
}

/// Default produces an empty intel struct with no signals.
/// Used as a fallback when price data is unavailable or the asset is not tracked.
impl Default for DataDrivenIntel {
    fn default() -> Self {
        Self {
            candle_trend: HashMap::new(),
            realized_vol: HashMap::new(),
            trend_agreement: 0.0,
            displacement: HashMap::new(),
            child_accuracy: HashMap::new(),
        }
    }
}

/// Response payload for the `/health` endpoint.
///
/// Provides operational status of the data-hub: whether each data adapter is
/// connected and how recently it received data. Used by monitoring dashboards
/// and the frontend health indicator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    /// Overall status string. Always "ok" if the service is responding.
    pub status: String,
    /// Per-adapter connectivity and freshness status.
    pub adapters: Vec<AdapterStatus>,
    /// Number of currently active (unexpired) prediction rounds.
    pub active_rounds: usize,
    /// Total number of resolved rounds stored in the tracker.
    pub resolved_rounds: usize,
    /// Seconds since the data-hub process started.
    pub uptime_secs: u64,
}

/// Connectivity and data freshness status for a single data adapter.
///
/// Each adapter (Binance spot, Binance futures, Polymarket CLOB, Deribit) has
/// a staleness threshold. If the last data point is older than the threshold,
/// the adapter is marked as disconnected.
///
/// Hardcoded thresholds (chosen based on expected update frequency):
/// - binance_spot: 30s (WebSocket ticks every ~100ms)
/// - binance_futures: 120s (REST polling every 60s)
/// - polymarket_clob: 60s (WebSocket, sparse in quiet markets)
/// - deribit_options: 120s (WebSocket index updates)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdapterStatus {
    /// Adapter name, e.g. "binance_spot", "polymarket_clob".
    pub name: String,
    /// Whether the adapter is considered connected (data within staleness threshold).
    pub connected: bool,
    /// Seconds since the most recent data point was received.
    /// 999 indicates no data has ever been received.
    pub last_data_secs_ago: i64,
}

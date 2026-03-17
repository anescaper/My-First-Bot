//! Shared types for the scanner crate — general market representations.
//!
//! These types represent Polymarket's general (non-crypto) binary markets.
//! For crypto Up/Down rounds, see `crypto::CryptoRound`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A general Polymarket binary market (Yes/No).
///
/// Populated from the Gamma API during market scanning. Represents a single
/// binary prediction market (e.g., "Will X happen by date Y?").
///
/// Token IDs map 1:1 with outcomes: `token_ids[0]` corresponds to `outcomes[0]`, etc.
/// For general markets, outcomes are typically `["Yes", "No"]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Market {
    /// Polymarket condition ID — unique identifier for this market.
    pub condition_id: String,
    /// Human-readable market question (e.g., "Will BTC hit $100K by June?").
    pub question: String,
    /// CLOB token IDs for each outcome. Usually 2 elements for binary markets.
    pub token_ids: Vec<String>,
    /// Outcome labels (e.g., ["Yes", "No"]). Same order as token_ids.
    pub outcomes: Vec<String>,
    /// Current prices for each outcome (0.0 to 1.0). Same order as token_ids.
    pub outcome_prices: Vec<f64>,
    /// Total liquidity in USD available in the market.
    pub liquidity: f64,
    /// 24-hour trading volume in USD.
    pub volume_24h: f64,
    /// When the market resolves. None if no end date is set.
    pub end_date: Option<DateTime<Utc>>,
    /// Bid-ask spread (best_ask - best_bid). Initialized to 0.0 by ClobClient
    /// and must be computed separately from order book data.
    pub spread: f64,
    /// Whether the market is currently active (accepting trades).
    pub active: bool,
    /// Whether this is a "negative risk" market. Polymarket uses this flag for
    /// markets where the complement relationship between outcomes allows
    /// the CLOB to offer better pricing. Affects order signing.
    #[serde(default)]
    pub neg_risk: bool,
    /// Market taker fee rate in basis points (from CLOB API).
    /// Must be passed verbatim in the order's `feeRateBps` field during execution.
    /// Example: 200 = 2% fee.
    #[serde(default)]
    pub fee_rate_bps: u64,
}

/// A point-in-time snapshot of all scanned markets.
///
/// Used for logging and API responses to show the state of the market universe
/// at a given scan cycle. Includes timing metadata for performance monitoring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketSnapshot {
    /// All markets discovered in this scan cycle.
    pub markets: Vec<Market>,
    /// When this snapshot was taken (UTC).
    pub timestamp: DateTime<Utc>,
    /// How long the scan took in milliseconds.
    pub scan_duration_ms: u64,
}

/// A single price level in an order book (one bid or ask).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBookLevel {
    /// Price at this level (0.0 to 1.0 for Polymarket binary tokens).
    pub price: f64,
    /// Total size available at this price level (in token units).
    pub size: f64,
}

/// Full order book for a single token (one side of a binary market).
///
/// Bids are sorted descending by price (best bid first).
/// Asks are sorted ascending by price (best ask first).
/// Mid price and spread are pre-computed for convenience.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBook {
    /// Token ID this order book belongs to.
    pub market_id: String,
    /// Buy orders, sorted by price descending (highest/best bid first).
    pub bids: Vec<OrderBookLevel>,
    /// Sell orders, sorted by price ascending (lowest/best ask first).
    pub asks: Vec<OrderBookLevel>,
    /// Midpoint between best bid and best ask: (best_bid + best_ask) / 2.
    pub mid_price: f64,
    /// Bid-ask spread: best_ask - best_bid.
    pub spread: f64,
    /// When this order book was fetched.
    pub timestamp: DateTime<Utc>,
}

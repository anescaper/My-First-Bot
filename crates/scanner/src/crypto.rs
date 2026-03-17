//! Crypto round scanner — scans Polymarket crypto Up/Down rounds.
//!
//! This module discovers active crypto Up/Down binary rounds on Polymarket
//! (e.g. "Will BTC go up in the next 5 minutes?"). It enumerates all
//! asset × timeframe combinations, constructs candidate URL slugs based on
//! Polymarket's naming conventions, and fetches round data from the Gamma API.
//!
//! Design: Each round is a binary market with two tokens (Up/Down). The scanner
//! normalizes token ordering so index 0 is always "Up" and index 1 is always "Down",
//! regardless of how Polymarket returns them. This invariant is critical for
//! downstream pipeline logic that assumes `token_id_up` / `price_up` semantics.

use anyhow::Result;
use chrono::{DateTime, Datelike, Timelike, Utc};
use serde::{Deserialize, Serialize};

// === Asset ===

/// Supported crypto assets for Up/Down round trading on Polymarket.
///
/// These four assets are the only ones that Polymarket currently offers
/// crypto Up/Down binary rounds for. Each asset maps to a Polymarket slug,
/// a Binance trading pair (for real-time price feeds), and a Pyth oracle
/// feed (for on-chain reference prices).
///
/// Hardcoded because Polymarket's round creation is manual/curated — new assets
/// are added infrequently and require slug format changes anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Asset {
    BTC,
    ETH,
    SOL,
    XRP,
}

impl Asset {
    /// All supported assets. Used for exhaustive scanning across every asset × timeframe pair.
    pub const ALL: [Asset; 4] = [Asset::BTC, Asset::ETH, Asset::SOL, Asset::XRP];

    /// Lowercase slug used in 5m/15m round URLs on Polymarket.
    /// Format: `{slug}-updown-{timeframe}-{unix_timestamp}`
    pub fn slug_str(&self) -> &'static str {
        match self {
            Asset::BTC => "btc",
            Asset::ETH => "eth",
            Asset::SOL => "sol",
            Asset::XRP => "xrp",
        }
    }

    /// Full name used in hourly/daily event slugs on Polymarket.
    /// Format: `{event_name}-up-or-down-{month}-{day}-{hour}{am/pm}-et`
    ///
    /// Note: XRP uses "xrp" (not "ripple") — matching Polymarket's actual slug convention.
    pub fn event_name(&self) -> &'static str {
        match self {
            Asset::BTC => "bitcoin",
            Asset::ETH => "ethereum",
            Asset::SOL => "solana",
            Asset::XRP => "xrp",
        }
    }

    /// Binance spot trading pair symbol, used for real-time price feed via Binance WebSocket.
    /// All pairs are quoted against USDT because that's Binance's most liquid stablecoin pair.
    pub fn binance_symbol(&self) -> &'static str {
        match self {
            Asset::BTC => "BTCUSDT",
            Asset::ETH => "ETHUSDT",
            Asset::SOL => "SOLUSDT",
            Asset::XRP => "XRPUSDT",
        }
    }

    /// Pyth Network oracle price feed ID (hex-encoded).
    /// Used as a secondary/fallback price source and for on-chain reference price verification.
    ///
    /// These are stable feed IDs from Pyth's mainnet deployment. They rarely change.
    /// See: https://pyth.network/developers/price-feed-ids
    pub fn pyth_feed_id(&self) -> &'static str {
        match self {
            Asset::BTC => "0xe62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43",
            Asset::ETH => "0xff61491a931112ddf1bd8147cd1b641375f79f5825126d665480874634fd0ace",
            Asset::SOL => "0xef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d",
            Asset::XRP => "0xec5d399846a9209f3fe5881d70aae9268c94339ff9817e8d18ff19fa05eea1c8",
        }
    }
}

// === Timeframe ===

/// Trading timeframes for crypto Up/Down rounds on Polymarket.
///
/// Each timeframe corresponds to a round duration. Polymarket creates rounds
/// at fixed intervals: 5-minute and 15-minute rounds use unix-timestamp-based
/// slugs, while hourly and daily rounds use human-readable date-based slugs
/// (in US Eastern Time).
///
/// The timeframe also determines which Binance kline interval to use for
/// price data, and which parent/child timeframes exist for cross-timeframe
/// agreement signals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Timeframe {
    FiveMin,
    FifteenMin,
    OneHour,
    OneDay,
}

impl Timeframe {
    /// All available timeframes including OneDay.
    pub const ALL: [Timeframe; 4] = [
        Timeframe::FiveMin,
        Timeframe::FifteenMin,
        Timeframe::OneHour,
        Timeframe::OneDay,
    ];

    /// Default timeframes used when no explicit selection is provided.
    /// Excludes `OneDay` because Q6/Q14 research showed daily rounds have
    /// consistently negative expected value — the 24-hour window is too long
    /// for short-term directional prediction to maintain an edge.
    pub const DEFAULT: [Timeframe; 3] = [
        Timeframe::FiveMin,
        Timeframe::FifteenMin,
        Timeframe::OneHour,
    ];

    /// Parse a slug string like "5m", "15m", "1h", "1d" into a Timeframe.
    /// Returns `None` for unrecognized slugs.
    pub fn from_slug(s: &str) -> Option<Timeframe> {
        match s {
            "5m" => Some(Timeframe::FiveMin),
            "15m" => Some(Timeframe::FifteenMin),
            "1h" => Some(Timeframe::OneHour),
            "1d" => Some(Timeframe::OneDay),
            _ => None,
        }
    }

    /// Parse a comma-separated env var like "5m,15m,1h" into a `Vec<Timeframe>`.
    /// Silently ignores invalid timeframe slugs. Used for `TIMEFRAMES` env var.
    pub fn parse_list(s: &str) -> Vec<Timeframe> {
        s.split(',')
            .filter_map(|part| Timeframe::from_slug(part.trim()))
            .collect()
    }

    /// Round duration in seconds. Used for computing round boundaries from unix timestamps
    /// and for calculating `round_start` when the API doesn't provide it.
    pub fn seconds(&self) -> u64 {
        match self {
            Timeframe::FiveMin => 300,
            Timeframe::FifteenMin => 900,
            Timeframe::OneHour => 3600,
            Timeframe::OneDay => 86400,
        }
    }

    /// Short slug string for display and serialization ("5m", "15m", "1h", "1d").
    pub fn slug_str(&self) -> &'static str {
        match self {
            Timeframe::FiveMin => "5m",
            Timeframe::FifteenMin => "15m",
            Timeframe::OneHour => "1h",
            Timeframe::OneDay => "1d",
        }
    }

    /// Binance kline (candlestick) interval for price data retrieval.
    ///
    /// Hardcoded mapping because we need enough granularity within each round:
    /// - 5m/15m rounds use 1m klines (5-15 data points per round)
    /// - 1h rounds use 5m klines (12 data points per round)
    /// - 1d rounds use 1h klines (24 data points per round)
    ///
    /// Using the round's own timeframe as the kline interval would give only 1 candle,
    /// which is insufficient for volatility/trend analysis.
    pub fn binance_kline_interval(&self) -> &'static str {
        match self {
            Timeframe::FiveMin => "1m",
            Timeframe::FifteenMin => "1m",
            Timeframe::OneHour => "5m",
            Timeframe::OneDay => "1h",
        }
    }

    /// Parent timeframes (longer periods) for cross-timeframe agreement signals.
    /// Used to check if longer-term trends confirm the shorter-term signal direction.
    /// 5m -> [15m, 1h], 15m -> [1h], 1h -> [], 1d -> []
    pub fn parents(&self) -> Vec<Timeframe> {
        match self {
            Timeframe::FiveMin => vec![Timeframe::FifteenMin, Timeframe::OneHour],
            Timeframe::FifteenMin => vec![Timeframe::OneHour],
            Timeframe::OneHour | Timeframe::OneDay => vec![],
        }
    }

    /// Child timeframes (shorter periods) for cross-timeframe agreement signals.
    /// Used to check if shorter-term momentum confirms the longer-term signal.
    /// 1h -> [15m, 5m], 15m -> [5m], 5m -> [], 1d -> [1h]
    pub fn children(&self) -> Vec<Timeframe> {
        match self {
            Timeframe::OneHour => vec![Timeframe::FifteenMin, Timeframe::FiveMin],
            Timeframe::FifteenMin => vec![Timeframe::FiveMin],
            Timeframe::FiveMin => vec![],
            Timeframe::OneDay => vec![Timeframe::OneHour],
        }
    }

    /// Alias for `slug_str()`. Provided for ergonomic serialization.
    pub fn slug(&self) -> &'static str {
        self.slug_str()
    }
}

// === CryptoRound ===

/// A single crypto Up/Down binary round on Polymarket.
///
/// Represents one active round (e.g. "Will BTC go up in the next 5 minutes?").
/// The scanner populates market-level fields (condition_id, token_ids, prices,
/// liquidity, volume). Enrichment fields (our_p_up, edge) are filled later by
/// the pipeline after the strategy service computes a probability estimate.
///
/// Invariant: `token_id_up` and `price_up` always correspond to the "Up" outcome,
/// and `token_id_down` / `price_down` to "Down". This is enforced by the
/// `reorder_by_outcomes` function during scanning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CryptoRound {
    /// Polymarket condition ID — unique identifier for this binary market.
    pub condition_id: String,
    /// Which crypto asset this round is for (BTC, ETH, SOL, XRP).
    pub asset: Asset,
    /// Round duration (5m, 15m, 1h, 1d).
    pub timeframe: Timeframe,
    /// When the round opened (UTC). Derived from API or computed from timeframe.
    pub round_start: DateTime<Utc>,
    /// When the round resolves (UTC). Rounds past this time are filtered out.
    pub round_end: DateTime<Utc>,
    /// CLOB token ID for the "Up" outcome. Used for order placement.
    pub token_id_up: String,
    /// CLOB token ID for the "Down" outcome. Used for order placement.
    pub token_id_down: String,
    /// Current market price for "Up" token (0.0 to 1.0, represents implied probability).
    pub price_up: f64,
    /// Current market price for "Down" token (0.0 to 1.0, represents implied probability).
    pub price_down: f64,
    /// Total liquidity in the market (USD). From Gamma API.
    pub liquidity: f64,
    /// Trading volume (USD). From Gamma API.
    pub volume: f64,
    /// Reference price of the underlying asset at round start (e.g. BTC price at 10:00).
    /// Populated by PriceFeedManager after scanning.
    pub reference_price: Option<f64>,
    /// Current price of the underlying asset. Populated by PriceFeedManager.
    pub current_price: Option<f64>,
    /// Polymarket URL slug that was used to discover this round.
    pub slug: String,
    /// Our model's estimated probability that the asset goes Up.
    /// Filled by the pipeline after strategy service evaluation — not set by the scanner.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub our_p_up: Option<f64>,
    /// Edge = our_p_up - price_up (for Up direction), representing the perceived mispricing.
    /// Filled by the pipeline after strategy evaluation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edge: Option<f64>,
    /// Whether we currently have an open position on this round.
    /// Set by position tracking logic in the pipeline.
    #[serde(default)]
    pub has_position: bool,
    /// Taker fee rate in basis points from the Polymarket CLOB API.
    /// Must be included in order placement for fee calculations.
    #[serde(default)]
    pub fee_rate_bps: u64,
}

impl CryptoRound {
    /// Seconds until this round resolves. Negative if already expired.
    /// Used to prioritize rounds by urgency and filter out stale entries.
    pub fn seconds_remaining(&self) -> i64 {
        (self.round_end - Utc::now()).num_seconds()
    }

    /// How far through the round we are, as a fraction from 0.0 (just started)
    /// to 1.0 (finished/expired). Clamped to [0, 1].
    /// Returns 1.0 if total duration is zero or negative (degenerate case).
    pub fn progress_pct(&self) -> f64 {
        let total = (self.round_end - self.round_start).num_seconds() as f64;
        if total <= 0.0 { return 1.0; }
        let elapsed = (Utc::now() - self.round_start).num_seconds() as f64;
        (elapsed / total).clamp(0.0, 1.0)
    }
}

// === CryptoScanner ===

/// Base URL for Polymarket's Gamma API, which provides market/event metadata.
/// This is Polymarket's public read-only API for market discovery (not the CLOB trading API).
const GAMMA_API: &str = "https://gamma-api.polymarket.com";

/// Discovers active crypto Up/Down rounds on Polymarket.
///
/// The scanner works by constructing candidate URL slugs for each asset × timeframe
/// pair and querying the Gamma API to see if those rounds exist and are active.
/// This approach is necessary because Polymarket doesn't offer a filtered "all crypto
/// rounds" endpoint — we must guess the slug format.
///
/// Two API endpoints are used depending on timeframe:
/// - Markets API (`/markets?slug=`) for 5m/15m rounds (flat market objects)
/// - Events API (`/events?slug=`) for 1h/1d rounds (market nested inside event)
///
/// This split exists because Polymarket models hourly/daily rounds as "events"
/// containing one market, while short-term rounds are standalone markets.
pub struct CryptoScanner {
    /// Shared HTTP client for all API requests. Cloned (cheaply, via Arc) for parallel scans.
    http: reqwest::Client,
    /// Which timeframes to scan. Defaults to `Timeframe::DEFAULT` (5m, 15m, 1h).
    timeframes: Vec<Timeframe>,
}

/// Convert month number (1-12) to lowercase English name.
/// Used to construct Polymarket hourly/daily slug strings which embed the month name.
fn month_name(month: u32) -> &'static str {
    match month {
        1 => "january", 2 => "february", 3 => "march", 4 => "april",
        5 => "may", 6 => "june", 7 => "july", 8 => "august",
        9 => "september", 10 => "october", 11 => "november", 12 => "december",
        _ => "unknown",
    }
}

/// Format a 24-hour clock value as a 12-hour string like "2pm", "10am", "12pm".
/// Used in hourly round slugs which follow Polymarket's `{hour}{am/pm}` format.
fn format_hour_et(hour24: u32) -> String {
    let ampm = if hour24 < 12 { "am" } else { "pm" };
    let h12 = match hour24 % 12 {
        0 => 12,
        h => h,
    };
    format!("{}{}", h12, ampm)
}

/// Normalize token_ids and prices so index 0 is always "Up" and index 1 is always "Down".
///
/// Polymarket's API does not guarantee a consistent ordering of outcomes.
/// Some markets return `["Up", "Down"]` and others return `["Down", "Up"]`.
/// This function inspects the `outcomes` array and swaps both `token_ids` and
/// `prices` if needed to enforce the Up-first invariant.
///
/// If outcomes are missing or index 0 is already "Up", returns as-is.
/// If fewer than 2 token_ids/prices exist, returns the input unchanged (degenerate case).
fn reorder_by_outcomes(outcomes: &[String], token_ids: &[String], prices: &[String]) -> (Vec<String>, Vec<String>) {
    if token_ids.len() < 2 || prices.len() < 2 {
        return (token_ids.to_vec(), prices.to_vec());
    }

    // Check if first outcome is "Down" (case-insensitive) — need to swap
    if outcomes.first().map(|s| s.eq_ignore_ascii_case("down")).unwrap_or(false) {
        tracing::info!("Outcomes order is [Down, Up] — swapping token_ids and prices to normalize");
        return (
            vec![token_ids[1].clone(), token_ids[0].clone()],
            vec![prices[1].clone(), prices[0].clone()],
        );
    }

    // Default: outcomes[0]="Up" or outcomes empty — trust original order
    (token_ids.to_vec(), prices.to_vec())
}

impl CryptoScanner {
    /// Create a new scanner using the default timeframes (5m, 15m, 1h — no 1d).
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            timeframes: Timeframe::DEFAULT.to_vec(),
        }
    }

    /// Create a scanner targeting specific timeframes.
    /// Falls back to `Timeframe::DEFAULT` if the provided list is empty.
    pub fn with_timeframes(timeframes: Vec<Timeframe>) -> Self {
        Self {
            http: reqwest::Client::new(),
            timeframes: if timeframes.is_empty() {
                Timeframe::DEFAULT.to_vec()
            } else {
                timeframes
            },
        }
    }

    /// Generate candidate URL slugs for a given asset/timeframe.
    ///
    /// Returns multiple candidates because we can't know the exact round boundary
    /// with certainty (clock skew, API lag, timezone edge cases). We try the current
    /// round, the next round, and the previous round, then let the API tell us which
    /// one is active.
    ///
    /// Slug formats differ by timeframe because Polymarket uses two conventions:
    /// - Short rounds (5m/15m): `{asset}-updown-{tf}-{unix_timestamp}`
    /// - Long rounds (1h/1d): `{event_name}-up-or-down-{month}-{day}-{hour}{am/pm}-et`
    ///
    /// The EDT offset is hardcoded to UTC-4 because Polymarket's slug generation
    /// uses US Eastern Daylight Time. During EST (Nov-Mar), this can cause a 1-hour
    /// mismatch, which is covered by trying current, next, and previous hours.
    fn candidate_slugs(asset: Asset, timeframe: Timeframe) -> Vec<String> {
        match timeframe {
            Timeframe::FiveMin | Timeframe::FifteenMin => {
                // Slug format: {asset}-updown-{tf}-{unix_timestamp}
                // The timestamp is the round's start time, aligned to the interval boundary.
                let now_ts = Utc::now().timestamp() as u64;
                let interval = timeframe.seconds();
                let round_start_ts = now_ts - (now_ts % interval);
                vec![
                    format!("{}-updown-{}-{}", asset.slug_str(), timeframe.slug_str(), round_start_ts),
                    format!("{}-updown-{}-{}", asset.slug_str(), timeframe.slug_str(), round_start_ts + interval),
                    format!("{}-updown-{}-{}", asset.slug_str(), timeframe.slug_str(), round_start_ts.saturating_sub(interval)),
                ]
            }
            Timeframe::OneHour => {
                // Slug format: {asset_name}-up-or-down-{month}-{day}-{hour}{am/pm}-et
                // Polymarket uses US Eastern Time (UTC-4 EDT / UTC-5 EST)
                let now_utc = Utc::now();
                // Hardcoded EDT offset (UTC-4). See function doc for EST handling.
                let et_offset = chrono::FixedOffset::west_opt(4 * 3600).unwrap();
                let now_et = now_utc.with_timezone(&et_offset);
                let name = asset.event_name();
                let month = month_name(now_et.month());
                let day = now_et.day();
                let hour = now_et.hour();
                // Try current hour, next hour, and previous hour to handle boundary cases
                let prev_hour = if hour == 0 { 23 } else { hour - 1 };
                vec![
                    format!("{}-up-or-down-{}-{}-{}-et", name, month, day, format_hour_et(hour)),
                    format!("{}-up-or-down-{}-{}-{}-et", name, month, day, format_hour_et((hour + 1) % 24)),
                    format!("{}-up-or-down-{}-{}-{}-et", name, month, day, format_hour_et(prev_hour)),
                ]
            }
            Timeframe::OneDay => {
                // Slug format: {asset_name}-up-or-down-on-{month}-{day}
                let now_utc = Utc::now();
                let et_offset = chrono::FixedOffset::west_opt(4 * 3600).unwrap();
                let now_et = now_utc.with_timezone(&et_offset);
                let name = asset.event_name();
                let month = month_name(now_et.month());
                let day = now_et.day();
                // Try today and tomorrow to handle the daily round spanning midnight ET
                let tomorrow = now_et + chrono::Duration::days(1);
                vec![
                    format!("{}-up-or-down-on-{}-{}", name, month, day),
                    format!("{}-up-or-down-on-{}-{}", name, month_name(tomorrow.month()), tomorrow.day()),
                ]
            }
        }
    }

    /// Scan for an active round for a specific asset/timeframe combination.
    ///
    /// Tries each candidate slug in order, returning the first active round found.
    /// Returns `Ok(None)` if no active round exists (normal — between rounds, or
    /// Polymarket hasn't created the round yet).
    ///
    /// The function also fetches the taker fee rate from the CLOB API, which is
    /// required for order placement and fee calculations downstream.
    pub async fn scan_round(&self, asset: Asset, timeframe: Timeframe) -> Result<Option<CryptoRound>> {
        let now = Utc::now();
        let slugs = Self::candidate_slugs(asset, timeframe);
        let use_events_api = matches!(timeframe, Timeframe::OneHour | Timeframe::OneDay);

        for slug in &slugs {
            let (condition_id, token_ids, prices_str, end_date, start_date, liquidity_val, volume_val) =
                if use_events_api {
                    match self.fetch_from_events_api(slug).await {
                        Some(data) => data,
                        None => continue,
                    }
                } else {
                    match self.fetch_from_markets_api(slug).await {
                        Some(data) => data,
                        None => continue,
                    }
                };

            if token_ids.len() < 2 { continue; }

            let price_up = prices_str.first().and_then(|p| p.parse::<f64>().ok()).unwrap_or(0.5);
            let price_down = prices_str.get(1).and_then(|p| p.parse::<f64>().ok()).unwrap_or(0.5);

            // Only include rounds that haven't ended yet
            let round_end = match end_date {
                Some(end) if end > now => end,
                _ => continue,
            };

            let round_start = start_date.unwrap_or(round_end - chrono::Duration::seconds(timeframe.seconds() as i64));

            return Ok(Some(CryptoRound {
                condition_id,
                asset,
                timeframe,
                round_start,
                round_end,
                token_id_up: token_ids[0].clone(),
                token_id_down: token_ids[1].clone(),
                price_up,
                price_down,
                liquidity: liquidity_val,
                volume: volume_val,
                reference_price: None,
                current_price: None,
                slug: slug.to_string(),
                our_p_up: None,
                edge: None,
                has_position: false,
                fee_rate_bps: self.fetch_fee_rate(&token_ids[0]).await,
            }));
        }

        Ok(None)
    }

    /// Fetch market data from the Gamma markets API (used for 5m and 15m rounds).
    ///
    /// Short-term rounds are modeled as standalone markets on Polymarket.
    /// Returns a tuple of (condition_id, token_ids, prices, end_date, start_date, liquidity, volume).
    /// Returns `None` if the market doesn't exist, is inactive, or is already closed.
    async fn fetch_from_markets_api(&self, slug: &str) -> Option<(String, Vec<String>, Vec<String>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, f64, f64)> {
        let url = format!("{}/markets?slug={}", GAMMA_API, slug);
        let resp: Vec<serde_json::Value> = self.http.get(&url).send().await.ok()?.json().await.ok()?;
        if resp.is_empty() { return None; }
        let m = &resp[0];

        let active = m.get("active").and_then(|v| v.as_bool()).unwrap_or(false);
        let closed = m.get("closed").and_then(|v| v.as_bool()).unwrap_or(true);
        if !active || closed { return None; }

        let condition_id = m.get("conditionId").and_then(|v| v.as_str())?.to_string();
        let raw_token_ids: Vec<String> = m.get("clobTokenIds")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        let raw_prices: Vec<String> = m.get("outcomePrices")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();

        // Parse outcomes array to verify Up/Down index mapping
        let outcomes: Vec<String> = m.get("outcomes")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str(s).ok())
            .or_else(|| m.get("outcomes").and_then(|v| v.as_array()).map(|arr| {
                arr.iter().filter_map(|x| x.as_str().map(String::from)).collect()
            }))
            .unwrap_or_default();
        let (token_ids, prices) = reorder_by_outcomes(&outcomes, &raw_token_ids, &raw_prices);

        let end_date = m.get("endDate").and_then(|v| v.as_str()).and_then(|s| s.parse().ok());
        let start_date = m.get("startDate").and_then(|v| v.as_str()).and_then(|s| s.parse().ok());
        let liquidity = m.get("liquidity").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let volume = m.get("volume").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);

        Some((condition_id, token_ids, prices, end_date, start_date, liquidity, volume))
    }

    /// Fetch market data from the Gamma events API (used for 1h and 1d rounds).
    ///
    /// Hourly and daily rounds are modeled as "events" on Polymarket, with the
    /// actual market nested inside `event.markets[0]`. The event-level `endDate`
    /// is used as the round resolution time, while market-level fields provide
    /// condition_id, token_ids, and prices.
    ///
    /// Returns the same tuple format as `fetch_from_markets_api` for consistency.
    /// Returns `None` if the event doesn't exist, is inactive, or is already closed.
    async fn fetch_from_events_api(&self, slug: &str) -> Option<(String, Vec<String>, Vec<String>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, f64, f64)> {
        let url = format!("{}/events?slug={}", GAMMA_API, slug);
        let resp: Vec<serde_json::Value> = self.http.get(&url).send().await.ok()?.json().await.ok()?;
        if resp.is_empty() { return None; }
        let event = &resp[0];

        let active = event.get("active").and_then(|v| v.as_bool()).unwrap_or(false);
        let closed = event.get("closed").and_then(|v| v.as_bool()).unwrap_or(true);
        if !active || closed { return None; }

        // Market data is nested inside event.markets[0]
        let markets = event.get("markets").and_then(|v| v.as_array())?;
        if markets.is_empty() { return None; }
        let m = &markets[0];

        let m_active = m.get("active").and_then(|v| v.as_bool()).unwrap_or(false);
        let m_closed = m.get("closed").and_then(|v| v.as_bool()).unwrap_or(true);
        if !m_active || m_closed { return None; }

        let condition_id = m.get("conditionId").and_then(|v| v.as_str())?.to_string();
        let raw_token_ids: Vec<String> = m.get("clobTokenIds")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        let raw_prices: Vec<String> = m.get("outcomePrices")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();

        // Parse outcomes array to verify Up/Down index mapping
        let outcomes: Vec<String> = m.get("outcomes")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str(s).ok())
            .or_else(|| m.get("outcomes").and_then(|v| v.as_array()).map(|arr| {
                arr.iter().filter_map(|x| x.as_str().map(String::from)).collect()
            }))
            .unwrap_or_default();
        let (token_ids, prices) = reorder_by_outcomes(&outcomes, &raw_token_ids, &raw_prices);

        // Use event-level endDate as the round end
        let end_date = event.get("endDate").and_then(|v| v.as_str()).and_then(|s| s.parse().ok());
        let start_date = event.get("startTime").and_then(|v| v.as_str()).and_then(|s| s.parse().ok());
        let liquidity = m.get("liquidity").and_then(|v| v.as_f64())
            .or_else(|| m.get("liquidity").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()))
            .unwrap_or(0.0);
        let volume = m.get("volume").and_then(|v| v.as_f64())
            .or_else(|| m.get("volume").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()))
            .unwrap_or(0.0);

        Some((condition_id, token_ids, prices, end_date, start_date, liquidity, volume))
    }

    /// Scan all asset/timeframe combinations in parallel, returning all active rounds.
    ///
    /// Spawns one tokio task per asset/timeframe pair (up to 4 assets x 3 timeframes = 12 tasks).
    /// Each task runs `scan_round` independently. Failed scans are logged and skipped
    /// (best-effort — one failing API call shouldn't block the entire scan cycle).
    ///
    /// Returns a flat Vec of all discovered active rounds.
    pub async fn scan_all(&self) -> Vec<CryptoRound> {
        let mut handles = Vec::new();

        for asset in Asset::ALL {
            for &timeframe in &self.timeframes {
                let scanner = Self {
                    http: self.http.clone(),
                    timeframes: self.timeframes.clone(),
                };
                handles.push(tokio::spawn(async move {
                    scanner.scan_round(asset, timeframe).await
                }));
            }
        }

        let mut rounds = Vec::new();
        for handle in handles {
            match handle.await {
                Ok(Ok(Some(round))) => rounds.push(round),
                Ok(Ok(None)) => {}
                Ok(Err(e)) => tracing::warn!("Scan error: {}", e),
                Err(e) => tracing::warn!("Join error: {}", e),
            }
        }

        rounds
    }

    /// Fetch the taker fee rate for a token from the Polymarket CLOB API.
    ///
    /// Returns the fee in basis points (e.g. 200 = 2%). Returns 0 on any error
    /// (network failure, parse error) — conservative fallback since 0 means
    /// we undercount fees rather than reject valid trades.
    async fn fetch_fee_rate(&self, token_id: &str) -> u64 {
        let url = format!("https://clob.polymarket.com/fee-rate?token_id={}", token_id);
        match self.http.get(&url).send().await {
            Ok(resp) => {
                match resp.json::<serde_json::Value>().await {
                    Ok(v) => v.get("base_fee").and_then(|f| f.as_u64()).unwrap_or(0),
                    Err(_) => 0,
                }
            }
            Err(_) => 0,
        }
    }
}

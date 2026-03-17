//! HTTP client for the shared data-hub service.
//! Replaces DataHub (WS/REST connections) and PriceFeedManager (kline fetches)
//! for bot containers.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
use polybot_scanner::crypto::{Asset, CryptoRound, Timeframe};
use polybot_scanner::price_feed::Candle;
use serde::{Deserialize, Serialize};

use crate::state::{DataState, FuturesState, OptionsState, OrderBookSnapshot, RoundKey, TokenTick};

// === Snapshot types matching the data-hub API response ===

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CycleData {
    pub rounds: Vec<CryptoRound>,
    pub prices: HashMap<String, f64>,
    pub reference_prices: HashMap<String, f64>,
    pub candles: HashMap<String, Vec<Candle>>,
    pub futures: HashMap<String, FuturesSnapshot>,
    pub token_prices: HashMap<String, Vec<TokenTickSnapshot>>,
    pub order_books: HashMap<String, OrderBookSnapshotData>,
    pub intel: HashMap<String, IntelSnapshot>,
    #[serde(default)]
    pub options: HashMap<String, OptionsSnapshot>,
    pub resolved_rounds: Vec<ResolvedRoundSnapshot>,
    #[serde(default)]
    pub trade_tapes: HashMap<String, Vec<TradeSnapshot>>,
    #[serde(default)]
    pub coinbase_premiums: HashMap<String, f64>,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FuturesSnapshot {
    pub funding_rate: f64,
    pub open_interest: f64,
    pub taker_buy_sell_ratio: f64,
    #[serde(default)]
    pub oi_change_5m: f64,
    #[serde(default)]
    pub recent_liquidations: Vec<LiquidationSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiquidationSnapshot {
    pub side: String,
    pub price: f64,
    pub quantity: f64,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenTickSnapshot {
    pub p_up: f64,
    pub p_down: f64,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBookSnapshotData {
    pub bids_up: Vec<(f64, f64)>,
    pub asks_up: Vec<(f64, f64)>,
    pub bids_down: Vec<(f64, f64)>,
    pub asks_down: Vec<(f64, f64)>,
    pub updated: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptionsSnapshot {
    pub iv_atm: f64,
    pub skew: f64,
    pub put_call_ratio: f64,
    pub dvol: f64,
    pub updated: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntelSnapshot {
    pub candle_trend: HashMap<String, f64>,
    pub realized_vol: HashMap<String, f64>,
    pub trend_agreement: f64,
    pub displacement: HashMap<String, f64>,
    pub child_accuracy: HashMap<String, f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeSnapshot {
    pub side: String,
    pub price: f64,
    pub size: f64,
    pub is_buyer_maker: bool,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedRoundSnapshot {
    pub condition_id: String,
    pub asset: String,
    pub timeframe: String,
    pub reference_price: f64,
    pub close_price: f64,
    pub resolved_direction: String,
    pub resolved_at: String,
    #[serde(default)]
    pub round_start: String,
}

// === DataClient ===

pub struct DataClient {
    http: reqwest::Client,
    data_hub_url: String,
    state: Arc<DataState>,
    last_cycle: RwLock<Option<CycleData>>,
}

impl DataClient {
    pub fn new(data_hub_url: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            data_hub_url: data_hub_url.trim_end_matches('/').to_string(),
            state: Arc::new(DataState::new()),
            last_cycle: RwLock::new(None),
        }
    }

    /// Get shared DataState (populated by hydrate_state on each fetch_cycle).
    pub fn state(&self) -> Arc<DataState> {
        Arc::clone(&self.state)
    }

    /// Fetch all data for one pipeline cycle from data-hub.
    /// Returns rounds, prices, candles, intel, etc.
    /// Also hydrates the local DataState for code that reads it directly.
    pub async fn fetch_cycle(&self) -> anyhow::Result<CycleData> {
        let url = format!("{}/cycle-data", self.data_hub_url);
        let resp: CycleData = self
            .http
            .get(&url)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await?
            .json()
            .await?;
        self.hydrate_state(&resp);
        *self.last_cycle.write().unwrap() = Some(resp.clone());
        Ok(resp)
    }

    /// Get candles for a specific asset + binance kline interval (e.g., "1m", "5m").
    pub fn get_candles(&self, asset: Asset, interval: &str) -> Vec<Candle> {
        let key = format!("{:?}_{}", asset, interval);
        self.last_cycle
            .read()
            .unwrap()
            .as_ref()
            .and_then(|c| c.candles.get(&key))
            .cloned()
            .unwrap_or_default()
    }

    /// Get candles for a specific asset + timeframe (maps timeframe to interval).
    pub fn get_candles_for_timeframe(&self, asset: Asset, tf: Timeframe) -> Vec<Candle> {
        self.get_candles(asset, tf.binance_kline_interval())
    }

    /// Get reference price for an asset/timeframe.
    pub fn get_reference(&self, asset: Asset, tf: Timeframe) -> Option<f64> {
        let key = format!("{:?}_{}", asset, tf.slug());
        self.last_cycle
            .read()
            .unwrap()
            .as_ref()
            .and_then(|c| c.reference_prices.get(&key))
            .copied()
    }

    /// Get current price for an asset.
    pub fn get_price(&self, asset: Asset) -> Option<f64> {
        let key = format!("{:?}", asset);
        self.last_cycle
            .read()
            .unwrap()
            .as_ref()
            .and_then(|c| c.prices.get(&key))
            .copied()
    }

    /// Get all current prices (keyed by Asset).
    pub fn get_all_prices(&self) -> HashMap<Asset, f64> {
        let guard = self.last_cycle.read().unwrap();
        let cycle = match guard.as_ref() {
            Some(c) => c,
            None => return HashMap::new(),
        };
        let mut out = HashMap::new();
        for (key, &price) in &cycle.prices {
            if let Some(asset) = parse_asset(key) {
                out.insert(asset, price);
            }
        }
        out
    }

    /// Get all reference prices (keyed by Asset).
    /// When multiple timeframes exist for one asset, use the shortest (most recent) timeframe.
    pub fn get_all_reference_prices(&self) -> HashMap<Asset, f64> {
        let guard = self.last_cycle.read().unwrap();
        let cycle = match guard.as_ref() {
            Some(c) => c,
            None => return HashMap::new(),
        };
        // Collect all (asset, timeframe_slug, price) tuples
        let mut by_asset: HashMap<Asset, Vec<(&str, f64)>> = HashMap::new();
        for (key, &price) in &cycle.reference_prices {
            let parts: Vec<&str> = key.splitn(2, '_').collect();
            if parts.len() == 2 {
                if let Some(asset) = parse_asset(parts[0]) {
                    by_asset.entry(asset).or_default().push((parts[1], price));
                }
            }
        }
        // Pick the shortest timeframe per asset (5m < 15m < 1h < 1d)
        let tf_rank = |slug: &str| -> u8 {
            match slug { "5m" => 0, "15m" => 1, "1h" => 2, "1d" => 3, _ => 4 }
        };
        by_asset.into_iter().map(|(asset, mut entries)| {
            entries.sort_by_key(|(slug, _)| tf_rank(slug));
            (asset, entries[0].1)
        }).collect()
    }

    /// Get data-driven intel for an asset.
    pub fn get_intel(&self, asset: Asset) -> Option<IntelSnapshot> {
        let key = format!("{:?}", asset);
        self.last_cycle
            .read()
            .unwrap()
            .as_ref()
            .and_then(|c| c.intel.get(&key))
            .cloned()
    }

    /// Get the last fetched CycleData, if any.
    pub fn last_cycle(&self) -> Option<CycleData> {
        self.last_cycle.read().unwrap().clone()
    }

    /// Hydrate the local DataState from cycle data so existing pipeline code
    /// that reads data_state directly still works.
    fn hydrate_state(&self, data: &CycleData) {
        let now = Utc::now();

        // latest_prices: HashMap<Asset, (f64, DateTime<Utc>)>
        if let Ok(mut prices) = self.state.latest_prices.write() {
            prices.clear();
            for (key, &price) in &data.prices {
                if let Some(asset) = parse_asset(key) {
                    prices.insert(asset, (price, now));
                }
            }
        }

        // futures_state: HashMap<Asset, FuturesState>
        if let Ok(mut futures) = self.state.futures_state.write() {
            futures.clear();
            for (key, snap) in &data.futures {
                if let Some(asset) = parse_asset(key) {
                    let liquidations: VecDeque<crate::state::LiquidationEvent> = snap
                        .recent_liquidations
                        .iter()
                        .filter_map(|l| {
                            let ts = l.timestamp.parse::<DateTime<Utc>>().ok()?;
                            Some(crate::state::LiquidationEvent {
                                asset,
                                side: l.side.clone(),
                                price: l.price,
                                quantity: l.quantity,
                                timestamp: ts,
                            })
                        })
                        .collect();
                    futures.insert(
                        asset,
                        FuturesState {
                            funding_rate: snap.funding_rate,
                            funding_updated: now,
                            open_interest: snap.open_interest,
                            oi_change_5m: snap.oi_change_5m,
                            taker_buy_sell_ratio: snap.taker_buy_sell_ratio,
                            recent_liquidations: liquidations,
                        },
                    );
                }
            }
        }

        // token_prices: HashMap<RoundKey, VecDeque<TokenTick>>
        if let Ok(mut token_prices) = self.state.token_prices.write() {
            token_prices.clear();
            for (condition_id, ticks) in &data.token_prices {
                let round_key = RoundKey {
                    condition_id: condition_id.clone(),
                };
                let deque: VecDeque<TokenTick> = ticks
                    .iter()
                    .filter_map(|snap| {
                        let ts = DateTime::parse_from_rfc3339(&snap.timestamp)
                            .ok()?
                            .with_timezone(&Utc);
                        Some(TokenTick {
                            p_up: snap.p_up,
                            p_down: snap.p_down,
                            timestamp: ts,
                        })
                    })
                    .collect();
                if !deque.is_empty() {
                    token_prices.insert(round_key, deque);
                }
            }
        }

        // options_state: HashMap<Asset, OptionsState>
        if let Ok(mut opts) = self.state.options_state.write() {
            opts.clear();
            for (key, snap) in &data.options {
                if let Some(asset) = parse_asset(key) {
                    let updated = DateTime::parse_from_rfc3339(&snap.updated)
                        .map(|dt| dt.with_timezone(&Utc))
                        .unwrap_or(now);
                    opts.insert(
                        asset,
                        OptionsState {
                            iv_atm: snap.iv_atm,
                            skew: snap.skew,
                            put_call_ratio: snap.put_call_ratio,
                            dvol: snap.dvol,
                            updated,
                        },
                    );
                }
            }
        }

        // order_books: HashMap<RoundKey, OrderBookSnapshot> + secondary (Asset, Timeframe) index
        if let Ok(mut books) = self.state.order_books.write() {
            books.clear();
            let mut by_at = HashMap::new();
            for (key, snap) in &data.order_books {
                let updated = DateTime::parse_from_rfc3339(&snap.updated)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or(now);
                let ob = OrderBookSnapshot {
                    bids_up: snap.bids_up.clone(),
                    asks_up: snap.asks_up.clone(),
                    bids_down: snap.bids_down.clone(),
                    asks_down: snap.asks_down.clone(),
                    updated,
                };
                // Check if key is "BTC_5m" style (secondary) or condition_id (primary)
                if let Some((asset, tf)) = parse_asset_timeframe_key(key) {
                    by_at.insert((asset, tf), ob);
                } else {
                    books.insert(RoundKey { condition_id: key.clone() }, ob);
                }
            }
            // Populate secondary index
            if let Ok(mut at_books) = self.state.order_books_by_at.write() {
                *at_books = by_at;
            }
        }

        // trade_tapes: HashMap<RoundKey, VecDeque<PolyFill>>
        if let Ok(mut tapes) = self.state.trade_tapes.write() {
            tapes.clear();
            for (condition_id, fills) in &data.trade_tapes {
                let round_key = RoundKey { condition_id: condition_id.clone() };
                let deque: VecDeque<crate::state::PolyFill> = fills.iter().filter_map(|snap| {
                    let ts = DateTime::parse_from_rfc3339(&snap.timestamp)
                        .ok()?.with_timezone(&Utc);
                    let side = if snap.side == "Up" {
                        crate::state::TokenSide::Up
                    } else {
                        crate::state::TokenSide::Down
                    };
                    Some(crate::state::PolyFill {
                        side,
                        price: snap.price,
                        size: snap.size,
                        timestamp: ts,
                        is_buyer_maker: snap.is_buyer_maker,
                    })
                }).collect();
                if !deque.is_empty() {
                    tapes.insert(round_key, deque);
                }
            }
        }

        // coinbase_premium: HashMap<Asset, f64>
        if let Ok(mut premiums) = self.state.coinbase_premium.write() {
            premiums.clear();
            for (key, &premium) in &data.coinbase_premiums {
                if let Some(asset) = parse_asset(key) {
                    premiums.insert(asset, premium);
                }
            }
        }
    }
}

/// Parse an asset string like "BTC", "ETH", "SOL", "XRP" into an Asset enum.
fn parse_asset(s: &str) -> Option<Asset> {
    match s {
        "BTC" => Some(Asset::BTC),
        "ETH" => Some(Asset::ETH),
        "SOL" => Some(Asset::SOL),
        "XRP" => Some(Asset::XRP),
        _ => None,
    }
}

/// Parse a key like "bitcoin_5m" into (Asset, Timeframe).
fn parse_asset_timeframe_key(key: &str) -> Option<(Asset, Timeframe)> {
    let parts: Vec<&str> = key.splitn(2, '_').collect();
    if parts.len() != 2 { return None; }
    let asset = match parts[0] {
        "bitcoin" => Asset::BTC,
        "ethereum" => Asset::ETH,
        "solana" => Asset::SOL,
        "xrp" => Asset::XRP,
        _ => return None,
    };
    let tf = match parts[1] {
        "5m" => Timeframe::FiveMin,
        "15m" => Timeframe::FifteenMin,
        "1h" => Timeframe::OneHour,
        _ => return None,
    };
    Some((asset, tf))
}

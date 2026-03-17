//! Multi-source price feed with Binance primary + Pyth Hermes fallback
//! Maintains reference price cache for crypto round boundary detection

use std::collections::HashMap;
use std::sync::Arc;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::crypto::{Asset, Timeframe};

// === Types ===

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferencePrice {
    pub asset: Asset,
    pub timeframe: Timeframe,
    pub price: f64,
    pub recorded_at: DateTime<Utc>,
    pub source: PriceSource,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum PriceSource {
    Binance,
    Pyth,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candle {
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub open_time: i64,
    pub close_time: i64,
}

// === PriceFeedManager ===

pub struct PriceFeedManager {
    http: reqwest::Client,
    /// Cache: (Asset, Timeframe) -> reference price at round start
    reference_cache: Arc<RwLock<HashMap<(Asset, Timeframe), ReferencePrice>>>,
    /// Latest spot prices per asset
    spot_cache: Arc<RwLock<HashMap<Asset, (f64, DateTime<Utc>)>>>,
}

impl PriceFeedManager {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            reference_cache: Arc::new(RwLock::new(HashMap::new())),
            spot_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Fetch current price from Binance, falling back to Pyth
    pub async fn fetch_price(&self, asset: Asset) -> Result<(f64, PriceSource)> {
        // Try Binance first
        match self.fetch_binance_price(asset).await {
            Ok(price) => {
                self.spot_cache.write().await.insert(asset, (price, Utc::now()));
                return Ok((price, PriceSource::Binance));
            }
            Err(e) => {
                tracing::warn!("Binance price fetch failed for {:?}: {}", asset, e);
            }
        }

        // Fallback to Pyth
        let price = self.fetch_pyth_price(asset).await?;
        self.spot_cache.write().await.insert(asset, (price, Utc::now()));
        Ok((price, PriceSource::Pyth))
    }

    /// Binance REST: GET /api/v3/ticker/price
    async fn fetch_binance_price(&self, asset: Asset) -> Result<f64> {
        let url = format!(
            "https://api.binance.com/api/v3/ticker/price?symbol={}",
            asset.binance_symbol()
        );
        let resp: serde_json::Value = self.http.get(&url).send().await?.json().await?;
        let price_str = resp.get("price")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing price field in Binance response"))?;
        let price: f64 = price_str.parse()?;
        Ok(price)
    }

    /// Pyth Hermes: GET /v2/updates/price/latest
    async fn fetch_pyth_price(&self, asset: Asset) -> Result<f64> {
        let feed_id = asset.pyth_feed_id();
        let url = format!(
            "https://hermes.pyth.network/v2/updates/price/latest?ids[]={}",
            feed_id
        );
        let resp: serde_json::Value = self.http.get(&url).send().await?.json().await?;

        let parsed = resp.get("parsed")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .ok_or_else(|| anyhow::anyhow!("Empty Pyth response"))?;

        let price_data = parsed.get("price")
            .ok_or_else(|| anyhow::anyhow!("Missing price in Pyth response"))?;

        let price_str = price_data.get("price")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing price string"))?;

        let expo = price_data.get("expo")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);

        let raw_price: f64 = price_str.parse()?;
        let price = raw_price * 10_f64.powi(expo as i32);

        Ok(price)
    }

    /// Fetch candle data from Binance klines API
    pub async fn fetch_candles(&self, asset: Asset, timeframe: Timeframe, limit: usize) -> Result<Vec<Candle>> {
        let url = format!(
            "https://api.binance.com/api/v3/klines?symbol={}&interval={}&limit={}",
            asset.binance_symbol(),
            timeframe.binance_kline_interval(),
            limit,
        );
        let resp: Vec<serde_json::Value> = self.http.get(&url).send().await?.json().await?;

        let candles = resp.iter().filter_map(|k| {
            let arr = k.as_array()?;
            Some(Candle {
                open_time: arr.first()?.as_i64()?,
                open: arr.get(1)?.as_str()?.parse().ok()?,
                high: arr.get(2)?.as_str()?.parse().ok()?,
                low: arr.get(3)?.as_str()?.parse().ok()?,
                close: arr.get(4)?.as_str()?.parse().ok()?,
                volume: arr.get(5)?.as_str()?.parse().ok()?,
                close_time: arr.get(6)?.as_i64()?,
            })
        }).collect();

        Ok(candles)
    }

    /// Check if we're at a round boundary (now % interval == 0, ±2s tolerance)
    pub fn is_round_boundary(&self, timeframe: Timeframe) -> bool {
        let now_secs = Utc::now().timestamp() as u64;
        let interval = timeframe.seconds();
        let remainder = now_secs % interval;
        // Within ±2 seconds of the boundary
        remainder <= 2 || remainder >= (interval - 2)
    }

    /// Record reference price at round start
    pub async fn record_reference(&self, asset: Asset, timeframe: Timeframe) -> Result<f64> {
        let (price, source) = self.fetch_price(asset).await?;

        let reference = ReferencePrice {
            asset,
            timeframe,
            price,
            recorded_at: Utc::now(),
            source,
        };

        self.reference_cache.write().await.insert((asset, timeframe), reference);
        tracing::info!("Recorded reference price for {:?}/{:?}: {:.4} ({:?})", asset, timeframe, price, source);

        Ok(price)
    }

    /// Get cached reference price for a round (returns None if stale)
    pub async fn get_reference(&self, asset: Asset, timeframe: Timeframe) -> Option<ReferencePrice> {
        let cache = self.reference_cache.read().await;
        let ref_price = cache.get(&(asset, timeframe))?;
        // Reject stale references (older than 2x the round interval)
        let age_secs = (Utc::now() - ref_price.recorded_at).num_seconds();
        if age_secs > timeframe.seconds() as i64 * 2 {
            return None;
        }
        Some(ref_price.clone())
    }

    /// Get latest cached spot price
    pub async fn get_spot(&self, asset: Asset) -> Option<f64> {
        self.spot_cache.read().await.get(&asset).map(|(p, _)| *p)
    }

    /// Fetch prices for all assets in parallel
    pub async fn fetch_all_prices(&self) -> HashMap<Asset, f64> {
        let mut prices = HashMap::new();
        let mut handles = Vec::new();

        for asset in Asset::ALL {
            let http = self.http.clone();
            handles.push(tokio::spawn(async move {
                let url = format!(
                    "https://api.binance.com/api/v3/ticker/price?symbol={}",
                    asset.binance_symbol()
                );
                let result: Result<f64> = async {
                    let resp: serde_json::Value = http.get(&url).send().await?.json().await?;
                    let price_str = resp.get("price")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| anyhow::anyhow!("Missing price"))?;
                    Ok(price_str.parse()?)
                }.await;
                (asset, result)
            }));
        }

        for handle in handles {
            if let Ok((asset, Ok(price))) = handle.await {
                self.spot_cache.write().await.insert(asset, (price, Utc::now()));
                prices.insert(asset, price);
            }
        }

        prices
    }

    /// Check and record references for all round boundaries
    pub async fn check_boundaries(&self) {
        for timeframe in Timeframe::ALL {
            if self.is_round_boundary(timeframe) {
                for asset in Asset::ALL {
                    if let Err(e) = self.record_reference(asset, timeframe).await {
                        tracing::warn!("Failed to record reference {:?}/{:?}: {}", asset, timeframe, e);
                    }
                }
            }
        }
    }

    /// Get all reference prices (for API exposure)
    pub async fn all_references(&self) -> Vec<ReferencePrice> {
        self.reference_cache.read().await.values().cloned().collect()
    }

    /// Get all spot prices (for API exposure)
    pub async fn all_spots(&self) -> HashMap<Asset, f64> {
        self.spot_cache.read().await.iter().map(|(a, (p, _))| (*a, *p)).collect()
    }
}

//! Binance Futures REST adapter.
//! Polls funding rate, open interest, and taker buy/sell ratio every 60s.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::state::{DataState, FuturesState};
use crate::traits::{RestAdapter, SourceType};
use polybot_scanner::crypto::Asset;

const FAPI_BASE: &str = "https://fapi.binance.com";

/// Binance Futures REST adapter.
/// Polls funding rate, open interest, and taker buy/sell ratio.
pub struct BinanceFuturesRestAdapter {
    assets: Vec<Asset>,
    client: reqwest::Client,
    last_data: Arc<AtomicI64>,
}

impl BinanceFuturesRestAdapter {
    pub fn new(assets: Vec<Asset>) -> Self {
        Self {
            assets,
            client: reqwest::Client::new(),
            last_data: Arc::new(AtomicI64::new(0)),
        }
    }

    async fn fetch_funding_rate(&self, symbol: &str) -> Result<f64> {
        let url = format!(
            "{}/fapi/v1/fundingRate?symbol={}&limit=1",
            FAPI_BASE, symbol
        );
        let resp: Vec<serde_json::Value> = self.client.get(&url).send().await?.json().await?;
        let rate = resp
            .first()
            .and_then(|v| v.get("fundingRate"))
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        Ok(rate)
    }

    async fn fetch_open_interest(&self, symbol: &str) -> Result<f64> {
        let url = format!("{}/fapi/v1/openInterest?symbol={}", FAPI_BASE, symbol);
        let resp: serde_json::Value = self.client.get(&url).send().await?.json().await?;
        let oi = resp
            .get("openInterest")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        Ok(oi)
    }

    async fn fetch_taker_ratio(&self, symbol: &str) -> Result<f64> {
        let url = format!(
            "{}/futures/data/takerlongshortRatio?symbol={}&period=5m&limit=1",
            FAPI_BASE, symbol
        );
        let resp: Vec<serde_json::Value> = self.client.get(&url).send().await?.json().await?;
        let ratio = resp
            .first()
            .and_then(|v| v.get("buySellRatio"))
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(1.0);
        Ok(ratio)
    }
}

#[async_trait]
impl RestAdapter for BinanceFuturesRestAdapter {
    fn name(&self) -> &str {
        "binance-futures-rest"
    }

    fn source_type(&self) -> SourceType {
        SourceType::UnderlyingAsset
    }

    async fn fetch(&self, state: &DataState) -> Result<()> {
        for asset in &self.assets {
            let symbol = asset.binance_symbol();

            // Fetch all 3 endpoints concurrently
            let (funding, oi, taker) = tokio::join!(
                self.fetch_funding_rate(symbol),
                self.fetch_open_interest(symbol),
                self.fetch_taker_ratio(symbol),
            );

            let now = Utc::now();

            // Read previous OI to compute change
            let prev_oi = {
                let states = state.futures_state.read().unwrap();
                states.get(asset).map(|s| s.open_interest).unwrap_or(0.0)
            };

            let current_oi = oi.unwrap_or(0.0);
            let oi_change = if prev_oi > 0.0 {
                (current_oi - prev_oi) / prev_oi
            } else {
                0.0
            };

            let fs = FuturesState {
                funding_rate: funding.unwrap_or(0.0),
                funding_updated: now,
                open_interest: current_oi,
                oi_change_5m: oi_change,
                taker_buy_sell_ratio: taker.unwrap_or(1.0),
                ..Default::default()
            };

            {
                let mut states = state.futures_state.write().unwrap();
                states.insert(*asset, fs);
            }
        }

        self.last_data
            .store(Utc::now().timestamp(), Ordering::Relaxed);
        tracing::debug!(
            assets = self.assets.len(),
            "BinanceFuturesRest: fetched data"
        );
        Ok(())
    }

    fn poll_interval(&self) -> Duration {
        Duration::from_secs(60)
    }

    fn is_healthy(&self) -> bool {
        let ts = self.last_data.load(Ordering::Relaxed);
        if ts == 0 {
            return false;
        }
        let elapsed = Utc::now().timestamp() - ts;
        elapsed < 180 // healthy if data within 3 minutes
    }

    fn last_data_at(&self) -> Option<DateTime<Utc>> {
        let ts = self.last_data.load(Ordering::Relaxed);
        if ts > 0 {
            DateTime::from_timestamp(ts, 0)
        } else {
            None
        }
    }

    fn last_data_atomic(&self) -> Arc<AtomicI64> {
        self.last_data.clone()
    }
}

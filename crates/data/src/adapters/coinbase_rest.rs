/// Coinbase REST adapter.
/// Polls Coinbase spot prices every 30s.
/// Writes to state.coinbase_premium (price difference vs Binance).

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::state::DataState;
use crate::traits::{RestAdapter, SourceType};
use polybot_scanner::crypto::Asset;

const COINBASE_BASE: &str = "https://api.coinbase.com/v2/prices";

/// Coinbase REST adapter.
/// Fetches spot prices and computes cross-exchange premium vs Binance.
pub struct CoinbaseRestAdapter {
    assets: Vec<Asset>,
    client: reqwest::Client,
    last_data: Arc<AtomicI64>,
}

impl CoinbaseRestAdapter {
    pub fn new(assets: Vec<Asset>) -> Self {
        Self {
            assets,
            client: reqwest::Client::new(),
            last_data: Arc::new(AtomicI64::new(0)),
        }
    }

    /// Map asset to Coinbase pair string (e.g. "BTC-USD").
    fn coinbase_pair(asset: &Asset) -> &'static str {
        match asset {
            Asset::BTC => "BTC-USD",
            Asset::ETH => "ETH-USD",
            Asset::SOL => "SOL-USD",
            Asset::XRP => "XRP-USD",
        }
    }

    /// Fetch spot price for a single asset from Coinbase.
    async fn fetch_spot_price(&self, asset: &Asset) -> Result<f64> {
        let pair = Self::coinbase_pair(asset);
        let url = format!("{}/{}/spot", COINBASE_BASE, pair);
        let resp: serde_json::Value = self.client.get(&url).send().await?.json().await?;
        let price = resp
            .get("data")
            .and_then(|d| d.get("amount"))
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .ok_or_else(|| anyhow::anyhow!("bad Coinbase response for {}", pair))?;
        Ok(price)
    }
}

#[async_trait]
impl RestAdapter for CoinbaseRestAdapter {
    fn name(&self) -> &str {
        "coinbase-rest"
    }

    fn source_type(&self) -> SourceType {
        SourceType::UnderlyingAsset
    }

    async fn fetch(&self, state: &DataState) -> Result<()> {
        for asset in &self.assets {
            let cb_price = match self.fetch_spot_price(asset).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(asset = ?asset, error = %e, "CoinbaseRest: failed to fetch");
                    continue;
                }
            };

            // Read Binance price from shared state
            let binance_price = {
                let prices = state.latest_prices.read().unwrap();
                prices.get(asset).map(|(p, _)| *p)
            };

            let premium = match binance_price {
                Some(bp) if bp > 0.0 => (cb_price - bp) / bp,
                _ => 0.0,
            };

            {
                let mut premiums = state.coinbase_premium.write().unwrap();
                premiums.insert(*asset, premium);
            }

            tracing::debug!(
                asset = ?asset,
                coinbase = cb_price,
                premium = format!("{:.6}", premium),
                "CoinbaseRest: updated premium"
            );
        }

        self.last_data
            .store(Utc::now().timestamp(), Ordering::Relaxed);
        tracing::debug!(
            assets = self.assets.len(),
            "CoinbaseRest: fetched data"
        );
        Ok(())
    }

    fn poll_interval(&self) -> Duration {
        Duration::from_secs(30)
    }

    fn is_healthy(&self) -> bool {
        let ts = self.last_data.load(Ordering::Relaxed);
        if ts == 0 {
            return false;
        }
        let elapsed = Utc::now().timestamp() - ts;
        elapsed < 90 // healthy if data within 90s (3x poll interval)
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

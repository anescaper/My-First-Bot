//! Polymarket CLOB client — REST API for market discovery and order book data.
//!
//! This module provides a client for two Polymarket APIs:
//! - **Gamma API** (`gamma-api.polymarket.com`): Market metadata, discovery, filtering.
//!   Public read-only API, no auth needed. Used to find active markets.
//! - **CLOB API** (`clob.polymarket.com`): Order book depth, fee rates, trading.
//!   Read endpoints are public; write endpoints (order placement) require ECDSA auth
//!   and are handled by the executor crate, not here.
//!
//! This client is used for general market scanning (non-crypto), while the
//! `CryptoScanner` handles crypto-specific Up/Down round discovery.

use anyhow::Result;
use crate::types::{Market, OrderBook};

/// Polymarket's Gamma API base URL — public market metadata and discovery.
const GAMMA_API: &str = "https://gamma-api.polymarket.com";
/// Polymarket's CLOB API base URL — order book, fees, and trading.
const CLOB_API: &str = "https://clob.polymarket.com";

/// REST client for Polymarket market discovery and order book data.
///
/// Wraps `reqwest::Client` for HTTP calls to Gamma and CLOB APIs.
/// All methods are async and return `anyhow::Result` for ergonomic error handling.
pub struct ClobClient {
    /// Shared HTTP client (internally Arc'd by reqwest, cheap to clone).
    http: reqwest::Client,
}

impl ClobClient {
    /// Create a new CLOB client with a default HTTP client.
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }

    /// Fetch active markets from the Gamma API, sorted by liquidity descending.
    ///
    /// Returns up to `limit` markets that are active and not yet closed.
    /// For each market, also fetches the taker fee rate from the CLOB API
    /// concurrently (best-effort — defaults to 0 on failure).
    ///
    /// Market outcomes are hardcoded to `["Yes", "No"]` because Polymarket's
    /// general markets are binary Yes/No. Crypto Up/Down markets use the
    /// CryptoScanner instead.
    ///
    /// The `spread` field is initialized to 0.0 here and must be computed
    /// separately from order book data via `fetch_order_book`.
    pub async fn fetch_markets(&self, limit: usize) -> Result<Vec<Market>> {
        let url = format!(
            "{}/markets?limit={}&order=liquidity&ascending=false&active=true&closed=false",
            GAMMA_API, limit
        );
        let resp: Vec<serde_json::Value> = self.http.get(&url).send().await?.json().await?;

        let markets: Vec<Market> = resp.into_iter().filter_map(|m| {
            let token_ids: Vec<String> = serde_json::from_str(m.get("clobTokenIds")?.as_str()?).ok()?;
            let prices: Vec<String> = m.get("outcomePrices")
                .and_then(|v| v.as_str())
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();

            Some(Market {
                condition_id: m.get("conditionId")?.as_str()?.to_string(),
                question: m.get("question").or(m.get("groupItemTitle"))?.as_str()?.to_string(),
                token_ids,
                outcomes: vec!["Yes".into(), "No".into()],
                outcome_prices: prices.iter().filter_map(|p| p.parse().ok()).collect(),
                liquidity: m.get("liquidity")?.as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
                volume_24h: m.get("volume")?.as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
                end_date: m.get("endDate").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()),
                spread: 0.0, // Computed from order book
                active: true,
                neg_risk: m.get("neg_risk").or(m.get("negRisk")).and_then(|v| v.as_bool()).unwrap_or(false),
                fee_rate_bps: 0,
            })
        }).collect();

        // Fetch per-market fee rates from CLOB API (concurrent, best-effort)
        let mut markets = markets;
        let mut handles = Vec::new();
        for m in &markets {
            let token_id = m.token_ids.first().cloned().unwrap_or_default();
            let url = format!("{}/fee-rate?token_id={}", CLOB_API, token_id);
            let client = self.http.clone();
            handles.push(tokio::spawn(async move {
                let resp = client.get(&url).send().await.ok()?;
                let v: serde_json::Value = resp.json().await.ok()?;
                v.get("base_fee").and_then(|f| f.as_u64())
            }));
        }
        for (m, handle) in markets.iter_mut().zip(handles) {
            m.fee_rate_bps = handle.await.ok().flatten().unwrap_or(0);
        }

        Ok(markets)
    }

    /// Fetch the full order book (bids + asks) for a specific token from the CLOB API.
    ///
    /// Returns an `OrderBook` with sorted levels (bids descending, asks ascending),
    /// computed mid_price and spread. The defensive sort is included because while
    /// the API usually returns sorted data, we can't rely on that invariant.
    ///
    /// `token_id`: The CLOB token ID (not condition_id). Each market outcome
    /// has its own token_id.
    pub async fn fetch_order_book(&self, token_id: &str) -> Result<OrderBook> {
        let url = format!("{}/book?token_id={}", CLOB_API, token_id);
        let resp: serde_json::Value = self.http.get(&url).send().await?.json().await?;

        let parse_levels = |side: &str| -> Vec<crate::types::OrderBookLevel> {
            resp.get(side)
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter().filter_map(|l| {
                        Some(crate::types::OrderBookLevel {
                            price: l.get("price")?.as_str()?.parse().ok()?,
                            size: l.get("size")?.as_str()?.parse().ok()?,
                        })
                    }).collect()
                })
                .unwrap_or_default()
        };

        let mut bids = parse_levels("bids");
        let mut asks = parse_levels("asks");
        // Defensive sort: bids descending, asks ascending (API usually returns sorted, but be safe)
        bids.sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap_or(std::cmp::Ordering::Equal));
        asks.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));

        let best_bid = bids.first().map(|l| l.price).unwrap_or(0.0);
        let best_ask = asks.first().map(|l| l.price).unwrap_or(1.0);
        let mid_price = (best_bid + best_ask) / 2.0;
        let spread = best_ask - best_bid;

        Ok(OrderBook {
            market_id: token_id.to_string(),
            bids,
            asks,
            mid_price,
            spread,
            timestamp: chrono::Utc::now(),
        })
    }
}

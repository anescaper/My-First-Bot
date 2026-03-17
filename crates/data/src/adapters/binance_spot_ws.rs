//! Binance Spot WebSocket adapter.
//! Connects to combined stream for multi-asset trade data.
//! Produces micro-candles via CandleBuilder and updates latest prices.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

use crate::candle_builder::CandleBuilder;
use crate::state::DataState;
use crate::traits::{SourceType, TickAdapter};
use polybot_scanner::crypto::Asset;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Binance Spot WebSocket adapter.
/// Connects to `wss://stream.binance.com:9443/stream?streams=...` for each asset.
/// Produces micro-candles via CandleBuilder.
pub struct BinanceSpotWsAdapter {
    assets: Vec<Asset>,
    candle_builders: Vec<(Asset, CandleBuilder)>,
    ws: Option<WsStream>,
    last_data: Arc<AtomicI64>,
}

impl BinanceSpotWsAdapter {
    pub fn new(assets: Vec<Asset>) -> Self {
        let candle_builders = assets
            .iter()
            .map(|a| (*a, CandleBuilder::new(5)))
            .collect();
        Self {
            assets,
            candle_builders,
            ws: None,
            last_data: Arc::new(AtomicI64::new(0)),
        }
    }

    fn asset_from_symbol(&self, symbol: &str) -> Option<Asset> {
        let upper = symbol.to_uppercase();
        self.assets.iter().find(|a| a.binance_symbol() == upper).copied()
    }
}

#[async_trait]
impl TickAdapter for BinanceSpotWsAdapter {
    fn name(&self) -> &str {
        "binance-spot-ws"
    }

    fn source_type(&self) -> SourceType {
        SourceType::UnderlyingAsset
    }

    async fn connect(&mut self) -> Result<()> {
        // Build combined stream URL: btcusdt@trade/ethusdt@trade/...
        let streams: Vec<String> = self
            .assets
            .iter()
            .map(|a| format!("{}@trade", a.binance_symbol().to_lowercase()))
            .collect();
        let url = format!(
            "wss://stream.binance.com:9443/stream?streams={}",
            streams.join("/")
        );

        tracing::info!(url = %url, "BinanceSpotWs: connecting");
        let (ws, _resp) = connect_async(&url).await?;
        self.ws = Some(ws);
        tracing::info!("BinanceSpotWs: connected");
        Ok(())
    }

    async fn disconnect(&mut self) {
        if let Some(mut ws) = self.ws.take() {
            let _ = ws.close(None).await;
        }
    }

    async fn subscribe(&mut self, _symbols: &[String]) -> Result<()> {
        // Combined stream auto-subscribes via URL — no explicit subscribe needed
        Ok(())
    }

    async fn poll_next(&mut self, state: &DataState) -> Result<bool> {
        let ws = self
            .ws
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("not connected"))?;

        // Read with 500ms timeout to avoid blocking forever
        let msg = match tokio::time::timeout(Duration::from_millis(500), ws.next()).await {
            Ok(Some(Ok(msg))) => msg,
            Ok(Some(Err(e))) => return Err(e.into()),
            Ok(None) => return Err(anyhow::anyhow!("WS stream ended")),
            Err(_) => return Ok(false), // timeout, no data
        };

        match msg {
            Message::Text(text) => {
                // Combined stream format: {"stream":"btcusdt@trade","data":{...}}
                let v: serde_json::Value = serde_json::from_str(&text)?;
                let stream = v.get("stream").and_then(|s| s.as_str()).unwrap_or("");
                let data = match v.get("data") {
                    Some(d) => d,
                    None => return Ok(false),
                };

                // Extract symbol from stream name (e.g., "btcusdt@trade" -> "BTCUSDT")
                let symbol = stream.split('@').next().unwrap_or("").to_uppercase();
                let asset = match self.asset_from_symbol(&symbol) {
                    Some(a) => a,
                    None => return Ok(false),
                };

                // Parse trade data
                let price: f64 = data
                    .get("p")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                let qty: f64 = data
                    .get("q")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                let trade_time_ms: i64 = data
                    .get("T")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let ts = DateTime::from_timestamp_millis(trade_time_ms)
                    .unwrap_or_else(Utc::now);

                if price <= 0.0 {
                    return Ok(false);
                }

                // Update latest price
                {
                    let mut prices = state.latest_prices.write().unwrap();
                    prices.insert(asset, (price, ts));
                }

                // Feed to CandleBuilder
                if let Some((_, builder)) = self
                    .candle_builders
                    .iter_mut()
                    .find(|(a, _)| *a == asset)
                {
                    if let Some(candle) = builder.feed(price, qty, ts) {
                        let mut candles = state.micro_candles.write().unwrap();
                        candles
                            .entry(asset)
                            .or_insert_with(VecDeque::new)
                            .push_back(candle);
                    }
                }

                self.last_data.store(Utc::now().timestamp(), Ordering::Relaxed);
                Ok(true)
            }
            Message::Ping(data) => {
                ws.send(Message::Pong(data)).await?;
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    fn is_healthy(&self) -> bool {
        self.ws.is_some()
    }

    fn last_data_at(&self) -> Option<DateTime<Utc>> {
        let ts = self.last_data.load(Ordering::Relaxed);
        if ts > 0 {
            DateTime::from_timestamp(ts, 0)
        } else {
            None
        }
    }

    fn inactivity_timeout(&self) -> Duration {
        Duration::from_secs(30)
    }

    fn last_data_atomic(&self) -> Arc<AtomicI64> {
        self.last_data.clone()
    }
}

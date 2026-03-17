//! Binance Futures WebSocket adapter.
//! Connects to combined stream for liquidation (forceOrder) events.
//! Writes liquidation events into DataState.futures_state.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

use crate::state::{DataState, LiquidationEvent};
use crate::traits::{SourceType, TickAdapter};
use polybot_scanner::crypto::Asset;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

const MAX_LIQUIDATIONS: usize = 200;

/// Binance Futures WebSocket adapter.
/// Connects to `wss://fstream.binance.com/stream?streams=...@forceOrder` for each asset.
/// Parses liquidation events and writes them into DataState.
pub struct BinanceFuturesWsAdapter {
    assets: Vec<Asset>,
    ws: Option<WsStream>,
    last_data: Arc<AtomicI64>,
}

impl BinanceFuturesWsAdapter {
    pub fn new(assets: Vec<Asset>) -> Self {
        Self {
            assets,
            ws: None,
            last_data: Arc::new(AtomicI64::new(0)),
        }
    }

    fn asset_from_symbol(&self, symbol: &str) -> Option<Asset> {
        let upper = symbol.to_uppercase();
        self.assets
            .iter()
            .find(|a| a.binance_symbol() == upper)
            .copied()
    }
}

#[async_trait]
impl TickAdapter for BinanceFuturesWsAdapter {
    fn name(&self) -> &str {
        "binance-futures-ws"
    }

    fn source_type(&self) -> SourceType {
        SourceType::UnderlyingAsset
    }

    async fn connect(&mut self) -> Result<()> {
        let streams: Vec<String> = self
            .assets
            .iter()
            .map(|a| format!("{}@forceOrder", a.binance_symbol().to_lowercase()))
            .collect();
        let url = format!(
            "wss://fstream.binance.com/stream?streams={}",
            streams.join("/")
        );

        tracing::info!(url = %url, "BinanceFuturesWs: connecting");
        let (ws, _resp) = connect_async(&url).await?;
        self.ws = Some(ws);
        tracing::info!("BinanceFuturesWs: connected");
        Ok(())
    }

    async fn disconnect(&mut self) {
        if let Some(mut ws) = self.ws.take() {
            let _ = ws.close(None).await;
        }
    }

    async fn subscribe(&mut self, _symbols: &[String]) -> Result<()> {
        // Combined stream auto-subscribes via URL
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
                // Combined stream format: {"stream":"btcusdt@forceOrder","data":{"e":"forceOrder","o":{...}}}
                let v: serde_json::Value = serde_json::from_str(&text)?;
                let stream = v.get("stream").and_then(|s| s.as_str()).unwrap_or("");
                let data = match v.get("data") {
                    Some(d) => d,
                    None => return Ok(false),
                };

                // Extract symbol from stream name (e.g., "btcusdt@forceOrder" -> "BTCUSDT")
                let symbol = stream.split('@').next().unwrap_or("").to_uppercase();
                let asset = match self.asset_from_symbol(&symbol) {
                    Some(a) => a,
                    None => return Ok(false),
                };

                // forceOrder data is nested under "o"
                let order = match data.get("o") {
                    Some(o) => o,
                    None => return Ok(false),
                };

                let side = order
                    .get("S")
                    .and_then(|v| v.as_str())
                    .unwrap_or("UNKNOWN")
                    .to_string();
                let price: f64 = order
                    .get("p")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                let quantity: f64 = order
                    .get("q")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                let trade_time_ms: i64 = order.get("T").and_then(|v| v.as_i64()).unwrap_or(0);
                let timestamp = DateTime::from_timestamp_millis(trade_time_ms)
                    .unwrap_or_else(Utc::now);

                if price <= 0.0 || quantity <= 0.0 {
                    return Ok(false);
                }

                let event = LiquidationEvent {
                    asset,
                    side,
                    price,
                    quantity,
                    timestamp,
                };

                tracing::debug!(
                    asset = ?asset,
                    side = %event.side,
                    price = %event.price,
                    qty = %event.quantity,
                    "Liquidation event"
                );

                // Write into futures_state
                {
                    let mut fs = state.futures_state.write().unwrap();
                    let entry = fs.entry(asset).or_default();
                    entry.recent_liquidations.push_back(event);
                    while entry.recent_liquidations.len() > MAX_LIQUIDATIONS {
                        entry.recent_liquidations.pop_front();
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
        // Liquidation events are sparse — minutes between events in calm markets
        Duration::from_secs(600)
    }

    fn last_data_atomic(&self) -> Arc<AtomicI64> {
        self.last_data.clone()
    }
}

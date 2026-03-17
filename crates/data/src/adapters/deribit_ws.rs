//! Deribit WebSocket adapter.
//! Connects to `wss://www.deribit.com/ws/api/v2` for DVOL (volatility index)
//! and options ticker data. Writes into DataState.options_state.
//!
//! Subscribes to:
//!   - `deribit_volatility_index.{btc,eth}_usd` (DVOL)
//!   - `ticker.{BTC,ETH}-PERPETUAL` (for mark price / funding if needed)
//!
//! Only BTC and ETH have Deribit options markets.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

use crate::state::DataState;
use crate::traits::{SourceType, TickAdapter};
use polybot_scanner::crypto::Asset;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

const DERIBIT_WS_URL: &str = "wss://www.deribit.com/ws/api/v2";

/// Assets that have Deribit options markets.
const DERIBIT_ASSETS: &[Asset] = &[Asset::BTC, Asset::ETH];

pub struct DeribitWsAdapter {
    ws: Option<WsStream>,
    last_data: Arc<AtomicI64>,
    msg_id: u64,
}

impl DeribitWsAdapter {
    pub fn new() -> Self {
        Self {
            ws: None,
            last_data: Arc::new(AtomicI64::new(0)),
            msg_id: 0,
        }
    }

    fn next_id(&mut self) -> u64 {
        self.msg_id += 1;
        self.msg_id
    }

    fn asset_from_channel(channel: &str) -> Option<Asset> {
        let lower = channel.to_lowercase();
        if lower.contains("btc") {
            Some(Asset::BTC)
        } else if lower.contains("eth") {
            Some(Asset::ETH)
        } else {
            None
        }
    }
}

#[async_trait]
impl TickAdapter for DeribitWsAdapter {
    fn name(&self) -> &str {
        "deribit-ws"
    }

    fn source_type(&self) -> SourceType {
        SourceType::UnderlyingAsset
    }

    async fn connect(&mut self) -> Result<()> {
        tracing::info!("DeribitWs: connecting to {}", DERIBIT_WS_URL);
        let (ws, _resp) = connect_async(DERIBIT_WS_URL).await?;
        self.ws = Some(ws);
        tracing::info!("DeribitWs: connected");

        // Subscribe to DVOL channels
        let channels: Vec<String> = DERIBIT_ASSETS
            .iter()
            .map(|a| {
                let slug = match a {
                    Asset::BTC => "btc",
                    Asset::ETH => "eth",
                    _ => unreachable!(),
                };
                format!("deribit_volatility_index.{slug}_usd")
            })
            .collect();

        let subscribe_msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": "public/subscribe",
            "params": {
                "channels": channels,
            }
        });

        if let Some(ws) = self.ws.as_mut() {
            ws.send(Message::Text(subscribe_msg.to_string().into()))
                .await?;
            tracing::info!("DeribitWs: subscribed to {:?}", channels);
        }

        Ok(())
    }

    async fn disconnect(&mut self) {
        if let Some(mut ws) = self.ws.take() {
            let _ = ws.close(None).await;
        }
    }

    async fn subscribe(&mut self, _symbols: &[String]) -> Result<()> {
        // Subscriptions are done in connect()
        Ok(())
    }

    async fn poll_next(&mut self, state: &DataState) -> Result<bool> {
        let ws = self
            .ws
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("not connected"))?;

        let msg = match tokio::time::timeout(Duration::from_millis(500), ws.next()).await {
            Ok(Some(Ok(msg))) => msg,
            Ok(Some(Err(e))) => return Err(e.into()),
            Ok(None) => return Err(anyhow::anyhow!("WS stream ended")),
            Err(_) => return Ok(false),
        };

        match msg {
            Message::Text(text) => {
                let v: serde_json::Value = serde_json::from_str(&text)?;

                // Deribit subscription notifications come as:
                // {"jsonrpc":"2.0","method":"subscription","params":{"channel":"...","data":{...}}}
                let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("");
                if method != "subscription" {
                    // Could be subscription confirmation or other RPC response
                    return Ok(false);
                }

                let params = match v.get("params") {
                    Some(p) => p,
                    None => return Ok(false),
                };

                let channel = params
                    .get("channel")
                    .and_then(|c| c.as_str())
                    .unwrap_or("");
                let data = match params.get("data") {
                    Some(d) => d,
                    None => return Ok(false),
                };

                // Parse DVOL channel: deribit_volatility_index.btc_usd
                if channel.starts_with("deribit_volatility_index.") {
                    let asset = match Self::asset_from_channel(channel) {
                        Some(a) => a,
                        None => return Ok(false),
                    };

                    // Data format: {"volatility": 45.2, "timestamp": 1710000000000, ...}
                    let dvol = data
                        .get("volatility")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);

                    if dvol <= 0.0 {
                        return Ok(false);
                    }

                    // DVOL is annualized vol in percentage (e.g., 45.0 = 45%)
                    let iv_atm = dvol / 100.0; // Convert to decimal

                    tracing::debug!(
                        asset = ?asset,
                        dvol = dvol,
                        iv_atm = iv_atm,
                        "Deribit DVOL update"
                    );

                    {
                        let mut opts = state.options_state.write().unwrap();
                        let entry = opts.entry(asset).or_default();
                        entry.dvol = dvol;
                        entry.iv_atm = iv_atm;
                        entry.updated = Utc::now();
                    }

                    self.last_data.store(Utc::now().timestamp(), Ordering::Relaxed);
                    return Ok(true);
                }

                Ok(false)
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
        // DVOL updates every ~1s, but give generous timeout
        Duration::from_secs(120)
    }

    fn last_data_atomic(&self) -> Arc<AtomicI64> {
        self.last_data.clone()
    }
}

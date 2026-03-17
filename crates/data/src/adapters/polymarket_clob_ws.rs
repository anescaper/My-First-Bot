//! Polymarket CLOB WebSocket adapter.
//! Connects to wss://ws-subscriptions-clob.polymarket.com/ws/market.
//! Handles price_change, last_trade_price, book events.
//! CRITICAL: 120s inactivity timeout for known silent freeze bug.
//! Sends PING every 10s to keep connection alive.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

use crate::state::{OrderBookSnapshot, PolyFill, RoundKey, TokenSide, TokenTick, DataState};
use crate::traits::{SourceType, TickAdapter};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

const CLOB_WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";
const PING_INTERVAL: Duration = Duration::from_secs(10);

/// Polymarket CLOB WebSocket adapter.
/// Subscribe to token IDs via shared subscription list.
pub struct PolymarketClobWsAdapter {
    ws: Option<WsStream>,
    last_data: Arc<AtomicI64>,
    last_ping: Instant,
    /// Shared subscription list — updated externally by the pipeline.
    /// Maps token_id → (condition_id, side)
    subscriptions: Arc<RwLock<HashMap<String, (String, TokenSide)>>>,
    /// Track what we've actually subscribed to on the WS
    active_token_ids: Vec<String>,
}

impl PolymarketClobWsAdapter {
    pub fn new(subscriptions: Arc<RwLock<HashMap<String, (String, TokenSide)>>>) -> Self {
        Self {
            ws: None,
            last_data: Arc::new(AtomicI64::new(0)),
            last_ping: Instant::now(),
            subscriptions,
            active_token_ids: Vec::new(),
        }
    }

    /// Check if subscriptions changed and re-subscribe if needed.
    async fn sync_subscriptions(&mut self) -> Result<()> {
        let current_ids: Vec<String> = {
            let subs = self.subscriptions.read().unwrap();
            subs.keys().cloned().collect()
        };

        if current_ids.is_empty() {
            return Ok(());
        }
        // Use set comparison — HashMap iteration order is non-deterministic
        let current_set: std::collections::HashSet<&String> = current_ids.iter().collect();
        let active_set: std::collections::HashSet<&String> = self.active_token_ids.iter().collect();
        if current_set == active_set {
            return Ok(());
        }

        let ws = match self.ws.as_mut() {
            Some(ws) => ws,
            None => return Ok(()),
        };

        // Send subscribe message
        let sub_msg = serde_json::json!({
            "auth": {},
            "type": "market",
            "assets_ids": current_ids,
        });
        ws.send(Message::Text(sub_msg.to_string().into())).await?;
        tracing::info!(
            count = current_ids.len(),
            "PolymarketClobWs: subscribed to tokens"
        );
        self.active_token_ids = current_ids;
        Ok(())
    }

    fn lookup_round(&self, asset_id: &str) -> Option<(String, TokenSide)> {
        let subs = self.subscriptions.read().unwrap();
        subs.get(asset_id).cloned()
    }
}

#[async_trait]
impl TickAdapter for PolymarketClobWsAdapter {
    fn name(&self) -> &str {
        "polymarket-clob-ws"
    }

    fn source_type(&self) -> SourceType {
        SourceType::PredictionMarket
    }

    async fn connect(&mut self) -> Result<()> {
        tracing::info!("PolymarketClobWs: connecting to {}", CLOB_WS_URL);
        let (ws, _resp) = connect_async(CLOB_WS_URL).await?;
        self.ws = Some(ws);
        self.last_ping = Instant::now();
        self.active_token_ids.clear();
        tracing::info!("PolymarketClobWs: connected");
        Ok(())
    }

    async fn disconnect(&mut self) {
        if let Some(mut ws) = self.ws.take() {
            let _ = ws.close(None).await;
        }
        self.active_token_ids.clear();
    }

    async fn subscribe(&mut self, symbols: &[String]) -> Result<()> {
        // Update shared subscription map (pipeline calls this with token IDs)
        // For simple subscribe, just mark them as Up tokens (the pipeline handles mapping)
        let ws = match self.ws.as_mut() {
            Some(ws) => ws,
            None => return Err(anyhow::anyhow!("not connected")),
        };

        if symbols.is_empty() {
            return Ok(());
        }

        let sub_msg = serde_json::json!({
            "auth": {},
            "type": "market",
            "assets_ids": symbols,
        });
        ws.send(Message::Text(sub_msg.to_string().into())).await?;
        self.active_token_ids = symbols.to_vec();
        tracing::info!(count = symbols.len(), "PolymarketClobWs: subscribed");
        Ok(())
    }

    async fn poll_next(&mut self, state: &DataState) -> Result<bool> {
        if self.ws.is_none() {
            return Err(anyhow::anyhow!("not connected"));
        }

        // Check for subscription updates (before borrowing ws)
        self.sync_subscriptions().await?;

        // Send PING if due
        if self.last_ping.elapsed() >= PING_INTERVAL {
            let ws = self.ws.as_mut().unwrap();
            ws.send(Message::Text("PING".into())).await?;
            self.last_ping = Instant::now();
        }

        // Read with 500ms timeout
        let ws = self.ws.as_mut().unwrap();
        let msg = match tokio::time::timeout(Duration::from_millis(500), ws.next()).await {
            Ok(Some(Ok(msg))) => msg,
            Ok(Some(Err(e))) => return Err(e.into()),
            Ok(None) => return Err(anyhow::anyhow!("WS stream ended")),
            Err(_) => return Ok(false), // timeout
        };

        match msg {
            Message::Text(text) => {
                let text_str: &str = &text;

                // Server PONG response
                if text_str == "PONG" {
                    return Ok(false);
                }

                let v: serde_json::Value = match serde_json::from_str(text_str) {
                    Ok(v) => v,
                    Err(_) => return Ok(false),
                };

                // Handle array of events
                let events = if v.is_array() {
                    v.as_array().cloned().unwrap_or_default()
                } else {
                    vec![v]
                };

                let mut produced = false;
                let now = Utc::now();

                for event in &events {
                    let event_type = event
                        .get("event_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let asset_id = event
                        .get("asset_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");

                    if asset_id.is_empty() {
                        continue;
                    }

                    let (condition_id, side) = match self.lookup_round(asset_id) {
                        Some(info) => info,
                        None => continue,
                    };
                    let round_key = RoundKey { condition_id };

                    match event_type {
                        "price_change" => {
                            let price: f64 = event
                                .get("price")
                                .and_then(|v| v.as_str().or_else(|| v.as_f64().map(|_| "")))
                                .and_then(|s| if s.is_empty() {
                                    event.get("price").and_then(|v| v.as_f64())
                                } else {
                                    s.parse().ok()
                                })
                                .unwrap_or(0.0);

                            if price > 0.0 {
                                let mut prices = state.token_prices.write().unwrap();
                                let ticks = prices
                                    .entry(round_key)
                                    .or_insert_with(VecDeque::new);

                                let tick = match side {
                                    TokenSide::Up => TokenTick {
                                        p_up: price,
                                        p_down: 1.0 - price,
                                        timestamp: now,
                                    },
                                    TokenSide::Down => TokenTick {
                                        p_up: 1.0 - price,
                                        p_down: price,
                                        timestamp: now,
                                    },
                                };
                                ticks.push_back(tick);
                                produced = true;
                            }
                        }
                        "last_trade_price" => {
                            let price: f64 = event
                                .get("price")
                                .and_then(|v| v.as_str())
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(0.0);
                            let size: f64 = event
                                .get("size")
                                .and_then(|v| v.as_str())
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(0.0);

                            if price > 0.0 {
                                let fill = PolyFill {
                                    side,
                                    price,
                                    size,
                                    timestamp: now,
                                    is_buyer_maker: event
                                        .get("maker_address")
                                        .is_some(),
                                };
                                let mut tapes = state.trade_tapes.write().unwrap();
                                tapes
                                    .entry(round_key)
                                    .or_insert_with(VecDeque::new)
                                    .push_back(fill);
                                produced = true;
                            }
                        }
                        "book" => {
                            // Parse order book snapshot, merging with existing other side
                            let mut books = state.order_books.write().unwrap();
                            let existing = books.get(&round_key);
                            if let Some(snapshot) = parse_book_event(event, side, now, existing) {
                                books.insert(round_key, snapshot);
                                produced = true;
                            }
                        }
                        _ => {}
                    }
                }

                if produced {
                    self.last_data
                        .store(now.timestamp(), Ordering::Relaxed);
                }
                Ok(produced)
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
        // 120s — critical for known silent freeze bug
        Duration::from_secs(120)
    }

    fn last_data_atomic(&self) -> Arc<AtomicI64> {
        self.last_data.clone()
    }
}

/// Parse a "book" event and merge into existing OrderBookSnapshot.
/// Returns the merged snapshot (preserves the other side's data).
fn parse_book_event(
    event: &serde_json::Value,
    side: TokenSide,
    now: DateTime<Utc>,
    existing: Option<&OrderBookSnapshot>,
) -> Option<OrderBookSnapshot> {
    let bids = parse_levels(event.get("bids")?)?;
    let asks = parse_levels(event.get("asks")?)?;

    let (bids_up, asks_up, bids_down, asks_down) = match side {
        TokenSide::Up => (
            bids,
            asks,
            existing.map(|e| e.bids_down.clone()).unwrap_or_default(),
            existing.map(|e| e.asks_down.clone()).unwrap_or_default(),
        ),
        TokenSide::Down => (
            existing.map(|e| e.bids_up.clone()).unwrap_or_default(),
            existing.map(|e| e.asks_up.clone()).unwrap_or_default(),
            bids,
            asks,
        ),
    };

    Some(OrderBookSnapshot {
        bids_up,
        asks_up,
        bids_down,
        asks_down,
        updated: now,
    })
}

/// Parse price levels array: [{"price":"0.5","size":"100"}, ...]
fn parse_levels(v: &serde_json::Value) -> Option<Vec<(f64, f64)>> {
    let arr = v.as_array()?;
    Some(
        arr.iter()
            .filter_map(|item| {
                let price = item.get("price")?.as_str()?.parse::<f64>().ok()?;
                let size = item.get("size")?.as_str()?.parse::<f64>().ok()?;
                Some((price, size))
            })
            .collect(),
    )
}

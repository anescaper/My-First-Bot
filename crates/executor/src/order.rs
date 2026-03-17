//! Polymarket CLOB order types and structures.
//!
//! This module defines the order types needed to interact with Polymarket's
//! Central Limit Order Book (CLOB) on Polygon. Orders follow the EIP-712
//! typed data signing standard and are submitted via REST API.
//!
//! Key concepts:
//! - **Conditional tokens**: each binary outcome (Yes/No or Up/Down) has a token_id
//! - **BUY side**: spend USDC to buy conditional tokens (opening a position)
//! - **SELL side**: sell conditional tokens for USDC (closing a position)
//! - **Token amounts**: all amounts are in base units (6 decimals, like USDC)
//! - **Price**: probability price 0.01-0.99, rounded to 1-cent ticks

use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};

// --- Polymarket CLOB order types ---

/// Side for CLOB orders.
///
/// For new positions: ALWAYS use Buy. Direction (Up vs Down) is determined by
/// which token_id you choose (token_id_up or token_id_down), not by BUY/SELL.
/// SELL is only used when closing an existing position.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ClobSide {
    #[default]
    Buy,
    Sell,
}

/// CLOB order execution type — determines how the order interacts with the book.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ClobOrderType {
    /// Good-Til-Cancelled: sits on the book until filled, cancelled, or market closes.
    /// Used for live trading — better fill rate than FOK in illiquid markets.
    Gtc,
    /// Fill-Or-Kill: must fill entirely at the limit price or be cancelled immediately.
    /// Used for paper trading simulation and general-pipeline SELLs.
    Fok,
    /// Immediate-Or-Cancel: fill as much as possible, cancel the rest.
    /// Not currently used by the bot but supported by the CLOB.
    Ioc,
}

/// USDC and conditional token decimals on Polygon.
/// Hardcoded because both USDC and Polymarket conditional tokens use 6 decimal places
/// on the Polygon network. This matches the ERC-20 token standard for USDC.
const TOKEN_DECIMALS: u32 = 6;

/// Convert a float to base units (6 decimals), truncated to `decimals` decimal places.
///
/// Polymarket CLOB enforces precision rules:
/// - Shares (conditional tokens): max 2 decimal places
/// - USDC amounts: max 4 decimal places
/// - CLOB validation: maker_amount must equal taker_human * price at correct precision
///
/// Uses round (not floor) to avoid floating-point truncation artifacts:
/// e.g., 5.18 * 0.82 = 4.247599999... in f64, round gives 4.2476 not 4.2475
fn to_base_units_dp(amount: f64, decimals: u32) -> u128 {
    let factor = 10f64.powi(decimals as i32);
    // Use round (not floor) to avoid floating-point truncation artifacts
    // e.g., 5.18 * 0.82 = 4.247599999... in f64, round→4.2476 not floor→4.2475
    let rounded = (amount * factor).round() / factor;
    (rounded * 10f64.powi(TOKEN_DECIMALS as i32)).round() as u128
}

/// Convenience: truncate to 2 decimal places (shares).
pub fn to_base_units(amount: f64) -> u128 {
    to_base_units_dp(amount, 2)
}

/// The raw EIP-712 order struct that gets signed and submitted.
/// Fields match the Polymarket CTF Exchange contract.
/// Note: `side` is u8 internally (0=BUY, 1=SELL) for EIP-712 signing,
/// but serializes to "BUY"/"SELL" string for the CLOB API.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolymarketOrder {
    pub salt: u128,
    pub maker: String,
    pub signer: String,
    pub taker: String,        // "0x0000000000000000000000000000000000000000"
    pub token_id: String,     // CLOB token ID (decimal string)
    #[serde(serialize_with = "ser_u128_str", deserialize_with = "de_u128_str")]
    pub maker_amount: u128,   // USDC base units (what maker gives)
    #[serde(serialize_with = "ser_u128_str", deserialize_with = "de_u128_str")]
    pub taker_amount: u128,   // Conditional token base units (what maker receives)
    #[serde(serialize_with = "ser_u128_str", deserialize_with = "de_u128_str")]
    pub expiration: u128,
    #[serde(serialize_with = "ser_u128_str", deserialize_with = "de_u128_str")]
    pub nonce: u128,
    #[serde(serialize_with = "ser_u128_str", deserialize_with = "de_u128_str")]
    pub fee_rate_bps: u128,
    #[serde(serialize_with = "ser_side", deserialize_with = "de_side")]
    pub side: u8,             // 0 = BUY, 1 = SELL (serializes as "BUY"/"SELL")
    pub signature_type: u8,   // 0 = EOA
    #[serde(default)]
    pub signature: String,    // "0x..." EIP-712 hex signature
    #[serde(skip)]
    pub neg_risk: bool,       // false = standard CTF Exchange, true = Neg Risk CTF Exchange
}

fn ser_u128_str<S: serde::Serializer>(val: &u128, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&val.to_string())
}

fn de_u128_str<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
    let s: String = Deserialize::deserialize(d)?;
    s.parse().map_err(serde::de::Error::custom)
}

fn ser_side<S: serde::Serializer>(val: &u8, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(if *val == 0 { "BUY" } else { "SELL" })
}

fn de_side<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u8, D::Error> {
    let s: String = Deserialize::deserialize(d)?;
    match s.as_str() {
        "BUY" => Ok(0),
        "SELL" => Ok(1),
        _ => Err(serde::de::Error::custom(format!("invalid side: {}", s))),
    }
}

impl PolymarketOrder {
    /// Build a BUY order for a given token at a price with a USD size.
    /// price: probability price (0.0 - 1.0)
    /// size_usd: amount in USD to spend
    pub fn new_buy(
        token_id: String,
        price: f64,
        size_usd: f64,
        maker_address: &str,
        signer_address: &str,
        fee_rate_bps: u64,
        neg_risk: bool,
    ) -> Self {
        // Round to 1-cent tick size, then clamp to CLOB-valid range (0, 1) exclusive
        let price_clamped = ((price * 100.0).round() / 100.0).clamp(0.01, 0.99);
        // BUY: taker (shares, 2dp) then maker (USDC, 4dp) derived from taker × price.
        // CLOB validates: maker_amount == taker_human × price, both at correct precision.
        let taker_amount = to_base_units_dp(size_usd / price_clamped, 2); // shares: max 2 decimals
        let taker_human = taker_amount as f64 / 1_000_000.0;
        let maker_amount = to_base_units_dp(taker_human * price_clamped, 4); // USDC: max 4 decimals

        // Random salt for uniqueness
        let salt: u128 = rand_salt();

        // signature_type: 0=EOA (maker==signer), 2=POLY_GNOSIS_SAFE (maker is Gnosis Safe proxy wallet)
        let sig_type = if maker_address.eq_ignore_ascii_case(signer_address) { 0 } else { 2 };

        Self {
            salt,
            maker: maker_address.to_string(),
            signer: signer_address.to_string(),
            taker: "0x0000000000000000000000000000000000000000".to_string(),
            token_id,
            maker_amount,
            taker_amount,
            expiration: 0,  // No expiration
            nonce: 0,
            fee_rate_bps: fee_rate_bps as u128,
            side: 0,  // BUY
            signature_type: sig_type,
            signature: String::new(),
            neg_risk,
        }
    }

    /// Build a SELL order to close a position.
    /// price: probability price (0.0 - 1.0)
    /// shares: number of conditional tokens to sell (NOT USD amount)
    pub fn new_sell(
        token_id: String,
        price: f64,
        shares: f64,
        maker_address: &str,
        signer_address: &str,
        fee_rate_bps: u64,
        neg_risk: bool,
    ) -> Self {
        // Round to 1-cent tick size, then clamp to CLOB-valid range (0, 1) exclusive
        let price_clamped = ((price * 100.0).round() / 100.0).clamp(0.01, 0.99);
        // SELL: maker (shares, 2dp) then taker (USDC, 4dp) derived from maker × price.
        let maker_amount = to_base_units_dp(shares, 2); // shares: max 2 decimals
        let maker_human = maker_amount as f64 / 1_000_000.0;
        let taker_amount = to_base_units_dp(maker_human * price_clamped, 4); // USDC: max 4 decimals

        let salt: u128 = rand_salt();

        // signature_type: 0=EOA (maker==signer), 2=POLY_GNOSIS_SAFE (maker is Gnosis Safe proxy wallet)
        let sig_type = if maker_address.eq_ignore_ascii_case(signer_address) { 0 } else { 2 };

        Self {
            salt,
            maker: maker_address.to_string(),
            signer: signer_address.to_string(),
            taker: "0x0000000000000000000000000000000000000000".to_string(),
            token_id,
            maker_amount,
            taker_amount,
            expiration: 0,
            nonce: 0,
            fee_rate_bps: fee_rate_bps as u128,
            side: 1,  // SELL
            signature_type: sig_type,
            signature: String::new(),
            neg_risk,
        }
    }
}

/// Generate a unique salt for order deduplication on the CLOB.
///
/// Uses seconds-since-epoch * 10^6 + random UUID fragment.
/// Kept within JSON safe integer range (2^53) to avoid precision loss
/// in JavaScript-based frontends and API consumers.
fn rand_salt() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as u128;
    // Keep within JSON safe integer range (2^53) by using seconds * 10^6 + random
    let random_part = (uuid::Uuid::new_v4().as_u128() % 1_000_000) as u128;
    secs * 1_000_000 + random_part
}

/// The full payload submitted to POST /order on the CLOB API.
///
/// Wraps the signed EIP-712 order with authentication metadata.
/// The `owner` field is the API key (not the wallet address).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClobOrderPayload {
    /// The signed EIP-712 order struct
    pub order: PolymarketOrder,
    /// API key that owns this order (used for authentication)
    pub owner: String,
    /// Order execution type: "FOK", "GTC", or "IOC"
    pub order_type: String,
}

/// High-level order request used by the pipeline.
///
/// This is the pipeline's abstraction over order details. The executor translates
/// it into a `PolymarketOrder` (with EIP-712 fields, signatures, etc.) internally.
/// The pipeline never constructs `PolymarketOrder` directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClobOrderRequest {
    pub token_id: String,
    pub price: f64,
    pub size_usd: f64,
    pub order_type: ClobOrderType,
    pub fee_rate_bps: u64,
    #[serde(default)]
    pub side: ClobSide,
    #[serde(default)]
    pub neg_risk: bool,
}

/// Response from the CLOB API after order submission.
///
/// For GTC orders, `status` indicates whether the order was immediately matched
/// ("MATCHED") or placed on the book ("LIVE"). The pipeline uses this to decide
/// whether to create a position immediately or track as pending GTC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClobOrderResponse {
    pub success: bool,
    #[serde(alias = "orderID")]
    pub order_id: Option<String>,
    pub status: Option<String>,
    #[serde(alias = "transactHash")]
    pub transact_hash: Option<String>,
    #[serde(alias = "averagePrice")]
    pub price: Option<f64>,
    #[serde(alias = "errorMsg")]
    pub error_msg: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum OrderStatus {
    Pending,
    Submitted,
    PartialFill,
    Filled,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderState {
    pub order_id: String,
    pub market_id: String,
    pub side: ClobSide,
    pub price: f64,
    pub size: f64,
    pub filled_size: f64,
    pub status: OrderStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub tx_hash: Option<String>,
    pub error: Option<String>,
}

/// Status of a GTC order on the CLOB
#[derive(Debug, Clone)]
pub enum GtcOrderStatus {
    /// Order is still on the book, not yet filled
    Open,
    /// Order was filled (fully or partially)
    Filled { avg_price: f64, size_matched: f64 },
    /// Order was cancelled
    Cancelled,
    /// Could not determine status
    Unknown,
}

/// Thread-safe order tracker for managing concurrent order lifecycle.
///
/// Enforces a maximum number of active (Pending/Submitted/PartialFill) orders
/// to prevent over-committing capital on the CLOB. Not currently used by the
/// pipeline (which uses `pending_gtc_orders` directly), but available for
/// future use by the market-making pipeline.
pub struct OrderManager {
    /// All tracked orders (active + completed)
    orders: std::sync::RwLock<Vec<OrderState>>,
    /// Maximum number of concurrent active orders
    max_active: usize,
}

impl OrderManager {
    pub fn new(max_active: usize) -> Self {
        Self { orders: std::sync::RwLock::new(Vec::new()), max_active }
    }

    pub fn can_submit(&self) -> bool {
        let orders = self.orders.read().unwrap();
        let active = orders.iter().filter(|o| matches!(o.status, OrderStatus::Pending | OrderStatus::Submitted | OrderStatus::PartialFill)).count();
        active < self.max_active
    }

    pub fn track_order(&self, state: OrderState) {
        self.orders.write().unwrap().push(state);
    }

    pub fn update_status(&self, order_id: &str, status: OrderStatus) {
        let mut orders = self.orders.write().unwrap();
        if let Some(order) = orders.iter_mut().find(|o| o.order_id == order_id) {
            order.status = status;
            order.updated_at = Utc::now();
        }
    }

    pub fn active_orders(&self) -> Vec<OrderState> {
        self.orders.read().unwrap().iter()
            .filter(|o| matches!(o.status, OrderStatus::Pending | OrderStatus::Submitted | OrderStatus::PartialFill))
            .cloned()
            .collect()
    }

    pub fn cleanup_completed(&self) {
        let mut orders = self.orders.write().unwrap();
        orders.retain(|o| !matches!(o.status, OrderStatus::Filled | OrderStatus::Cancelled | OrderStatus::Failed));
    }
}

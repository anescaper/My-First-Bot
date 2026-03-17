//! Live CLOB HTTP client for submitting signed orders to Polymarket.
//!
//! Handles the full order submission flow:
//! 1. Build EIP-712 order struct (PolymarketOrder)
//! 2. Sign with ECDSA via the OrderSigner trait
//! 3. Compute HMAC for API authentication
//! 4. Submit via POST /order with Polymarket auth headers
//! 5. Parse response (handle both structured and generic JSON)
//!
//! Also supports: order cancellation (DELETE /order/{id}), balance queries
//! (GET /balance-allowance), and GTC order status checks (GET /order/{id}).
//!
//! Authentication uses 5 custom headers: POLY_ADDRESS, POLY_SIGNATURE,
//! POLY_TIMESTAMP, POLY_NONCE, POLY_API_KEY, POLY_PASSPHRASE.

use anyhow::Result;
use async_trait::async_trait;
use crate::executor::Executor;
use crate::signer::OrderSigner;
use crate::order::{ClobOrderRequest, ClobOrderResponse, ClobOrderPayload, PolymarketOrder, GtcOrderStatus};

/// Default Polymarket CLOB API URL (production).
/// Can be overridden via POLYMARKET_CLOB_URL env var for testing/staging.
const CLOB_API: &str = "https://clob.polymarket.com";

/// Live executor that submits signed orders to the Polymarket CLOB API.
///
/// Supports two wallet modes:
/// - **EOA mode** (default): signer and maker are the same address (signatureType=0)
/// - **Proxy mode**: signer signs on behalf of a Polymarket proxy wallet (signatureType=2)
///   Set via POLYMARKET_FUNDER_ADDRESS env var.
pub struct ClobExecutor {
    /// Reusable HTTP client with 10-second timeout
    client: reqwest::Client,
    /// Base URL for CLOB API requests
    clob_url: String,
    /// Polymarket API key (used in POLY_API_KEY header and as `owner` in order payload)
    api_key: String,
    /// Polymarket passphrase (used in POLY_PASSPHRASE header)
    passphrase: String,
    /// ECDSA signer for EIP-712 order signing and HMAC API authentication
    signer: Box<dyn OrderSigner>,
    /// Address that holds USDC and conditional tokens on Polymarket.
    /// In EOA mode, this equals signer.address(). In proxy mode, this is the
    /// Polymarket-generated proxy wallet address (different from signer).
    funder_address: String,
}

impl ClobExecutor {
    /// Create a new ClobExecutor with the given credentials.
    ///
    /// The `_api_secret` parameter is not stored because HMAC signing is delegated
    /// to the OrderSigner (which already has the secret from its initialization).
    /// If `funder_address` is None, uses the signer's address (EOA mode).
    pub fn new(signer: Box<dyn OrderSigner>, api_key: String, _api_secret: String, passphrase: String, funder_address: Option<String>) -> Self {
        let funder = funder_address.unwrap_or_else(|| signer.address());
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            clob_url: CLOB_API.to_string(),
            api_key,
            passphrase,
            funder_address: funder,
            signer,
        }
    }

    /// Override the CLOB API URL (for testing against staging environments).
    pub fn with_url(mut self, url: String) -> Self {
        self.clob_url = url;
        self
    }

    /// Convenience alias for `execute_order` — used by external callers.
    pub async fn submit_order(&self, request: &ClobOrderRequest) -> Result<ClobOrderResponse> {
        self.execute_order(request).await
    }
}

#[async_trait]
impl Executor for ClobExecutor {
    async fn execute_order(&self, request: &ClobOrderRequest) -> Result<ClobOrderResponse> {
        // 1. Build the EIP-712 order struct (BUY or SELL)
        // maker = funder (proxy wallet that holds USDC)
        // signer = API key signer (signs on behalf of funder)
        let mut order = match request.side {
            crate::order::ClobSide::Buy => PolymarketOrder::new_buy(
                request.token_id.clone(),
                request.price,
                request.size_usd,
                &self.funder_address,
                &self.signer.address(),
                request.fee_rate_bps,
                request.neg_risk,
            ),
            crate::order::ClobSide::Sell => PolymarketOrder::new_sell(
                request.token_id.clone(),
                request.price,
                request.size_usd, // For sell: this is shares (token count), not USD
                &self.funder_address,
                &self.signer.address(),
                request.fee_rate_bps,
                request.neg_risk,
            ),
        };

        // 2. Sign the order with EIP-712
        let signature = self.signer.sign_order_eip712(&order);
        order.signature = format!("0x{}", hex::encode(&signature));

        // 3. Build the full payload
        let order_type_str = match request.order_type {
            crate::order::ClobOrderType::Fok => "FOK",
            crate::order::ClobOrderType::Gtc => "GTC",
            crate::order::ClobOrderType::Ioc => "IOC",
        };
        let payload = ClobOrderPayload {
            order,
            owner: self.api_key.clone(),
            order_type: order_type_str.to_string(),
        };

        let body = serde_json::to_string(&payload)?;
        tracing::info!(
            maker = %payload.order.maker,
            signer = %payload.order.signer,
            sig_type = payload.order.signature_type,
            neg_risk = payload.order.neg_risk,
            price = request.price,
            size = request.size_usd,
            "CLOB order: maker={} signer={} sigType={} negRisk={}",
            payload.order.maker, payload.order.signer,
            payload.order.signature_type, payload.order.neg_risk,
        );
        let timestamp = chrono::Utc::now().timestamp().to_string();
        let nonce = uuid::Uuid::new_v4().to_string();

        // 4. Compute HMAC for API auth
        let hmac_sig = self.signer.hmac_signature(&timestamp, "POST", "/order", &body);

        // 5. Submit to CLOB API
        let resp = self.client.post(format!("{}/order", self.clob_url))
            .header("POLY_ADDRESS", self.signer.address())
            .header("POLY_SIGNATURE", &hmac_sig)
            .header("POLY_TIMESTAMP", &timestamp)
            .header("POLY_NONCE", &nonce)
            .header("POLY_API_KEY", &self.api_key)
            .header("POLY_PASSPHRASE", &self.passphrase)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await?;

        let status = resp.status();
        let resp_text = resp.text().await?;

        if !status.is_success() {
            tracing::error!(status = %status, body = %resp_text, "CLOB order rejected");
            return Ok(ClobOrderResponse {
                success: false,
                order_id: None,
                status: Some(status.to_string()),
                transact_hash: None,
                price: None,
                error_msg: Some(resp_text),
            });
        }

        tracing::info!(resp = %resp_text, "CLOB order response body");
        match serde_json::from_str::<ClobOrderResponse>(&resp_text) {
            Ok(r) => Ok(r),
            Err(_) => {
                // Try to extract order_id from generic JSON
                let v: serde_json::Value = serde_json::from_str(&resp_text)?;
                // Check for error in 200 response body
                let error_msg = v.get("errorMsg")
                    .or(v.get("error_msg"))
                    .or(v.get("error"))
                    .and_then(|e| e.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());
                let success = error_msg.is_none();
                let fill_price = v.get("averagePrice")
                    .or(v.get("average_price"))
                    .and_then(|p| p.as_f64().or_else(|| p.as_str().and_then(|s| s.parse().ok())))
                    .unwrap_or(request.price);
                Ok(ClobOrderResponse {
                    success,
                    order_id: v.get("orderID").or(v.get("order_id")).and_then(|v| v.as_str()).map(|s| s.to_string()),
                    status: Some(if success { "submitted" } else { "error" }.into()),
                    transact_hash: v.get("transactHash").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    price: if success { Some(fill_price) } else { None },
                    error_msg,
                })
            }
        }
    }

    async fn cancel_order(&self, order_id: &str) -> Result<bool> {
        let timestamp = chrono::Utc::now().timestamp().to_string();
        let nonce = uuid::Uuid::new_v4().to_string();
        let path = format!("/order/{}", order_id);
        let hmac_sig = self.signer.hmac_signature(&timestamp, "DELETE", &path, "");

        let resp = self.client.delete(format!("{}{}", self.clob_url, path))
            .header("POLY_ADDRESS", self.signer.address())
            .header("POLY_SIGNATURE", &hmac_sig)
            .header("POLY_TIMESTAMP", &timestamp)
            .header("POLY_NONCE", &nonce)
            .header("POLY_API_KEY", &self.api_key)
            .header("POLY_PASSPHRASE", &self.passphrase)
            .send()
            .await?;

        Ok(resp.status().is_success())
    }

    fn name(&self) -> &str {
        "clob-live"
    }

    fn is_live(&self) -> bool {
        true
    }

    async fn get_balance(&self) -> Result<f64> {
        let timestamp = chrono::Utc::now().timestamp().to_string();
        let nonce = uuid::Uuid::new_v4().to_string();
        // HMAC signs path WITHOUT query string; full URL includes it
        let hmac_path = "/balance-allowance";
        let full_path = "/balance-allowance?asset_type=COLLATERAL";
        let hmac_sig = self.signer.hmac_signature(&timestamp, "GET", hmac_path, "");

        let resp = self.client.get(format!("{}{}", self.clob_url, full_path))
            .header("POLY_ADDRESS", self.signer.address())
            .header("POLY_SIGNATURE", &hmac_sig)
            .header("POLY_TIMESTAMP", &timestamp)
            .header("POLY_NONCE", &nonce)
            .header("POLY_API_KEY", &self.api_key)
            .header("POLY_PASSPHRASE", &self.passphrase)
            .send()
            .await?;

        let text = resp.text().await?;
        let v: serde_json::Value = serde_json::from_str(&text)?;
        let balance_raw = v.get("balance")
            .and_then(|b| b.as_str())
            .unwrap_or("0");
        let balance_units: f64 = balance_raw.parse().unwrap_or(0.0);
        let balance = balance_units / 1_000_000.0;

        // Check USDC allowance — if not approved to CTF Exchange, orders will fail on-chain
        let allowance_raw = v.get("allowance")
            .and_then(|a| a.as_str())
            .unwrap_or("0");
        let allowance_units: f64 = allowance_raw.parse().unwrap_or(0.0);
        let allowance = allowance_units / 1_000_000.0;

        if allowance < balance {
            tracing::warn!(
                balance = balance,
                allowance = allowance,
                "USDC allowance ({:.2}) is less than balance ({:.2}) — approve CTF Exchange contract before live trading",
                allowance, balance,
            );
        }

        // Return the minimum of balance and allowance — can't trade more than approved
        Ok(balance.min(allowance))
    }

    async fn get_order_status(&self, order_id: &str) -> Result<GtcOrderStatus> {
        let timestamp = chrono::Utc::now().timestamp().to_string();
        let nonce = uuid::Uuid::new_v4().to_string();
        let path = format!("/order/{}", order_id);
        let hmac_sig = self.signer.hmac_signature(&timestamp, "GET", &path, "");

        let resp = self.client.get(format!("{}{}", self.clob_url, path))
            .header("POLY_ADDRESS", self.signer.address())
            .header("POLY_SIGNATURE", &hmac_sig)
            .header("POLY_TIMESTAMP", &timestamp)
            .header("POLY_NONCE", &nonce)
            .header("POLY_API_KEY", &self.api_key)
            .header("POLY_PASSPHRASE", &self.passphrase)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Ok(GtcOrderStatus::Unknown);
        }

        let v: serde_json::Value = resp.json().await?;
        let status = v.get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("");

        match status {
            "MATCHED" => {
                let avg_price = v.get("associate_trades")
                    .and_then(|t| t.as_array())
                    .and_then(|trades| {
                        if trades.is_empty() { return None; }
                        let mut total_size = 0.0f64;
                        let mut total_value = 0.0f64;
                        for t in trades {
                            let price = t.get("price")
                                .and_then(|p| p.as_f64().or_else(|| p.as_str().and_then(|s| s.parse().ok())))
                                .unwrap_or(0.0);
                            let size = t.get("size")
                                .and_then(|s| s.as_f64().or_else(|| s.as_str().and_then(|v| v.parse().ok())))
                                .unwrap_or(0.0);
                            total_size += size;
                            total_value += price * size;
                        }
                        if total_size > 0.0 { Some(total_value / total_size) } else { None }
                    })
                    .or_else(|| {
                        v.get("price")
                            .and_then(|p| p.as_f64().or_else(|| p.as_str().and_then(|s| s.parse().ok())))
                    })
                    .unwrap_or(0.0);
                let size_matched = v.get("size_matched")
                    .and_then(|s| s.as_f64().or_else(|| s.as_str().and_then(|v| v.parse().ok())))
                    .or_else(|| v.get("original_size")
                        .and_then(|s| s.as_f64().or_else(|| s.as_str().and_then(|v| v.parse().ok()))))
                    .unwrap_or(0.0);
                Ok(GtcOrderStatus::Filled { avg_price, size_matched })
            }
            "LIVE" => Ok(GtcOrderStatus::Open),
            "CANCELLED" => Ok(GtcOrderStatus::Cancelled),
            _ => Ok(GtcOrderStatus::Unknown),
        }
    }
}

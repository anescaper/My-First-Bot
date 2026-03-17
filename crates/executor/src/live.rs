//! Factory functions for creating executors based on bot mode and environment.
//!
//! The live executor requires three Polymarket API credentials:
//! - `POLYMARKET_API_KEY`: identifies the API user
//! - `POLYMARKET_API_SECRET`: used for HMAC request signing
//! - `POLYMARKET_PASSPHRASE`: additional auth factor
//!
//! Plus the private key for EIP-712 order signing:
//! - `POLYMARKET_PRIVATE_KEY`: secp256k1 private key (hex, with or without 0x prefix)
//!
//! Optional:
//! - `POLYMARKET_FUNDER_ADDRESS`: proxy wallet address (for Polymarket proxy wallet setups)
//! - `POLYMARKET_CLOB_URL`: override CLOB API URL (default: https://clob.polymarket.com)
//!
//! All secrets support Docker secrets fallback: if the env var is not set,
//! reads from `/run/secrets/{lowercase_key}`.

use anyhow::Result;
use polybot_api::state::BotMode;
use crate::executor::Executor;
use crate::clob::ClobExecutor;
use crate::paper::PaperExecutor;


/// Read a secret from env var first, then Docker secret file as fallback.
///
/// Docker secrets are mounted at `/run/secrets/` in the container.
/// This allows the same code to work in local dev (env vars) and
/// Docker production (secrets mounted as files).
fn read_secret(env_key: &str) -> Result<String> {
    if let Ok(val) = std::env::var(env_key) {
        return Ok(val);
    }
    let file_path = format!("/run/secrets/{}", env_key.to_lowercase());
    std::fs::read_to_string(&file_path)
        .map(|s| s.trim().to_string())
        .map_err(|_| anyhow::anyhow!("{} not set and {} not found", env_key, file_path))
}

/// Create a live executor from environment variables or Docker secrets.
///
/// This function:
/// 1. Reads API credentials (key, secret, passphrase) from env/secrets
/// 2. Creates a LiveSigner from POLYMARKET_PRIVATE_KEY (requires `live-signing` feature)
/// 3. REFUSES to fall back to DevSigner in live mode (safety: would submit invalid signatures)
/// 4. Optionally reads POLYMARKET_FUNDER_ADDRESS for proxy wallet mode
/// 5. Returns a ClobExecutor configured with the signer and credentials
///
/// Fails hard if `live-signing` feature is not compiled or if credentials are missing.
pub fn create_live_executor() -> Result<Box<dyn Executor>> {
    let api_key = read_secret("POLYMARKET_API_KEY")?;
    let api_secret = read_secret("POLYMARKET_API_SECRET")?;
    let passphrase = read_secret("POLYMARKET_PASSPHRASE")?;

    let signer: Box<dyn crate::signer::OrderSigner> = {
        #[cfg(feature = "live-signing")]
        {
            match crate::signer::LiveSigner::from_env() {
                Ok(s) => {
                    tracing::info!("Live executor using LiveSigner (EIP-712 ECDSA)");
                    Box::new(s)
                }
                Err(e) => {
                    tracing::error!("LiveSigner failed: {} — REFUSING to fall back to DevSigner in live mode", e);
                    return Err(anyhow::anyhow!("LiveSigner required for live trading: {}", e));
                }
            }
        }
        #[cfg(not(feature = "live-signing"))]
        {
            tracing::error!("live-signing feature not compiled — cannot create live executor");
            return Err(anyhow::anyhow!("live-signing feature not enabled at compile time"));
        }
    };

    let clob_url = std::env::var("POLYMARKET_CLOB_URL")
        .unwrap_or_else(|_| "https://clob.polymarket.com".to_string());

    // Optional funder address for proxy wallet setups.
    // Default: None → EOA mode (signer = maker, signatureType=0)
    // Set POLYMARKET_FUNDER_ADDRESS only if using a Polymarket proxy contract wallet.
    let funder_address = read_secret("POLYMARKET_FUNDER_ADDRESS").ok();
    if let Some(ref addr) = funder_address {
        tracing::info!(funder = %addr, "Using POLY_PROXY mode: funder/proxy wallet as maker, signer as delegated signer");
    } else {
        tracing::info!("Using EOA mode: signer address is both maker and signer");
    }

    let mut executor = ClobExecutor::new(signer, api_key, api_secret, passphrase, funder_address);
    if clob_url != "https://clob.polymarket.com" {
        executor = executor.with_url(clob_url);
    }

    tracing::info!("Live executor created successfully");
    Ok(Box::new(executor))
}

/// Create executor based on bot mode.
/// For Live mode: MUST succeed or returns error (no silent fallback).
/// For Paper/Stopped: returns PaperExecutor.
pub fn create_executor(mode: BotMode) -> Result<Box<dyn Executor>> {
    match mode {
        BotMode::Live => create_live_executor(),
        _ => Ok(Box::new(PaperExecutor::default())),
    }
}

/// Re-create executor for a new mode. Called when mode changes via API.
pub fn recreate_executor(mode: BotMode) -> Result<Box<dyn Executor>> {
    match mode {
        BotMode::Live => create_live_executor(),
        _ => Ok(Box::new(PaperExecutor::default())),
    }
}

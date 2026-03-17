//! Market scanner crate — discovers and filters Polymarket trading opportunities.
//!
//! This crate provides two scanning modes:
//!
//! 1. **Crypto scanner** (`crypto` module): Discovers active crypto Up/Down binary
//!    rounds for BTC/ETH/SOL/XRP across 5m/15m/1h timeframes. This is the primary
//!    scanner used by the trading pipeline.
//!
//! 2. **General CLOB scanner** (`clob` module): Discovers general Yes/No prediction
//!    markets sorted by liquidity. Used for market overview and monitoring.
//!
//! Supporting modules:
//! - `filter`: Configurable market filtering (liquidity, spread, time-to-resolution).
//! - `price_feed`: Multi-source price feeds (Binance + Pyth) for underlying asset prices.
//! - `types`: Shared data types for markets and order books.

pub mod clob;
pub mod crypto;
pub mod filter;
pub mod price_feed;
pub mod types;

// Re-export key types at crate root for ergonomic imports.
pub use types::{Market, MarketSnapshot};
pub use crypto::{Asset, Timeframe, CryptoRound, CryptoScanner};
pub use price_feed::PriceFeedManager;

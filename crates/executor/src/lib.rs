//! Executor crate — handles order creation, signing, and submission to Polymarket CLOB.
//!
//! This crate contains everything needed to go from a trading signal to an on-chain order:
//!
//! - `order` — EIP-712 order structs, CLOB request/response types, order manager
//! - `signer` — ECDSA signing (DevSigner for paper, LiveSigner for real trading)
//! - `executor` — trait defining the executor interface (execute, cancel, balance, status)
//! - `clob` — live CLOB HTTP client that submits signed orders with HMAC authentication
//! - `paper` — simulated executor with configurable fill rate and slippage
//! - `cost` — fee models including Polymarket's crypto fee formula
//! - `live` — factory functions for creating executors from environment/secrets

pub mod signer;
pub mod clob;
pub mod order;
pub mod cost;
pub mod paper;
pub mod executor;
pub mod live;

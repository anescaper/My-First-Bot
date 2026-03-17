//! Risk engine crate — position sizing, exposure management, circuit breakers,
//! and composable risk gate chain.
//!
//! This crate provides two risk evaluation systems:
//!
//! ## 1. Composable Gate Chain (primary, for live trading pipeline)
//!
//! - `gate`: The `RiskGate` trait, `GateResult` enum, and `GateSignal` input type.
//! - `gates`: Concrete gate implementations (MinEdge, MaxExposure, Kelly, Volatility, etc.).
//! - `chain`: `RiskGateChain` — runs gates in priority order, accumulates resizes.
//! - `build_chain`: Factory functions to build profile-specific gate chains.
//!
//! ## 2. Legacy Risk Engine (for backtesting and API)
//!
//! - `checks`: `RiskEngine` — stateful sequential risk checks.
//! - `types`: `RiskConfig`, `PositionRequest`, `RiskVerdict`.
//! - `kelly`: Position sizing (flat conviction-based, not true Kelly).
//!
//! ## Design Philosophy
//!
//! Risk gates are stateless and composable — they receive all context via `GateSignal`
//! and return a `GateResult`. This makes them easy to test, reorder, and combine
//! into different profiles without shared mutable state.

pub mod kelly;
pub mod checks;
pub mod types;
pub mod gate;
pub mod chain;
pub mod gates;
pub mod build_chain;

// Re-export key types at crate root for ergonomic imports.
pub use types::{RiskConfig, RiskVerdict, PositionRequest};
pub use gate::{GateResult, GateSignal, RiskGate};
pub use chain::RiskGateChain;
pub use gates::*;
pub use build_chain::{build_risk_chain, build_risk_chain_with_config, RiskChainConfig};

pub mod traits;
pub mod state;
pub mod hub;
pub mod watchdog;
pub mod candle_builder;
pub mod adapters;
pub mod client;

pub use traits::{TickAdapter, RestAdapter, SourceType};
pub use state::DataState;
pub use hub::{DataHub, DataHealthReport, AdapterHealth};
pub use state::{MicroCandle, FuturesState, OptionsState, TokenTick, OrderBookSnapshot, PolyFill, RoundKey};
pub use client::{DataClient, CycleData, IntelSnapshot, OptionsSnapshot, ResolvedRoundSnapshot};
pub use adapters::binance_spot_ws::BinanceSpotWsAdapter;
pub use adapters::binance_futures_ws::BinanceFuturesWsAdapter;
pub use adapters::polymarket_clob_ws::PolymarketClobWsAdapter;
pub use adapters::binance_futures_rest::BinanceFuturesRestAdapter;
pub use adapters::coinbase_rest::CoinbaseRestAdapter;
pub use adapters::deribit_ws::DeribitWsAdapter;

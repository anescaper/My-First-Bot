//! Central data manager. Registers adapters, spawns tasks, monitors health.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;

use crate::state::DataState;
use crate::traits::{RestAdapter, TickAdapter};
use crate::watchdog::DataWatchdog;

/// Health status of a single adapter.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AdapterHealth {
    pub name: String,
    pub healthy: bool,
    pub last_data_secs_ago: Option<f64>,
    pub reconnect_count: u32,
}

/// Aggregated health report for all adapters.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DataHealthReport {
    pub adapters: Vec<AdapterHealth>,
    pub overall_healthy: bool,
}

/// Central data manager. Owns adapters, runs them in parallel, monitors health.
pub struct DataHub {
    state: Arc<DataState>,
    pending_tick: Vec<Box<dyn TickAdapter>>,
    pending_rest: Vec<Box<dyn RestAdapter>>,
    tick_handles: Vec<JoinHandle<()>>,
    rest_handles: Vec<JoinHandle<()>>,
    watchdog: DataWatchdog,
}

impl DataHub {
    pub fn new() -> Self {
        Self {
            state: Arc::new(DataState::new()),
            pending_tick: Vec::new(),
            pending_rest: Vec::new(),
            tick_handles: Vec::new(),
            rest_handles: Vec::new(),
            watchdog: DataWatchdog::new(),
        }
    }

    /// Add a tick (WebSocket) adapter. Builder pattern.
    pub fn add_tick(mut self, adapter: impl TickAdapter) -> Self {
        self.pending_tick.push(Box::new(adapter));
        self
    }

    /// Add a REST polling adapter. Builder pattern.
    pub fn add_rest(mut self, adapter: impl RestAdapter) -> Self {
        self.pending_rest.push(Box::new(adapter));
        self
    }

    /// Get shared state handle for consumers (Pipeline, API).
    pub fn state(&self) -> Arc<DataState> {
        self.state.clone()
    }

    /// Health summary for API /health endpoint.
    pub fn health(&self) -> DataHealthReport {
        let adapters = self.watchdog.report();
        let overall_healthy = adapters.iter().all(|a| a.healthy);
        DataHealthReport {
            adapters,
            overall_healthy,
        }
    }

    /// Start all adapters + watchdog + GC task.
    pub async fn start(&mut self) -> anyhow::Result<()> {
        let tick_count = self.pending_tick.len();
        let rest_count = self.pending_rest.len();
        tracing::info!("DataHub starting with {tick_count} tick adapters, {rest_count} rest adapters");

        // Start tick adapters
        for mut adapter in self.pending_tick.drain(..) {
            let state = self.state.clone();
            let name = adapter.name().to_string();
            let timeout = adapter.inactivity_timeout();
            let last_data = adapter.last_data_atomic();
            let last_data_for_loop = last_data.clone();

            self.watchdog.register(name.clone(), timeout, last_data);

            let handle = tokio::spawn(async move {
                loop {
                    if let Err(e) = adapter.connect().await {
                        tracing::error!(adapter = adapter.name(), "Connect failed: {e}");
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        continue;
                    }
                    tracing::info!(adapter = adapter.name(), "Connected");

                    loop {
                        match adapter.poll_next(&state).await {
                            Ok(_) => {
                                // Check inactivity: if no real data for > timeout, force reconnect
                                let last = last_data_for_loop.load(Ordering::Relaxed);
                                if last > 0 {
                                    let elapsed = chrono::Utc::now().timestamp() - last;
                                    if elapsed > timeout.as_secs() as i64 {
                                        tracing::warn!(
                                            adapter = adapter.name(),
                                            elapsed_secs = elapsed,
                                            "Inactivity timeout, forcing reconnect"
                                        );
                                        break;
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!(adapter = adapter.name(), "poll_next error: {e}");
                                break; // Break inner loop to reconnect
                            }
                        }
                    }

                    adapter.disconnect().await;
                    tracing::warn!(adapter = %name, "Disconnected, reconnecting in 5s");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            });
            self.tick_handles.push(handle);
        }

        // Start REST adapters
        for adapter in self.pending_rest.drain(..) {
            let state = self.state.clone();
            let interval = adapter.poll_interval();
            let name = adapter.name().to_string();
            let timeout = Duration::from_secs(interval.as_secs() * 3); // 3x poll interval
            let last_data = adapter.last_data_atomic();

            self.watchdog.register(name.clone(), timeout, last_data);

            let handle = tokio::spawn(async move {
                loop {
                    if let Err(e) = adapter.fetch(&state).await {
                        tracing::warn!(adapter = %name, "Fetch error: {e}");
                    }
                    tokio::time::sleep(interval).await;
                }
            });
            self.rest_handles.push(handle);
        }

        // Spawn watchdog check loop
        let watchdog = self.watchdog.clone();
        tokio::spawn(async move {
            loop {
                let stale = watchdog.check();
                for name in &stale {
                    tracing::warn!(adapter = %name, "Data inactivity detected");
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        });

        // Spawn GC task — evict data older than 5 minutes
        let gc_state = self.state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                gc_state.gc(chrono::Duration::seconds(300));
            }
        });

        tracing::info!("DataHub started");
        Ok(())
    }
}

impl Default for DataHub {
    fn default() -> Self {
        Self::new()
    }
}

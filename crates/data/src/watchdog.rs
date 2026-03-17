use chrono::Utc;
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::hub::AdapterHealth;

/// Tracks health of a single adapter.
struct WatchdogEntry {
    name: String,
    last_data: Arc<AtomicI64>,
    inactivity_timeout: Duration,
    reconnect_count: Arc<AtomicU32>,
    max_reconnects: u32,
}

/// Monitors all registered adapters for data inactivity.
#[derive(Clone)]
pub struct DataWatchdog {
    entries: Arc<std::sync::RwLock<Vec<WatchdogEntry>>>,
}

impl DataWatchdog {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(std::sync::RwLock::new(Vec::new())),
        }
    }

    /// Register an adapter to be monitored.
    pub fn register(
        &self,
        name: String,
        inactivity_timeout: Duration,
        last_data: Arc<AtomicI64>,
    ) -> Arc<AtomicU32> {
        let reconnect_count = Arc::new(AtomicU32::new(0));
        let entry = WatchdogEntry {
            name,
            last_data,
            inactivity_timeout,
            reconnect_count: reconnect_count.clone(),
            max_reconnects: 100,
        };
        self.entries.write().unwrap().push(entry);
        reconnect_count
    }

    /// Check all adapters, return names of those needing reconnect.
    pub fn check(&self) -> Vec<String> {
        let now = Utc::now().timestamp();
        let entries = self.entries.read().unwrap();
        entries
            .iter()
            .filter(|e| {
                let last = e.last_data.load(Ordering::Relaxed);
                if last == 0 {
                    return false; // Never received data yet, not stale
                }
                let elapsed = now - last;
                elapsed > e.inactivity_timeout.as_secs() as i64
                    && e.reconnect_count.load(Ordering::Relaxed) < e.max_reconnects
            })
            .map(|e| {
                e.reconnect_count.fetch_add(1, Ordering::Relaxed);
                e.name.clone()
            })
            .collect()
    }

    /// Generate health report for all adapters.
    pub fn report(&self) -> Vec<AdapterHealth> {
        let now = Utc::now().timestamp();
        let entries = self.entries.read().unwrap();
        entries
            .iter()
            .map(|e| {
                let last = e.last_data.load(Ordering::Relaxed);
                let secs_ago = if last > 0 {
                    Some((now - last) as f64)
                } else {
                    None
                };
                let healthy = match secs_ago {
                    Some(s) => s < e.inactivity_timeout.as_secs() as f64,
                    None => false, // No data yet
                };
                AdapterHealth {
                    name: e.name.clone(),
                    healthy,
                    last_data_secs_ago: secs_ago,
                    reconnect_count: e.reconnect_count.load(Ordering::Relaxed),
                }
            })
            .collect()
    }
}

use chrono::{DateTime, Duration, Utc};

use crate::state::MicroCandle;

/// Aggregates raw trade ticks into fixed-interval OHLCV micro-candles.
pub struct CandleBuilder {
    interval: Duration,
    current: Option<CandleAccumulator>,
}

struct CandleAccumulator {
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
    trade_count: u32,
    open_time: DateTime<Utc>,
    close_time: DateTime<Utc>,
}

impl CandleBuilder {
    /// Create a builder with the given candle interval (e.g., 5 seconds).
    pub fn new(interval_secs: i64) -> Self {
        Self {
            interval: Duration::seconds(interval_secs),
            current: None,
        }
    }

    /// Feed a trade tick. Returns a completed candle if the interval boundary was crossed.
    pub fn feed(&mut self, price: f64, volume: f64, timestamp: DateTime<Utc>) -> Option<MicroCandle> {
        let mut completed = None;

        if let Some(ref acc) = self.current {
            if timestamp >= acc.close_time {
                // Interval boundary crossed — emit completed candle
                completed = Some(MicroCandle {
                    open: acc.open,
                    high: acc.high,
                    low: acc.low,
                    close: acc.close,
                    volume: acc.volume,
                    trade_count: acc.trade_count,
                    open_time: acc.open_time,
                    close_time: acc.close_time,
                });
                self.current = None;
            }
        }

        match self.current.as_mut() {
            Some(acc) => {
                acc.high = acc.high.max(price);
                acc.low = acc.low.min(price);
                acc.close = price;
                acc.volume += volume;
                acc.trade_count += 1;
            }
            None => {
                let bucket_start = self.bucket_start(timestamp);
                self.current = Some(CandleAccumulator {
                    open: price,
                    high: price,
                    low: price,
                    close: price,
                    volume,
                    trade_count: 1,
                    open_time: bucket_start,
                    close_time: bucket_start + self.interval,
                });
            }
        }

        completed
    }

    /// Align timestamp to interval boundary.
    fn bucket_start(&self, ts: DateTime<Utc>) -> DateTime<Utc> {
        let secs = ts.timestamp();
        let interval_secs = self.interval.num_seconds();
        let bucket = secs - (secs % interval_secs);
        DateTime::from_timestamp(bucket, 0).unwrap_or(ts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_candle_builder_produces_candle_on_boundary() {
        let mut builder = CandleBuilder::new(5);
        // Use a timestamp aligned to a 5s boundary
        let base = DateTime::from_timestamp(1700000000, 0).unwrap(); // aligned to 5s

        // Feed trades within first 5s bucket [0, 5)
        assert!(builder.feed(100.0, 1.0, base).is_none());
        assert!(builder.feed(102.0, 2.0, base + Duration::seconds(2)).is_none());
        assert!(builder.feed(99.0, 1.5, base + Duration::seconds(4)).is_none());

        // Cross boundary at +5s — should emit candle for [0, 5)
        let candle = builder.feed(101.0, 0.5, base + Duration::seconds(6));
        assert!(candle.is_some());

        let c = candle.unwrap();
        assert_eq!(c.open, 100.0);
        assert_eq!(c.high, 102.0);
        assert_eq!(c.low, 99.0);
        assert_eq!(c.close, 99.0);
        assert_eq!(c.trade_count, 3);
        assert!((c.volume - 4.5).abs() < 1e-10);
    }
}

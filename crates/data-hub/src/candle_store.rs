use std::collections::{HashMap, VecDeque};

use polybot_scanner::crypto::Asset;
use polybot_scanner::price_feed::Candle;

/// Max retention per timeframe
const MAX_1M: usize = 120;  // 2 hours
const MAX_5M: usize = 60;   // 5 hours
const MAX_15M: usize = 40;  // 10 hours
const MAX_1H: usize = 24;   // 24 hours

pub struct CandleStore {
    pub candles_1m: HashMap<Asset, VecDeque<Candle>>,
    pub candles_5m: HashMap<Asset, VecDeque<Candle>>,
    pub candles_15m: HashMap<Asset, VecDeque<Candle>>,
    pub candles_1h: HashMap<Asset, VecDeque<Candle>>,
}

impl CandleStore {
    pub fn new() -> Self {
        let mut candles_1m = HashMap::new();
        let mut candles_5m = HashMap::new();
        let mut candles_15m = HashMap::new();
        let mut candles_1h = HashMap::new();

        for asset in Asset::ALL {
            candles_1m.insert(asset, VecDeque::new());
            candles_5m.insert(asset, VecDeque::new());
            candles_15m.insert(asset, VecDeque::new());
            candles_1h.insert(asset, VecDeque::new());
        }

        Self { candles_1m, candles_5m, candles_15m, candles_1h }
    }

    /// Add a 1m candle and trigger higher-TF aggregation if on a boundary.
    pub fn ingest_1m(&mut self, asset: Asset, candle: Candle) {
        let deque = self.candles_1m.entry(asset).or_default();
        deque.push_back(candle);
        while deque.len() > MAX_1M {
            deque.pop_front();
        }

        // Try aggregating 1m -> 5m (every 5 candles aligned to :00, :05, :10, ...)
        self.try_aggregate_up(asset, 5, MAX_5M);
        // Try aggregating 5m -> 15m (every 3x 5m candles aligned to :00, :15, :30, :45)
        self.try_aggregate_5m_to_15m(asset);
        // Try aggregating 15m -> 1h (every 4x 15m candles aligned to hour boundary)
        self.try_aggregate_15m_to_1h(asset);
    }

    /// Bulk load 1m candles from REST bootstrap. Triggers full re-aggregation.
    pub fn backfill(&mut self, asset: Asset, candles_1m: Vec<Candle>) {
        let deque = self.candles_1m.entry(asset).or_default();
        for c in candles_1m {
            deque.push_back(c);
        }
        while deque.len() > MAX_1M {
            deque.pop_front();
        }

        // Rebuild higher timeframes from scratch
        self.rebuild_5m(asset);
        self.rebuild_15m(asset);
        self.rebuild_1h(asset);
    }

    /// Return candles for the requested interval.
    pub fn get_candles(&self, asset: Asset, interval: &str) -> Vec<Candle> {
        let deque = match interval {
            "1m" => self.candles_1m.get(&asset),
            "5m" => self.candles_5m.get(&asset),
            "15m" => self.candles_15m.get(&asset),
            "1h" => self.candles_1h.get(&asset),
            _ => None,
        };
        deque.map(|d| d.iter().cloned().collect()).unwrap_or_default()
    }

    /// Aggregate N consecutive candles into one higher-TF candle.
    fn aggregate(source: &[Candle]) -> Option<Candle> {
        if source.is_empty() {
            return None;
        }
        let first = &source[0];
        let last = &source[source.len() - 1];
        Some(Candle {
            open: first.open,
            high: source.iter().fold(f64::NEG_INFINITY, |acc, c| acc.max(c.high)),
            low: source.iter().fold(f64::INFINITY, |acc, c| acc.min(c.low)),
            close: last.close,
            volume: source.iter().map(|c| c.volume).sum(),
            open_time: first.open_time,
            close_time: last.close_time,
        })
    }

    /// Try to aggregate the last N 1m candles into a 5m candle if aligned.
    fn try_aggregate_up(&mut self, asset: Asset, factor: usize, max: usize) {
        let src = match self.candles_1m.get(&asset) {
            Some(d) if d.len() >= factor => d,
            _ => return,
        };

        // Check if the latest candle's close_time aligns to a 5m boundary
        // Binance close_time is ms timestamp at the end of the minute
        let last = &src[src.len() - 1];
        let close_sec = last.close_time / 1000;
        // 5m boundary: close_time should be at :04:59, :09:59, :14:59, etc.
        // The close_time of the 5th minute candle (e.g. 12:04) marks the 5m boundary
        let minute_of_close = (close_sec / 60) % 60;
        // A 5m candle ends when (minute+1) is divisible by 5
        // e.g. close_time at 12:04:59.999 means minute=4, (4+1)%5==0 => boundary
        if (minute_of_close + 1) % (factor as i64) != 0 {
            return;
        }

        // Grab the last `factor` candles
        let start = src.len() - factor;
        let slice: Vec<Candle> = src.iter().skip(start).cloned().collect();

        if let Some(agg) = Self::aggregate(&slice) {
            let dst = self.candles_5m.entry(asset).or_default();
            // Avoid duplicate: check if we already have this candle
            if dst.back().map_or(true, |last| last.close_time < agg.close_time) {
                dst.push_back(agg);
                while dst.len() > max {
                    dst.pop_front();
                }
            }
        }
    }

    /// Aggregate 5m -> 15m (every 3 consecutive 5m candles aligned to :00, :15, :30, :45)
    fn try_aggregate_5m_to_15m(&mut self, asset: Asset) {
        let src = match self.candles_5m.get(&asset) {
            Some(d) if d.len() >= 3 => d,
            _ => return,
        };

        let last = &src[src.len() - 1];
        let close_sec = last.close_time / 1000;
        let minute_of_close = (close_sec / 60) % 60;
        // 15m boundary: close at :14, :29, :44, :59 => (minute+1) % 15 == 0
        if (minute_of_close + 1) % 15 != 0 {
            return;
        }

        let start = src.len() - 3;
        let slice: Vec<Candle> = src.iter().skip(start).cloned().collect();

        if let Some(agg) = Self::aggregate(&slice) {
            let dst = self.candles_15m.entry(asset).or_default();
            if dst.back().map_or(true, |last| last.close_time < agg.close_time) {
                dst.push_back(agg);
                while dst.len() > MAX_15M {
                    dst.pop_front();
                }
            }
        }
    }

    /// Aggregate 15m -> 1h (every 4 consecutive 15m candles aligned to hour boundary)
    fn try_aggregate_15m_to_1h(&mut self, asset: Asset) {
        let src = match self.candles_15m.get(&asset) {
            Some(d) if d.len() >= 4 => d,
            _ => return,
        };

        let last = &src[src.len() - 1];
        let close_sec = last.close_time / 1000;
        let minute_of_close = (close_sec / 60) % 60;
        // 1h boundary: close at :59 => (minute+1) % 60 == 0
        if (minute_of_close + 1) % 60 != 0 {
            return;
        }

        let start = src.len() - 4;
        let slice: Vec<Candle> = src.iter().skip(start).cloned().collect();

        if let Some(agg) = Self::aggregate(&slice) {
            let dst = self.candles_1h.entry(asset).or_default();
            if dst.back().map_or(true, |last| last.close_time < agg.close_time) {
                dst.push_back(agg);
                while dst.len() > MAX_1H {
                    dst.pop_front();
                }
            }
        }
    }

    /// Rebuild all 5m candles from 1m data
    fn rebuild_5m(&mut self, asset: Asset) {
        let src = match self.candles_1m.get(&asset) {
            Some(d) => d.iter().cloned().collect::<Vec<_>>(),
            None => return,
        };
        let dst = self.candles_5m.entry(asset).or_default();
        dst.clear();

        // Group by 5m boundary
        let mut i = 0;
        while i < src.len() {
            let open_min = (src[i].open_time / 1000 / 60) % 60;
            let boundary_start = open_min - (open_min % 5);
            let expected_close_min = boundary_start + 4;

            let mut group = vec![src[i].clone()];
            let mut j = i + 1;
            while j < src.len() && group.len() < 5 {
                let m = (src[j].open_time / 1000 / 60) % 60;
                if m <= expected_close_min && m >= boundary_start {
                    group.push(src[j].clone());
                    j += 1;
                } else {
                    break;
                }
            }

            if group.len() == 5 {
                if let Some(agg) = Self::aggregate(&group) {
                    dst.push_back(agg);
                }
            }
            i = j;
        }

        while dst.len() > MAX_5M {
            dst.pop_front();
        }
    }

    /// Rebuild all 15m candles from 5m data
    fn rebuild_15m(&mut self, asset: Asset) {
        let src = match self.candles_5m.get(&asset) {
            Some(d) => d.iter().cloned().collect::<Vec<_>>(),
            None => return,
        };
        let dst = self.candles_15m.entry(asset).or_default();
        dst.clear();

        let mut i = 0;
        while i + 3 <= src.len() {
            let group: Vec<Candle> = src[i..i + 3].to_vec();
            if let Some(agg) = Self::aggregate(&group) {
                dst.push_back(agg);
            }
            i += 3;
        }

        while dst.len() > MAX_15M {
            dst.pop_front();
        }
    }

    /// Rebuild all 1h candles from 15m data
    fn rebuild_1h(&mut self, asset: Asset) {
        let src = match self.candles_15m.get(&asset) {
            Some(d) => d.iter().cloned().collect::<Vec<_>>(),
            None => return,
        };
        let dst = self.candles_1h.entry(asset).or_default();
        dst.clear();

        let mut i = 0;
        while i + 4 <= src.len() {
            let group: Vec<Candle> = src[i..i + 4].to_vec();
            if let Some(agg) = Self::aggregate(&group) {
                dst.push_back(agg);
            }
            i += 4;
        }

        while dst.len() > MAX_1H {
            dst.pop_front();
        }
    }
}

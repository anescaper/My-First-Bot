/// Compute Hurst exponent from a price series via R/S analysis.
///
/// Returns:
/// - 0.0 - 0.5: mean-reverting
/// - 0.5: random walk
/// - 0.5 - 1.0: trending
///
/// Requires at least 21 prices (20 returns). Returns 0.5 if insufficient data.
pub fn hurst_exponent(prices: &[f64]) -> f64 {
    if prices.len() < 2 {
        return 0.5;
    }

    // Convert prices to log-returns
    let returns: Vec<f64> = prices
        .windows(2)
        .map(|w| (w[1] / w[0]).ln())
        .filter(|r| r.is_finite())
        .collect();

    if returns.len() < 20 {
        return 0.5;
    }

    let n = returns.len();

    // Generate chunk sizes: start at 8, multiply by 1.5
    let mut chunk_sizes = Vec::new();
    let mut size = 8.0_f64;
    while (size as usize) <= n / 2 {
        chunk_sizes.push(size as usize);
        size *= 1.5;
    }

    if chunk_sizes.is_empty() {
        return 0.5;
    }

    let mut log_sizes = Vec::new();
    let mut log_rs = Vec::new();

    for &chunk_size in &chunk_sizes {
        let n_blocks = n / chunk_size;
        if n_blocks == 0 {
            continue;
        }

        let mut rs_values = Vec::new();

        for b in 0..n_blocks {
            let block = &returns[b * chunk_size..(b + 1) * chunk_size];
            let block_mean = block.iter().sum::<f64>() / chunk_size as f64;

            // Standard deviation
            let var = block.iter().map(|r| (r - block_mean).powi(2)).sum::<f64>()
                / (chunk_size - 1) as f64;
            let std_dev = var.sqrt();

            if std_dev < 1e-20 {
                continue;
            }

            // Cumulative deviation from mean
            let mut cum_dev = Vec::with_capacity(chunk_size);
            let mut running = 0.0;
            for &r in block {
                running += r - block_mean;
                cum_dev.push(running);
            }

            let max_dev = cum_dev.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            let min_dev = cum_dev.iter().cloned().fold(f64::INFINITY, f64::min);

            let rs = (max_dev - min_dev) / std_dev;
            rs_values.push(rs);
        }

        if rs_values.is_empty() {
            continue;
        }

        let avg_rs = rs_values.iter().sum::<f64>() / rs_values.len() as f64;
        if avg_rs > 0.0 {
            log_sizes.push((chunk_size as f64).ln());
            log_rs.push(avg_rs.ln());
        }
    }

    if log_sizes.len() < 2 {
        return 0.5;
    }

    // OLS regression: log(R/S) = H * log(n) + c
    let n_pts = log_sizes.len() as f64;
    let mean_x = log_sizes.iter().sum::<f64>() / n_pts;
    let mean_y = log_rs.iter().sum::<f64>() / n_pts;

    let mut ss_xy = 0.0;
    let mut ss_xx = 0.0;
    for i in 0..log_sizes.len() {
        let dx = log_sizes[i] - mean_x;
        let dy = log_rs[i] - mean_y;
        ss_xy += dx * dy;
        ss_xx += dx * dx;
    }

    if ss_xx.abs() < 1e-20 {
        return 0.5;
    }

    let hurst = ss_xy / ss_xx;
    hurst.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_insufficient_data() {
        assert!((hurst_exponent(&[1.0, 2.0, 3.0]) - 0.5).abs() < 1e-10);
    }

    #[test]
    fn test_trending_series() {
        // Pure uptrend should give H > 0.5
        let prices: Vec<f64> = (0..200).map(|i| 100.0 + i as f64 * 0.5).collect();
        let h = hurst_exponent(&prices);
        assert!(h > 0.5, "trending series should have H > 0.5, got {}", h);
    }

    #[test]
    fn test_mean_reverting_series() {
        // Alternating series should give H < 0.5
        let prices: Vec<f64> = (0..200)
            .map(|i| 100.0 + if i % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        let h = hurst_exponent(&prices);
        assert!(h < 0.5, "mean-reverting series should have H < 0.5, got {}", h);
    }

    #[test]
    fn test_result_in_range() {
        let prices: Vec<f64> = (0..100)
            .map(|i| 100.0 * (1.0 + 0.01 * (i as f64 * 0.3).sin()))
            .collect();
        let h = hurst_exponent(&prices);
        assert!(h >= 0.0 && h <= 1.0, "H should be in [0,1], got {}", h);
    }
}

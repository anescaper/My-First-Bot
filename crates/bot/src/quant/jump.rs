/// Detect jumps in a returns series using Barndorff-Nielsen bipower variation.
///
/// RV (realized variance) = sum(r_i^2)
/// BPV (bipower variation) = (pi/2) * sum(|r_t| * |r_{t-1}|)
/// jump_var = max(0, RV - BPV)
/// A return is flagged as a jump if |r_i| > threshold * sqrt(jump_var / n)
///
/// Returns indices of detected jumps.
pub fn detect_jumps(returns: &[f64], threshold: f64) -> Vec<usize> {
    if returns.len() < 3 {
        return Vec::new();
    }

    let n = returns.len() as f64;

    // Realized variance
    let rv: f64 = returns.iter().map(|r| r * r).sum();

    // Bipower variation with finite-sample correction: n/(n-1)
    let bpv: f64 = (std::f64::consts::PI / 2.0)
        * (n / (n - 1.0))
        * returns
            .windows(2)
            .map(|w| w[0].abs() * w[1].abs())
            .sum::<f64>();

    let jump_var = (rv - bpv).max(0.0);

    if jump_var < 1e-20 {
        return Vec::new();
    }

    let jump_threshold = threshold * (jump_var / n).sqrt();

    returns
        .iter()
        .enumerate()
        .filter(|(_, r)| r.abs() > jump_threshold)
        .map(|(i, _)| i)
        .collect()
}

/// Returns true if any jump detected in the series.
pub fn has_jumps(returns: &[f64], threshold: f64) -> bool {
    !detect_jumps(returns, threshold).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_jumps_in_calm_series() {
        let returns: Vec<f64> = (0..50).map(|i| 0.001 * (i as f64 * 0.5).sin()).collect();
        assert!(!has_jumps(&returns, 3.0));
    }

    #[test]
    fn test_detects_spike() {
        let mut returns: Vec<f64> = vec![0.001; 50];
        returns[25] = 0.15; // big spike
        let jumps = detect_jumps(&returns, 2.0);
        assert!(!jumps.is_empty(), "should detect the spike as a jump");
        assert!(jumps.contains(&25));
    }

    #[test]
    fn test_short_series() {
        assert!(detect_jumps(&[0.01, -0.02], 3.0).is_empty());
    }

    #[test]
    fn test_higher_threshold_fewer_jumps() {
        let mut returns: Vec<f64> = vec![0.001; 50];
        returns[10] = 0.05;
        returns[30] = 0.10;
        let jumps_low = detect_jumps(&returns, 1.5);
        let jumps_high = detect_jumps(&returns, 5.0);
        assert!(
            jumps_high.len() <= jumps_low.len(),
            "higher threshold should find fewer or equal jumps"
        );
    }
}

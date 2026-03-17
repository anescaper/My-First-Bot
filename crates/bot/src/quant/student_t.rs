use std::f64::consts::PI;

/// Lanczos approximation of ln(Gamma(x))
fn ln_gamma(x: f64) -> f64 {
    const C: [f64; 9] = [
        0.99999999999980993,
        676.5203681218851,
        -1259.1392167224028,
        771.32342877765313,
        -176.61502916214059,
        12.507343278686905,
        -0.13857109526572012,
        9.9843695780195716e-6,
        1.5056327351493116e-7,
    ];
    const G: f64 = 7.0;

    if x < 0.5 {
        let s = PI / (PI * x).sin();
        return s.ln() - ln_gamma(1.0 - x);
    }

    let x = x - 1.0;
    let mut t = C[0];
    for (i, &ci) in C[1..].iter().enumerate() {
        t += ci / (x + i as f64 + 1.0);
    }
    let w = x + G + 0.5;
    0.5 * (2.0 * PI).ln() + (x + 0.5) * w.ln() - w + t.ln()
}

/// Regularized incomplete beta function I_x(a, b) using continued fraction (Lentz's method)
fn regularized_incomplete_beta(x: f64, a: f64, b: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }

    // Use symmetry relation for better convergence
    if x > (a + 1.0) / (a + b + 2.0) {
        return 1.0 - regularized_incomplete_beta(1.0 - x, b, a);
    }

    // ln of the prefix: x^a * (1-x)^b / (a * B(a,b))
    let ln_prefix =
        a * x.ln() + b * (1.0 - x).ln() - (ln_gamma(a) + ln_gamma(b) - ln_gamma(a + b))
            - a.ln();
    let prefix = ln_prefix.exp();

    const TINY: f64 = 1e-30;

    // Lentz's continued fraction method
    let mut c = 1.0;
    let mut d = 1.0 - (a + b) * x / (a + 1.0);
    if d.abs() < TINY {
        d = TINY;
    }
    d = 1.0 / d;
    let mut f = d;

    for m in 1..200u64 {
        let m_f = m as f64;

        // Even numerator: a_{2m} = m(b-m)x / ((a+2m-1)(a+2m))
        let num = m_f * (b - m_f) * x / ((a + 2.0 * m_f - 1.0) * (a + 2.0 * m_f));
        d = 1.0 + num * d;
        if d.abs() < TINY {
            d = TINY;
        }
        c = 1.0 + num / c;
        if c.abs() < TINY {
            c = TINY;
        }
        d = 1.0 / d;
        f *= c * d;

        // Odd numerator: a_{2m+1} = -(a+m)(a+b+m)x / ((a+2m)(a+2m+1))
        let num = -(a + m_f) * (a + b + m_f) * x / ((a + 2.0 * m_f) * (a + 2.0 * m_f + 1.0));
        d = 1.0 + num * d;
        if d.abs() < TINY {
            d = TINY;
        }
        c = 1.0 + num / c;
        if c.abs() < TINY {
            c = TINY;
        }
        d = 1.0 / d;
        let delta = c * d;
        f *= delta;

        if (delta - 1.0).abs() < 1e-10 {
            break;
        }
    }

    prefix * f
}

/// Compute CDF of Student-t distribution at x with given degrees of freedom.
///
/// Uses the regularized incomplete beta function.
pub fn student_t_cdf(x: f64, df: f64) -> f64 {
    if df <= 0.0 {
        return 0.5;
    }

    let t = df / (df + x * x);
    let ib = regularized_incomplete_beta(t, df / 2.0, 0.5);

    if x >= 0.0 {
        1.0 - 0.5 * ib
    } else {
        0.5 * ib
    }
}

/// Fit degrees of freedom to a returns series using method of moments.
///
/// For Student-t: excess kurtosis = 6 / (df - 4) for df > 4,
/// so df = 4 + 6 / kurtosis.
///
/// Returns estimated df clamped to [2.1, 30.0].
/// If kurtosis <= 0 (platykurtic or normal), returns 30.0.
pub fn estimate_df(returns: &[f64]) -> f64 {
    if returns.len() < 4 {
        return 30.0;
    }

    let n = returns.len() as f64;
    let mean = returns.iter().sum::<f64>() / n;

    let m2 = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / n;
    let m4 = returns.iter().map(|r| (r - mean).powi(4)).sum::<f64>() / n;

    if m2 < 1e-20 {
        return 30.0;
    }

    let kurtosis = m4 / (m2 * m2) - 3.0; // excess kurtosis

    if kurtosis <= 0.0 {
        return 30.0;
    }

    let df = 4.0 + 6.0 / kurtosis;
    df.clamp(2.1, 30.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cdf_symmetry() {
        let df = 5.0;
        let cdf_pos = student_t_cdf(1.0, df);
        let cdf_neg = student_t_cdf(-1.0, df);
        assert!(
            (cdf_pos + cdf_neg - 1.0).abs() < 1e-6,
            "CDF should be symmetric: P(X<=1) + P(X<=-1) = 1, got {} + {} = {}",
            cdf_pos,
            cdf_neg,
            cdf_pos + cdf_neg
        );
    }

    #[test]
    fn test_cdf_at_zero() {
        assert!(
            (student_t_cdf(0.0, 5.0) - 0.5).abs() < 1e-6,
            "CDF at 0 should be 0.5"
        );
    }

    #[test]
    fn test_cdf_monotone() {
        let df = 10.0;
        let vals: Vec<f64> = (-30..=30).map(|i| student_t_cdf(i as f64 * 0.1, df)).collect();
        for w in vals.windows(2) {
            assert!(
                w[1] >= w[0] - 1e-10,
                "CDF should be non-decreasing"
            );
        }
    }

    #[test]
    fn test_cdf_known_values() {
        // t-distribution with df=1 is Cauchy: CDF(0) = 0.5, CDF(1) = 0.75
        let cdf = student_t_cdf(1.0, 1.0);
        assert!(
            (cdf - 0.75).abs() < 0.01,
            "Cauchy CDF(1) should be ~0.75, got {}",
            cdf
        );
    }

    #[test]
    fn test_cdf_tails() {
        let df = 5.0;
        assert!(student_t_cdf(-10.0, df) < 0.01);
        assert!(student_t_cdf(10.0, df) > 0.99);
    }

    #[test]
    fn test_estimate_df_normal() {
        // Near-normal returns should give high df
        let returns: Vec<f64> = (0..500).map(|i| 0.01 * (i as f64 * 0.1).sin()).collect();
        let df = estimate_df(&returns);
        assert!(df >= 2.1 && df <= 30.0);
    }

    #[test]
    fn test_estimate_df_heavy_tails() {
        // Returns with occasional large values -> lower df
        let mut returns: Vec<f64> = vec![0.001; 100];
        returns[10] = 0.10;
        returns[50] = -0.12;
        returns[80] = 0.08;
        let df = estimate_df(&returns);
        assert!(df < 30.0, "heavy-tailed returns should give df < 30, got {}", df);
    }

    #[test]
    fn test_estimate_df_short_series() {
        assert!((estimate_df(&[0.01, -0.02]) - 30.0).abs() < 1e-10);
    }
}

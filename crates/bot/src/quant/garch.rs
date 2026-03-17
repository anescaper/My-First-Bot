/// GARCH(1,1) volatility model
///
/// Fit via grid search over (alpha, beta) with variance targeting for omega.
/// Log-likelihood: sum(-0.5 * (ln(var) + r^2 / var))
pub struct Garch11 {
    omega: f64,
    alpha: f64,
    beta: f64,
    variance: f64,
    last_return: f64,
}

impl Garch11 {
    /// Fit GARCH(1,1) to a returns series using grid search + MLE
    pub fn fit(returns: &[f64]) -> Self {
        if returns.len() < 3 {
            return Self {
                omega: 0.0001,
                alpha: 0.1,
                beta: 0.85,
                variance: 0.0001,
                last_return: 0.0,
            };
        }

        let n = returns.len() as f64;
        let mean = returns.iter().sum::<f64>() / n;
        let sample_var = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0);

        let mut best_ll = f64::NEG_INFINITY;
        let mut best_alpha = 0.1;
        let mut best_beta = 0.85;

        // Grid search: alpha in [0.015, 0.300], beta in [0.025, 1.000]
        let mut alpha_i = 1u32;
        while alpha_i < 20 {
            let alpha = alpha_i as f64 * 0.015;
            let mut beta_j = 1u32;
            while beta_j < 40 {
                let beta = beta_j as f64 * 0.025;

                if alpha + beta >= 0.999 {
                    beta_j += 1;
                    continue;
                }

                let omega = sample_var * (1.0 - alpha - beta);
                if omega <= 0.0 {
                    beta_j += 1;
                    continue;
                }

                // Compute log-likelihood
                let mut var = sample_var;
                let mut ll = 0.0;
                for &r in returns {
                    if var < 1e-20 {
                        var = 1e-20;
                    }
                    ll += -0.5 * (var.ln() + r * r / var);
                    var = omega + alpha * r * r + beta * var;
                }

                if ll > best_ll {
                    best_ll = ll;
                    best_alpha = alpha;
                    best_beta = beta;
                }

                beta_j += 1;
            }
            alpha_i += 1;
        }

        let omega = sample_var * (1.0 - best_alpha - best_beta);

        // Warm up state by running all returns through update
        let mut model = Self {
            omega,
            alpha: best_alpha,
            beta: best_beta,
            variance: sample_var,
            last_return: 0.0,
        };

        for &r in returns {
            model.update(r);
        }

        model
    }

    /// Update with new return observation
    pub fn update(&mut self, ret: f64) {
        self.variance = self.omega + self.alpha * ret * ret + self.beta * self.variance;
        if self.variance < 1e-20 {
            self.variance = 1e-20;
        }
        self.last_return = ret;
    }

    /// Current volatility estimate
    pub fn volatility(&self) -> f64 {
        self.variance.sqrt()
    }

    /// Forecast n-step ahead volatilities (not variances)
    pub fn forecast_vol(&self, n_steps: usize) -> Vec<f64> {
        let long_run_var = self.omega / (1.0 - self.alpha - self.beta);
        let persistence = self.alpha + self.beta;

        let mut vols = Vec::with_capacity(n_steps);
        let mut var_t = self.variance;

        for _ in 0..n_steps {
            var_t = self.omega + persistence * var_t;
            // Equivalent to: long_run_var + persistence^k * (var_t - long_run_var)
            // but iterative is simpler
            vols.push(var_t.max(1e-20).sqrt());
        }

        // The iterative forecast converges to long_run_var
        let _ = long_run_var;

        vols
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fit_short_series() {
        let model = Garch11::fit(&[0.01, -0.02]);
        assert!((model.alpha - 0.1).abs() < 1e-10);
        assert!((model.beta - 0.85).abs() < 1e-10);
    }

    #[test]
    fn test_fit_and_forecast() {
        let returns: Vec<f64> = (0..100)
            .map(|i| ((i as f64 * 0.7).sin()) * 0.02)
            .collect();

        let model = Garch11::fit(&returns);
        assert!(model.volatility() > 0.0);

        let vols = model.forecast_vol(5);
        assert_eq!(vols.len(), 5);
        for v in &vols {
            assert!(*v > 0.0);
        }
    }

    #[test]
    fn test_update() {
        let mut model = Garch11::fit(&[0.01, -0.02, 0.03, -0.01, 0.005]);
        let v1 = model.volatility();
        model.update(0.05); // big move
        let v2 = model.volatility();
        assert!(v2 > v1, "volatility should increase after large return");
    }
}

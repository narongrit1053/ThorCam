//! One-dimensional Gaussian peak fit with an offset term (`Profile_Fit.vi`).
//!
//! Model: `y = a * exp(-(x - mu)^2 / (2 sigma^2)) + offset`, fitted by Levenberg-Marquardt least squares.
//! Width definitions: 1/e² radius `w = 2 sigma`, 1/e² diameter `D = 4 sigma`, `FWHM = 2.355 sigma`.

/// Result of one Gaussian fit. All positions are in sensor pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GaussFit {
    pub amplitude: f64,
    pub center: f64,
    pub sigma: f64,
    pub offset: f64,
    /// Mean squared residual, like the `residue` output of LabVIEW's Gaussian Peak Fit.
    pub residue: f64,
}

impl GaussFit {
    pub fn eval(&self, x: f64) -> f64 {
        let u = (x - self.center) / self.sigma;
        self.amplitude * (-0.5 * u * u).exp() + self.offset
    }

    /// 1/e² diameter, `4 sigma`.
    pub fn d4sigma(&self) -> f64 {
        4.0 * self.sigma
    }

    pub fn fwhm(&self) -> f64 {
        2.354_820_045 * self.sigma
    }
}

/// Initial guess as in the blueprint: amplitude = max - min, center = x0 + index of max,
/// sigma = (points above half maximum) / 2.355, offset = min.
pub fn initial_guess(x0: f64, y: &[f64]) -> Option<[f64; 4]> {
    if y.len() < 4 {
        return None;
    }
    let (mut imax, mut ymax, mut ymin) = (0, f64::NEG_INFINITY, f64::INFINITY);
    for (i, &v) in y.iter().enumerate() {
        if !v.is_finite() {
            return None;
        }
        if v > ymax {
            ymax = v;
            imax = i;
        }
        ymin = ymin.min(v);
    }
    let a = ymax - ymin;
    if a <= 0.0 {
        return None;
    }
    let half = ymin + 0.5 * a;
    let above = y.iter().filter(|&&v| v > half).count();
    let sigma = (above as f64 / 2.355).max(0.5);
    Some([a, x0 + imax as f64, sigma, ymin])
}

/// Fits a Gaussian with offset to `y`, sampled at `x = x0, x0 + 1, ...`.
/// Returns `None` for a flat or too-short profile.
pub fn fit_gaussian(x0: f64, y: &[f64]) -> Option<GaussFit> {
    let mut p = initial_guess(x0, y)?;
    let n = y.len();

    let chi2 = |p: &[f64; 4]| -> f64 {
        let [a, mu, s, c] = *p;
        let k = -0.5 / (s * s);
        y.iter()
            .enumerate()
            .map(|(i, &yi)| {
                let dx = x0 + i as f64 - mu;
                let r = yi - (a * (k * dx * dx).exp() + c);
                r * r
            })
            .sum()
    };

    let mut cur = chi2(&p);
    let mut lambda = 1e-3;
    'outer: for _ in 0..200 {
        let [a, mu, s, _] = p;
        let mut jtj = [[0.0f64; 4]; 4];
        let mut jtr = [0.0f64; 4];
        for (i, &yi) in y.iter().enumerate() {
            let dx = x0 + i as f64 - mu;
            let e = (-0.5 * dx * dx / (s * s)).exp();
            let r = yi - (a * e + p[3]);
            let j = [e, a * e * dx / (s * s), a * e * dx * dx / (s * s * s), 1.0];
            for k in 0..4 {
                jtr[k] += j[k] * r;
                for l in 0..=k {
                    jtj[k][l] += j[k] * j[l];
                }
            }
        }
        for k in 0..4 {
            for l in 0..k {
                jtj[l][k] = jtj[k][l];
            }
        }

        loop {
            let mut m = jtj;
            for k in 0..4 {
                m[k][k] += lambda * jtj[k][k].max(1e-300);
            }
            if let Some(d) = solve4(m, jtr) {
                let trial = [p[0] + d[0], p[1] + d[1], p[2] + d[2], p[3] + d[3]];
                let t = chi2(&trial);
                if t.is_finite() && t <= cur && trial[2].abs() > 1e-6 {
                    let gain = (cur - t) / cur.max(f64::MIN_POSITIVE);
                    p = trial;
                    cur = t;
                    lambda = (lambda * 0.1).max(1e-12);
                    if gain < 1e-12 || cur == 0.0 {
                        break 'outer;
                    }
                    break;
                }
            }
            lambda *= 10.0;
            if lambda > 1e12 {
                break 'outer;
            }
        }
    }

    Some(GaussFit {
        amplitude: p[0],
        center: p[1],
        sigma: p[2].abs(),
        offset: p[3],
        residue: cur / n as f64,
    })
}

/// Second-moment width: subtract `offset`, clip negatives to 0, then `sqrt(sum((x - mean)^2 p) / sum(p))`.
/// It should agree with the fitted sigma within a few percent on a clean Gaussian beam.
pub fn second_moment_sigma(x0: f64, y: &[f64], offset: f64) -> f64 {
    let (mut s0, mut s1) = (0.0, 0.0);
    for (i, &v) in y.iter().enumerate() {
        let p = (v - offset).max(0.0);
        s0 += p;
        s1 += p * (x0 + i as f64);
    }
    if s0 <= 0.0 {
        return f64::NAN;
    }
    let mean = s1 / s0;
    let s2: f64 = y
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let d = x0 + i as f64 - mean;
            (v - offset).max(0.0) * d * d
        })
        .sum();
    (s2 / s0).sqrt()
}

/// Solves the 4 x 4 system `m d = b` by Gaussian elimination with partial pivoting.
fn solve4(mut m: [[f64; 4]; 4], mut b: [f64; 4]) -> Option<[f64; 4]> {
    for col in 0..4 {
        let piv = (col..4).max_by(|&i, &j| m[i][col].abs().total_cmp(&m[j][col].abs()))?;
        if m[piv][col].abs() < 1e-300 || !m[piv][col].is_finite() {
            return None;
        }
        m.swap(col, piv);
        b.swap(col, piv);
        for row in col + 1..4 {
            let f = m[row][col] / m[col][col];
            for k in col..4 {
                m[row][k] -= f * m[col][k];
            }
            b[row] -= f * b[col];
        }
    }
    let mut x = [0.0; 4];
    for row in (0..4).rev() {
        let s: f64 = (row + 1..4).map(|k| m[row][k] * x[k]).sum();
        x[row] = (b[row] - s) / m[row][row];
    }
    x.iter().all(|v| v.is_finite()).then_some(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gauss(n: usize, x0: f64, a: f64, mu: f64, s: f64, c: f64) -> Vec<f64> {
        (0..n)
            .map(|i| {
                let u = (x0 + i as f64 - mu) / s;
                a * (-0.5 * u * u).exp() + c
            })
            .collect()
    }

    #[test]
    fn recovers_exact_gaussian() {
        let y = gauss(300, 570.0, 5000.0, 723.4, 31.7, 820.0);
        let f = fit_gaussian(570.0, &y).unwrap();
        assert!((f.amplitude - 5000.0).abs() < 1e-6, "{f:?}");
        assert!((f.center - 723.4).abs() < 1e-8, "{f:?}");
        assert!((f.sigma - 31.7).abs() < 1e-8, "{f:?}");
        assert!((f.offset - 820.0).abs() < 1e-6, "{f:?}");
        assert!(f.residue < 1e-12);
    }

    #[test]
    fn recovers_narrow_off_centre_gaussian() {
        let y = gauss(100, 0.0, 300.0, 12.3, 1.8, 5.0);
        let f = fit_gaussian(0.0, &y).unwrap();
        assert!((f.sigma - 1.8).abs() < 1e-6, "{f:?}");
        assert!((f.center - 12.3).abs() < 1e-6, "{f:?}");
    }

    #[test]
    fn flat_profile_gives_none() {
        assert!(fit_gaussian(0.0, &[3.0; 50]).is_none());
        assert!(fit_gaussian(0.0, &[1.0, 2.0]).is_none());
    }

    #[test]
    fn second_moment_matches_sigma() {
        let y = gauss(400, 0.0, 1000.0, 200.0, 25.0, 40.0);
        let s2 = second_moment_sigma(0.0, &y, 40.0);
        assert!((s2 - 25.0).abs() < 0.01, "{s2}");
    }
}

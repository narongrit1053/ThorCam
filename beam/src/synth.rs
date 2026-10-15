//! Synthetic camera frames for tests and the simulator (the `Spots` node of `Test_AutoZoom.vi`).

/// One Gaussian spot: amplitude in counts, centre (column `x0`, row `y0`) and sigmas, in pixels.
#[derive(Clone, Copy, Debug)]
pub struct Spot {
    pub a: f64,
    pub x0: f64,
    pub y0: f64,
    pub sx: f64,
    pub sy: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct SynthOptions {
    /// Black level in counts.
    pub black: f64,
    /// Read noise, standard deviation in counts.
    pub noise_sigma: f64,
    /// Fraction of pixels that are hot (set to `clip`), at new places in every frame.
    pub hot_fraction: f64,
    /// Full scale, e.g. 1023 for a 10-bit sensor.
    pub clip: f64,
}

impl SynthOptions {
    /// The CS165 defaults from the auto-zoom design: black 12, read noise 2.5, 10-bit.
    pub fn cs165() -> Self {
        Self { black: 12.0, noise_sigma: 2.5, hot_fraction: 0.0, clip: 1023.0 }
    }
}

/// Renders `spots` on a `width x height` frame. The same `seed` gives the same frame.
pub fn render(width: usize, height: usize, spots: &[Spot], o: &SynthOptions, seed: u64) -> Vec<u16> {
    // Each spot is separable: a * gx[col] * gy[row].
    let profiles: Vec<(f64, Vec<f64>, Vec<f64>)> = spots
        .iter()
        .map(|s| {
            let g = |n: usize, mu: f64, sig: f64| -> Vec<f64> {
                (0..n).map(|i| (-0.5 * ((i as f64 - mu) / sig).powi(2)).exp()).collect()
            };
            (s.a, g(width, s.x0, s.sx), g(height, s.y0, s.sy))
        })
        .collect();

    let mut rng = Rng::new(seed);
    let mut out = Vec::with_capacity(width * height);
    for r in 0..height {
        for c in 0..width {
            let mut s = o.black + o.noise_sigma * rng.normal();
            for (a, gx, gy) in &profiles {
                s += a * gx[c] * gy[r];
            }
            let v = if o.hot_fraction > 0.0 && rng.uniform() < o.hot_fraction { o.clip } else { s.clamp(0.0, o.clip) };
            out.push(v.round() as u16);
        }
    }
    out
}

/// Small, fast, deterministic random numbers (xorshift64*); good enough for test noise.
#[derive(Clone, Debug)]
pub struct Rng {
    state: u64,
    spare: Option<f64>,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self { state: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1, spare: None }
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    pub fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Standard normal (Box-Muller).
    pub fn normal(&mut self) -> f64 {
        if let Some(z) = self.spare.take() {
            return z;
        }
        let u1 = self.uniform().max(f64::MIN_POSITIVE);
        let u2 = self.uniform();
        let r = (-2.0 * u1.ln()).sqrt();
        let t = std::f64::consts::TAU * u2;
        self.spare = Some(r * t.sin());
        r * t.cos()
    }
}

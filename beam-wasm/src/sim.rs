//! A simulated CS165MU (or, in colour mode, CS165CU), as in `thorcam/src/sim.rs`: 1440 x 1080, 10-bit, one
//! slowly moving Gaussian beam, read noise and a few hot pixels. Exposure and gain scale the beam. The colour model
//! has an RGGB Bayer filter and sees a red laser: red pixels get the full signal, green 15 % and blue 4 %.
//! The caller supplies the time, so the browser drives it from its own clock.

use beam::synth::Rng;

pub const W: usize = 1440;
pub const H: usize = 1080;
const FULL: f32 = 1023.0;
const NOISE_LEN: usize = 1 << 16;
/// Response of the R, G and B filters to the simulated red laser.
const LASER_RGB: [f32; 3] = [1.0, 0.15, 0.04];

pub struct SimCamera {
    rng: Rng,
    noise: Vec<f32>,
    hot: Vec<usize>,
    /// Filter response for [even row, odd row][even col, odd col]; all 1 for mono.
    response: [[f32; 2]; 2],
}

impl SimCamera {
    pub fn new(color: bool) -> Self {
        let mut rng = Rng::new(42);
        let noise = (0..NOISE_LEN).map(|_| 2.5 * rng.normal() as f32).collect();
        let hot = (0..25).map(|_| (rng.uniform() * (W * H) as f64) as usize).collect();
        let [r, g, b] = LASER_RGB;
        let response = if color { [[r, g], [g, b]] } else { [[1.0; 2]; 2] };
        Self { rng, noise, hot, response }
    }

    /// One frame at time `t` seconds. `blocked` removes the beam (for recording a dark frame).
    pub fn render(&mut self, out: &mut [u16], t: f64, exposure_ms: f64, gain_db: f64, black_level: f64, blocked: bool) {
        let tau = std::f64::consts::TAU;
        // Lissajous drift around the centre, and a slow change of size.
        let cx = 720.0 + 160.0 * (tau * t / 17.0).sin();
        let cy = 540.0 + 90.0 * (tau * t / 11.0).sin();
        let breathe = 1.0 + 0.15 * (tau * t / 23.0).sin();
        let (sx, sy) = (24.0 * breathe, 16.0 * breathe);
        let amp = if blocked { 0.0 } else { 600.0 * exposure_ms / 5.0 * 10f64.powf(gain_db / 20.0) };

        let g = |n: usize, mu: f64, sig: f64| -> Vec<f32> { (0..n).map(|i| (-0.5 * ((i as f64 - mu) / sig).powi(2)).exp() as f32).collect() };
        let (gx, gy) = (g(W, cx, sx), g(H, cy, sy));
        let black = black_level as f32;
        for (row, (gyr, line)) in gy.iter().zip(out.chunks_exact_mut(W)).enumerate() {
            let a = amp as f32 * gyr;
            let resp = self.response[row % 2];
            let off = (self.rng.next_u64() as usize) % (NOISE_LEN - W);
            let noise = &self.noise[off..off + W];
            for (c, ((o, &gxc), &n)) in line.iter_mut().zip(&gx).zip(noise).enumerate() {
                *o = (black + 0.5 + a * resp[c % 2] * gxc + n).clamp(0.0, FULL + 0.5) as u16;
            }
        }
        for &i in &self.hot {
            out[i] = FULL as u16;
        }
    }
}

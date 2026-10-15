//! A simulated CS165MU (or, in colour mode, CS165CU): 1440 x 1080, 10-bit, 3.45 µm pixels, one slowly moving
//! Gaussian beam, read noise and a few hot pixels. Exposure and gain scale the beam, so saturation can be tried
//! without hardware. The colour model has an RGGB Bayer filter and sees a red (HeNe-like) laser: the red pixels
//! get the full signal, green 15 % and blue 4 %.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use beam::synth::Rng;
use tlcam::{Applied, CameraInfo, CameraSettings, CfaPhase, ColorInfo, Frame};

use crate::source::CameraSource;

const W: usize = 1440;
const H: usize = 1080;
const FULL: f32 = 1023.0;
const NOISE_LEN: usize = 1 << 16;

/// The simulator's shutter: while set, frames have no beam (for recording a dark frame).
pub static BEAM_BLOCKED: AtomicBool = AtomicBool::new(false);

pub struct SimCamera {
    info: CameraInfo,
    s: Applied,
    t0: Instant,
    next_due: Instant,
    count: u64,
    rng: Rng,
    noise: Vec<f32>,
    hot: Vec<usize>,
    /// Filter response to the laser for [even row, odd row][even col, odd col]; all 1 for mono.
    response: [[f32; 2]; 2],
}

/// Response of the R, G and B filters to the simulated red laser.
const LASER_RGB: [f32; 3] = [1.0, 0.15, 0.04];

impl SimCamera {
    pub fn new(color: bool) -> Self {
        let initial = Applied { exposure_ms: 5.0, gain: 0, gain_db: 0.0, black_level: 12, led_on: false };
        let [r, g, b] = LASER_RGB;
        let color_info = color.then_some(ColorInfo {
            phase: CfaPhase::Red,
            color_correction: tlcam::IDENTITY3,
            // A typical daylight white balance for a CMOS Bayer sensor.
            white_balance: [1.6, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.9],
        });
        let info = CameraInfo {
            model: if color { "Simulated CS165CU" } else { "Simulated CS165MU" }.into(),
            serial: if color { "SIM-0002" } else { "SIM-0001" }.into(),
            firmware: String::new(),
            width: W,
            height: H,
            pixel_w_um: 3.45,
            pixel_h_um: 3.45,
            bin_x: 1,
            bin_y: 1,
            bit_depth: 10,
            monochrome: !color,
            color: color_info,
            exposure_range_us: (40, 26_843_000),
            gain_range: (0, 480),
            black_level_range: (0, 100),
            led_supported: true,
            initial,
        };
        let mut rng = Rng::new(42);
        let noise = (0..NOISE_LEN).map(|_| 2.5 * rng.normal() as f32).collect();
        let hot = (0..25).map(|_| (rng.uniform() * (W * H) as f64) as usize).collect();
        // RGGB: even rows R G R G ..., odd rows G B G B ...
        let response = if color { [[r, g], [g, b]] } else { [[1.0; 2]; 2] };
        Self { info, s: initial, t0: Instant::now(), next_due: Instant::now(), count: 0, rng, noise, hot, response }
    }

    fn render(&mut self) -> Vec<u16> {
        let t = self.t0.elapsed().as_secs_f64();
        let tau = std::f64::consts::TAU;
        // Lissajous drift around the centre, and a slow change of size.
        let cx = 720.0 + 160.0 * (tau * t / 17.0).sin();
        let cy = 540.0 + 90.0 * (tau * t / 11.0).sin();
        let breathe = 1.0 + 0.15 * (tau * t / 23.0).sin();
        let (sx, sy) = (24.0 * breathe, 16.0 * breathe);
        let amp = if BEAM_BLOCKED.load(Ordering::Relaxed) { 0.0 } else { 600.0 * self.s.exposure_ms / 5.0 * 10f64.powf(self.s.gain_db / 20.0) };

        let g = |n: usize, mu: f64, sig: f64| -> Vec<f32> { (0..n).map(|i| (-0.5 * ((i as f64 - mu) / sig).powi(2)).exp() as f32).collect() };
        let (gx, gy) = (g(W, cx, sx), g(H, cy, sy));
        let black = self.s.black_level as f32;

        let mut out = Vec::with_capacity(W * H);
        for (row, gyr) in gy.iter().enumerate() {
            let a = amp as f32 * gyr;
            let resp = self.response[row % 2];
            let off = (self.rng.next_u64() as usize) % (NOISE_LEN - W);
            let noise = &self.noise[off..off + W];
            out.extend(
                gx.iter().zip(noise).enumerate().map(|(c, (&gxc, &n))| (black + 0.5 + a * resp[c % 2] * gxc + n).clamp(0.0, FULL + 0.5) as u16),
            );
        }
        for &i in &self.hot {
            out[i] = FULL as u16;
        }
        out
    }
}

impl CameraSource for SimCamera {
    fn info(&self) -> &CameraInfo {
        &self.info
    }

    fn start(&mut self) -> Result<(), String> {
        self.next_due = Instant::now();
        Ok(())
    }

    fn apply(&mut self, s: &CameraSettings) -> Result<Applied, String> {
        let i = &self.info;
        let exp_us = ((s.exposure_ms * 1000.0).round() as i64).clamp(i.exposure_range_us.0, i.exposure_range_us.1);
        let gain = s.gain.clamp(i.gain_range.0, i.gain_range.1);
        self.s = Applied {
            exposure_ms: exp_us as f64 / 1000.0,
            gain,
            gain_db: gain as f64 / 10.0,
            black_level: s.black_level.map_or(self.s.black_level, |b| b.clamp(i.black_level_range.0, i.black_level_range.1)),
            led_on: s.led_on,
        };
        Ok(self.s)
    }

    fn next_frame(&mut self) -> Result<Option<Frame>, String> {
        let now = Instant::now();
        if now < self.next_due {
            std::thread::sleep((self.next_due - now).min(Duration::from_millis(20)));
            if Instant::now() < self.next_due {
                return Ok(None);
            }
        }
        // At most 30 frames per second, slower for long exposures.
        let period = Duration::from_secs_f64((self.s.exposure_ms / 1000.0).max(1.0 / 30.0));
        self.next_due = (self.next_due + period).max(Instant::now());
        self.count += 1;
        let data = self.render();
        Ok(Some(Frame { width: W, height: H, data, frame_count: self.count }))
    }
}

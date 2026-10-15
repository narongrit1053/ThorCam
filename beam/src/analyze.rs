//! Image in, beam numbers out (`Beam_Analyze.vi`).
//!
//! 1. Peak and its (row, col) on the despeckled image; saturated when peak >= 2^bit depth - 1.
//! 2. Optional dark-frame subtraction.
//! 3. ROI of `2h x 2h` around the peak, clipped at the sensor edge (h = 0: whole image).
//! 4. Y profile = sum of each ROI row, X profile = sum of each ROI column.
//! 5. A Gaussian fit per axis; Dx = 4 sigma_x * pixel width, Dy = 4 sigma_y * pixel height (µm).
//!
//! A frame counts as a beam only when every check of [`BeamParams`] passes (see [`NoBeam`]). Otherwise the fit
//! is dropped and the widths are NaN, so noise never produces numbers.

use crate::fit::{fit_gaussian, second_moment_sigma, GaussFit};
use crate::ImageRef;

#[derive(Clone, Copy, Debug)]
pub struct BeamParams {
    /// Saturation level of the analysed image, e.g. 1023 for a 10-bit sensor (more for a summed colour channel).
    pub full_scale: u32,
    /// Effective pixel width in µm (sensor pixel × horizontal binning).
    pub pixel_w_um: f64,
    /// Effective pixel height in µm (sensor pixel × vertical binning).
    pub pixel_h_um: f64,
    /// ROI half-size in pixels; 0 = whole image.
    pub roi_half: usize,
    /// The despeckled peak must stand this many counts above the background (darkest 32 x 32 corner).
    /// The caller may lower it while a beam is already detected, for hysteresis.
    pub min_contrast: f64,
    /// Smallest signal-to-noise ratio of each axis' fit: fitted amplitude / RMS residual. A fit to pure noise
    /// gives about 3 to 4.
    pub min_snr: f64,
}

impl Default for BeamParams {
    fn default() -> Self {
        Self { full_scale: 1023, pixel_w_um: 1.0, pixel_h_um: 1.0, roi_half: 150, min_contrast: 30.0, min_snr: 8.0 }
    }
}

/// Why a frame was not accepted as a beam.
#[derive(Clone, Debug, PartialEq)]
pub enum NoBeam {
    /// Peak − background below `min_contrast`.
    LowContrast { contrast: f64 },
    /// Even the darkest corner is near full scale: the whole frame is overexposed.
    Overexposed { background: f64, full_scale: f64 },
    /// Low contrast, but the frame is clearly lit everywhere (lamp, room light): no spot to measure.
    EvenlyLit { background: f64, contrast: f64 },
    /// The Gaussian fit failed, or its peak does not stand out of the residual noise (S/N below `min_snr`).
    PoorFit { axis: char, snr: f64 },
    /// The fitted centre lies outside the ROI.
    CenterOutside { axis: char },
    /// The fitted beam is wider than the ROI can measure.
    WiderThanRoi { axis: char },
}

impl std::fmt::Display for NoBeam {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NoBeam::LowContrast { contrast } => write!(f, "too faint: peak only {contrast:.0} counts above background"),
            NoBeam::Overexposed { background, full_scale } => {
                write!(f, "overexposed: the whole frame is near full scale ({background:.0} of {full_scale:.0}); reduce exposure or gain")
            }
            NoBeam::EvenlyLit { background, contrast } => write!(
                f,
                "evenly lit frame, no spot: background {background:.0}, peak only {contrast:.0} above it. A beam must be a spot                  smaller than the frame, with dark corners"
            ),
            NoBeam::PoorFit { axis, snr } => write!(f, "no clear Gaussian peak in {axis} (S/N {snr:.1})"),
            NoBeam::CenterOutside { axis } => write!(f, "{axis} fit centre outside the ROI"),
            NoBeam::WiderThanRoi { axis } => write!(f, "beam wider than the ROI in {axis}; increase ROI half-size"),
        }
    }
}

/// One axis of the result (`Axis.ctl`). Positions are in sensor pixels.
#[derive(Clone, Debug, Default)]
pub struct AxisResult {
    /// Sensor coordinate of `profile[0]`.
    pub x0: usize,
    pub profile: Vec<f64>,
    pub fit: Option<GaussFit>,
    /// The fitted curve at the same points as `profile` (empty without a fit).
    pub fit_curve: Vec<f64>,
    /// Second-moment sigma after subtracting the fitted offset, as a check on the fit. Computed over 3 beam
    /// diameters around the fitted centre (±6 sigma, the ISO 11146 integration window), so hot pixels and
    /// background far from the beam do not inflate it.
    pub sigma2: f64,
    /// Coefficient of determination of the fit (1 = perfect, about 0 for noise). For information only: a
    /// narrow beam in a wide ROI has a low R² even when it is clear.
    pub r2: f64,
    /// Fitted amplitude / RMS residual.
    pub snr: f64,
}

impl AxisResult {
    fn from_profile(x0: usize, profile: Vec<f64>) -> Self {
        let fit = fit_gaussian(x0 as f64, &profile);
        let fit_curve: Vec<f64> = fit
            .map(|f| (0..profile.len()).map(|i| f.eval((x0 + i) as f64)).collect())
            .unwrap_or_default();
        let sigma2 = fit.map_or(f64::NAN, |f| {
            let n = profile.len() as f64;
            let lo = (f.center - 6.0 * f.sigma - x0 as f64).floor().clamp(0.0, n) as usize;
            let hi = (f.center + 6.0 * f.sigma - x0 as f64).ceil().clamp(0.0, n) as usize;
            second_moment_sigma((x0 + lo) as f64, &profile[lo..hi.max(lo)], f.offset)
        });
        let r2 = if fit_curve.is_empty() {
            f64::NAN
        } else {
            let mean = profile.iter().sum::<f64>() / profile.len() as f64;
            let ss_tot: f64 = profile.iter().map(|y| (y - mean).powi(2)).sum();
            let ss_res: f64 = profile.iter().zip(&fit_curve).map(|(y, f)| (y - f).powi(2)).sum();
            if ss_tot > 0.0 { 1.0 - ss_res / ss_tot } else { f64::NAN }
        };
        let snr = fit.map_or(f64::NAN, |f| f.amplitude / f.residue.sqrt().max(f64::MIN_POSITIVE));
        Self { x0, profile, fit, fit_curve, sigma2, r2, snr }
    }

    /// Profile only: no fit was attempted.
    fn profile_only(x0: usize, profile: Vec<f64>) -> Self {
        Self { x0, profile, fit: None, fit_curve: Vec::new(), sigma2: f64::NAN, r2: f64::NAN, snr: f64::NAN }
    }

    fn drop_fit(&mut self) {
        self.fit = None;
        self.fit_curve.clear();
        self.sigma2 = f64::NAN;
    }

    /// Checks that this axis' fit describes a beam inside the ROI.
    fn check(&self, axis: char, min_snr: f64) -> Result<(), NoBeam> {
        let Some(f) = self.fit else { return Err(NoBeam::PoorFit { axis, snr: f64::NAN }) };
        // A sub-half-pixel "beam" is a fit to a noise spike or a hot pixel.
        if !(self.snr >= min_snr) || !(f.amplitude > 0.0) || !(f.sigma >= 0.5) {
            return Err(NoBeam::PoorFit { axis, snr: self.snr });
        }
        let (lo, n) = (self.x0 as f64, self.profile.len() as f64);
        if !(f.center >= lo && f.center <= lo + n) {
            return Err(NoBeam::CenterOutside { axis });
        }
        // The ROI must hold at least ±2 sigma (the 1/e² diameter) to measure the width.
        if !(4.0 * f.sigma <= n) {
            return Err(NoBeam::WiderThanRoi { axis });
        }
        Ok(())
    }

    pub fn sigma(&self) -> f64 {
        self.fit.map_or(f64::NAN, |f| f.sigma)
    }

    pub fn center(&self) -> f64 {
        self.fit.map_or(f64::NAN, |f| f.center)
    }
}

/// Result of [`analyze`] (`BeamResult.ctl`).
#[derive(Clone, Debug, Default)]
pub struct BeamResult {
    pub x: AxisResult,
    pub y: AxisResult,
    /// 1/e² diameters in µm.
    pub dx_um: f64,
    pub dy_um: f64,
    pub peak: u16,
    pub peak_row: usize,
    pub peak_col: usize,
    pub saturated: bool,
    /// The ROI that was fitted: (row0, col0, rows, cols).
    pub roi: (usize, usize, usize, usize),
    /// Darkest 32 x 32 corner mean (after dark subtraction, if any).
    pub background: f64,
    /// Despeckled peak − background.
    pub contrast: f64,
    /// `None` when the frame holds a beam; otherwise why not. Without a beam, fits are dropped and widths are NaN.
    pub no_beam: Option<NoBeam>,
}

impl BeamResult {
    pub fn detected(&self) -> bool {
        self.no_beam.is_none()
    }
}

/// Mean of each 32 x 32 corner patch (dark-subtracted when `dark` is given); returns the darkest.
fn darkest_corner(img: ImageRef, dark: Option<&[f64]>) -> f64 {
    let (h, w) = (img.height, img.width);
    let (ph, pw) = (32.min(h), 32.min(w));
    (0..4)
        .map(|i| {
            let r0 = if i >= 2 { h - ph } else { 0 };
            let c0 = if i % 2 == 1 { w - pw } else { 0 };
            let mut s = 0.0;
            for r in r0..r0 + ph {
                for c in c0..c0 + pw {
                    s += img.at(r, c) as f64 - dark.map_or(0.0, |d| d[r * w + c]);
                }
            }
            s / (ph * pw) as f64
        })
        .fold(f64::INFINITY, f64::min)
}

/// Analyses one frame. `dark`, when given and the same size as the image, is subtracted first.
pub fn analyze(img: ImageRef, p: &BeamParams, dark: Option<&[f64]>) -> BeamResult {
    let (w, h) = (img.width, img.height);
    if w == 0 || h == 0 {
        return BeamResult::default();
    }

    // Peak on the despeckled image min(I[r, c], I[r + 1, c + 1]), as in Beam_FindRegion: an isolated hot pixel
    // (or a hot pair in a row or column) cannot win, so it can neither move the ROI nor flag saturation.
    // A real beam, two or more pixels wide, keeps its peak. First occurrence of the maximum, like Array Max & Min.
    let (mut peak, mut peak_row, mut peak_col) = (0u16, 0usize, 0usize);
    if w >= 2 && h >= 2 {
        for r in 0..h - 1 {
            let (a, b) = (img.row(r), img.row(r + 1));
            for c in 0..w - 1 {
                let v = a[c].min(b[c + 1]);
                if v > peak {
                    (peak, peak_row, peak_col) = (v, r, c);
                }
            }
        }
    } else {
        for (i, &v) in img.data.iter().enumerate() {
            if v > peak {
                (peak, peak_row, peak_col) = (v, i / w, i % w);
            }
        }
    }
    let full_scale = p.full_scale.min(u16::MAX as u32) as u16;
    let saturated = peak >= full_scale;

    let (r0, rows, c0, cols) = if p.roi_half == 0 {
        (0, h, 0, w)
    } else {
        let r0 = peak_row.saturating_sub(p.roi_half);
        let c0 = peak_col.saturating_sub(p.roi_half);
        (r0, (2 * p.roi_half).min(h - r0), c0, (2 * p.roi_half).min(w - c0))
    };

    let dark = dark.filter(|d| d.len() == img.data.len());
    let mut yprof = vec![0.0f64; rows];
    let mut xprof = vec![0.0f64; cols];
    for (ri, r) in (r0..r0 + rows).enumerate() {
        let line = &img.row(r)[c0..c0 + cols];
        let mut sum = 0.0;
        match dark {
            Some(d) => {
                let dline = &d[r * w + c0..r * w + c0 + cols];
                for (ci, (&v, &dv)) in line.iter().zip(dline).enumerate() {
                    let v = v as f64 - dv;
                    sum += v;
                    xprof[ci] += v;
                }
            }
            None => {
                for (ci, &v) in line.iter().enumerate() {
                    let v = v as f64;
                    sum += v;
                    xprof[ci] += v;
                }
            }
        }
        yprof[ri] = sum;
    }

    // Detection, check 1: contrast. Noise or a blocked beam never reaches the fit.
    let background = darkest_corner(img, dark);
    let peak_level = peak as f64 - dark.map_or(0.0, |d| d[peak_row * w + peak_col]);
    let contrast = peak_level - background;
    let (mut x, mut y, no_beam) = if contrast < p.min_contrast {
        // Say why: a dark frame, a frame lit everywhere (the background is measured in the darkest corner), or
        // one that is overexposed everywhere.
        let fs = p.full_scale as f64;
        let why = if background >= 0.9 * fs {
            NoBeam::Overexposed { background, full_scale: fs }
        } else if background >= 0.1 * fs {
            NoBeam::EvenlyLit { background, contrast }
        } else {
            NoBeam::LowContrast { contrast }
        };
        (AxisResult::profile_only(c0, xprof), AxisResult::profile_only(r0, yprof), Some(why))
    } else {
        // Checks 2 and 3: each fit must describe the profile, centred inside an ROI that holds the beam.
        let (x, y) = (AxisResult::from_profile(c0, xprof), AxisResult::from_profile(r0, yprof));
        let verdict = x.check('X', p.min_snr).and_then(|_| y.check('Y', p.min_snr)).err();
        (x, y, verdict)
    };
    if no_beam.is_some() {
        x.drop_fit();
        y.drop_fit();
    }
    BeamResult {
        dx_um: 4.0 * x.sigma() * p.pixel_w_um,
        dy_um: 4.0 * y.sigma() * p.pixel_h_um,
        x,
        y,
        peak,
        peak_row,
        peak_col,
        // Only a beam can be saturated; a surviving hot-pixel pair at full scale is not.
        saturated: saturated && no_beam.is_none(),
        roi: (r0, c0, rows, cols),
        background,
        contrast,
        no_beam,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::{render, Spot, SynthOptions};

    const W: usize = 1440;
    const H: usize = 1080;

    fn frame(a: f64) -> Vec<u16> {
        // Test_Beam_Analyze.vi: sigma_x = 40 px, sigma_y = 25 px, offset 60 counts, a little noise, 10-bit clip.
        let spot = Spot { a, x0: 700.3, y0: 515.8, sx: 40.0, sy: 25.0 };
        let opts = SynthOptions { black: 60.0, noise_sigma: 2.5, ..SynthOptions::cs165() };
        render(W, H, &[spot], &opts, 7)
    }

    fn params() -> BeamParams {
        BeamParams { full_scale: 1023, pixel_w_um: 3.45, pixel_h_um: 3.45, roi_half: 150, ..Default::default() }
    }

    #[test]
    fn known_sigma_is_recovered() {
        let img = frame(800.0);
        let r = analyze(ImageRef::new(&img, W, H), &params(), None);
        assert!((r.x.sigma() - 40.0).abs() < 0.2, "sigma_x = {}", r.x.sigma());
        assert!((r.y.sigma() - 25.0).abs() < 0.2, "sigma_y = {}", r.y.sigma());
        assert!((r.dx_um - 552.0).abs() < 3.0, "Dx = {}", r.dx_um);
        assert!((r.x.center() - 700.3).abs() < 0.2 && (r.y.center() - 515.8).abs() < 0.2);
        assert!((r.x.sigma2 - 40.0).abs() < 0.05 * 40.0, "sigma2_x = {}", r.x.sigma2);
        assert!(!r.saturated);
    }

    #[test]
    fn clipped_beam_is_flagged_and_reads_too_wide() {
        let img = frame(1100.0);
        let r = analyze(ImageRef::new(&img, W, H), &params(), None);
        assert!(r.saturated);
        assert!(r.x.sigma() > 40.2, "sigma_x = {}", r.x.sigma());
    }

    #[test]
    fn whole_image_and_dark_frame() {
        let img = frame(800.0);
        let dark = vec![60.0; W * H];
        let p = BeamParams { roi_half: 0, ..params() };
        let r = analyze(ImageRef::new(&img, W, H), &p, Some(&dark));
        assert_eq!(r.roi, (0, 0, H, W));
        assert!((r.x.sigma() - 40.0).abs() < 0.3, "sigma_x = {}", r.x.sigma());
        assert!(r.x.fit.unwrap().offset.abs() < 0.5 * H as f64, "offset not removed");
    }

    #[test]
    fn hot_pixels_do_not_capture_the_roi() {
        let spot = Spot { a: 600.0, x0: 850.0, y0: 465.0, sx: 24.0, sy: 16.0 };
        let mut img = render(W, H, &[spot], &SynthOptions::cs165(), 5);
        for (r, c) in [(82, 121), (900, 40), (10, 1400), (500, 300)] {
            img[r * W + c] = 1023;
        }
        // A hot pair in a row survives min with the diagonal neighbour only if both are hot there too.
        img[700 * W + 700] = 1023;
        img[700 * W + 701] = 1023;
        let r = analyze(ImageRef::new(&img, W, H), &params(), None);
        assert!(!r.saturated, "hot pixels flagged saturation");
        assert!((r.x.center() - 850.0).abs() < 0.5 && (r.y.center() - 465.0).abs() < 0.5, "centre {} {}", r.x.center(), r.y.center());
        assert!((r.x.sigma() - 24.0).abs() < 0.3, "sigma_x = {}", r.x.sigma());
    }

    #[test]
    fn no_beam_gives_no_numbers() {
        // Blocked beam, with hot pixels, many noise seeds: never a beam, never a width.
        for seed in 0..20 {
            let opts = SynthOptions { hot_fraction: 0.0002, ..SynthOptions::cs165() };
            let img = render(W, H, &[], &opts, seed);
            let r = analyze(ImageRef::new(&img, W, H), &params(), None);
            // Usually too faint; a diagonal hot pair survives the despeckle and then fails the fit check instead.
            assert!(!r.detected(), "seed {seed}: noise accepted as a beam: sigma {} / {}, S/N {} / {}, contrast {}", r.x.sigma(), r.y.sigma(), r.x.snr, r.y.snr, r.contrast);
            assert!(r.dx_um.is_nan() && r.dy_um.is_nan() && r.x.fit.is_none() && !r.saturated);
        }
    }

    #[test]
    fn flood_light_and_overexposure_are_named() {
        let flat = |level: f64| render(W, H, &[], &SynthOptions { black: level, ..SynthOptions::cs165() }, 9);
        let r = analyze(ImageRef::new(&flat(500.0), W, H), &params(), None);
        assert!(matches!(r.no_beam, Some(NoBeam::EvenlyLit { .. })), "{:?}", r.no_beam);
        let r = analyze(ImageRef::new(&flat(1100.0), W, H), &params(), None);
        assert!(matches!(r.no_beam, Some(NoBeam::Overexposed { .. })), "{:?}", r.no_beam);
        let r = analyze(ImageRef::new(&flat(12.0), W, H), &params(), None);
        assert!(matches!(r.no_beam, Some(NoBeam::LowContrast { .. })), "{:?}", r.no_beam);
    }

    #[test]
    fn faint_beam_below_contrast_is_not_a_beam() {
        let img = render(W, H, &[Spot { a: 20.0, x0: 700.0, y0: 500.0, sx: 10.0, sy: 10.0 }], &SynthOptions::cs165(), 3);
        let r = analyze(ImageRef::new(&img, W, H), &params(), None);
        assert!(matches!(r.no_beam, Some(NoBeam::LowContrast { .. })), "{:?}", r.no_beam);
    }

    #[test]
    fn real_beams_are_detected() {
        for (a, s) in [(60.0, 3.0), (150.0, 8.0), (600.0, 40.0), (900.0, 1.5)] {
            let img = render(W, H, &[Spot { a, x0: 700.0, y0: 500.0, sx: s, sy: s }], &SynthOptions::cs165(), 4);
            let r = analyze(ImageRef::new(&img, W, H), &params(), None);
            assert!(r.detected(), "A = {a}, sigma = {s}: {:?}", r.no_beam);
            assert!((r.x.sigma() - s).abs() < 0.05 * s + 0.1, "A = {a}: sigma {}", r.x.sigma());
            assert!(r.x.snr > 8.0, "S/N = {}", r.x.snr);
        }
    }

    #[test]
    fn beam_wider_than_roi_is_reported() {
        let img = render(W, H, &[Spot { a: 600.0, x0: 700.0, y0: 540.0, sx: 120.0, sy: 120.0 }], &SynthOptions::cs165(), 6);
        let p = BeamParams { roi_half: 100, ..params() };
        let r = analyze(ImageRef::new(&img, W, H), &p, None);
        assert!(matches!(r.no_beam, Some(NoBeam::WiderThanRoi { .. }) | Some(NoBeam::PoorFit { .. })), "{:?}", r.no_beam);
        let r = analyze(ImageRef::new(&img, W, H), &BeamParams { roi_half: 0, ..params() }, None);
        assert!(r.detected(), "whole frame should measure it: {:?}", r.no_beam);
    }

    #[test]
    fn sigma2_ignores_hot_pixels_far_from_the_beam() {
        let spot = Spot { a: 600.0, x0: 700.0, y0: 520.0, sx: 12.0, sy: 9.0 };
        let mut img = render(W, H, &[spot], &SynthOptions::cs165(), 8);
        // Hot pixels inside the 300 x 300 ROI, 80 to 140 px from the beam.
        for (r, c) in [(520, 600), (450, 820), (610, 790), (380, 700), (640, 580), (500, 840)] {
            img[r * W + c] = 1023;
        }
        let r = analyze(ImageRef::new(&img, W, H), &params(), None);
        assert!(r.detected());
        assert!((r.x.sigma2 / r.x.sigma() - 1.0).abs() < 0.05, "sigma2_x {} vs sigma {}", r.x.sigma2, r.x.sigma());
        assert!((r.y.sigma2 / r.y.sigma() - 1.0).abs() < 0.05, "sigma2_y {} vs sigma {}", r.y.sigma2, r.y.sigma());
    }

    #[test]
    fn roi_is_clipped_at_the_sensor_edge() {
        let spot = Spot { a: 800.0, x0: 10.0, y0: 1075.0, sx: 5.0, sy: 5.0 };
        let img = render(W, H, &[spot], &SynthOptions::cs165(), 1);
        let r = analyze(ImageRef::new(&img, W, H), &params(), None);
        let (r0, c0, rows, cols) = r.roi;
        assert_eq!(c0, 0);
        assert!(r0 + rows <= H && c0 + cols <= W);
    }
}

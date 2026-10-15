//! Auto-zoom for the image display (`AutoZoom_Design.md`).
//!
//! [`find_region`] finds the box around every bright spot (`Beam_FindRegion.vi`), [`zoom_window`] turns it into a
//! steady display window with square pixels (`Zoom_Window.vi`), and [`display_params`] gives the contrast and the
//! zoom text (`Display_Image.vi`). Every formula follows the Formula Node text of the design document; `int()` there
//! rounds to nearest with ties to even, which is [`lv_int`] here.

use crate::ImageRef;

/// Inclusive-exclusive pixel extents of the bright region (`Box.ctl`). All -1 when nothing was found.
/// `row1` / `col1` are the last counting despeckled line + 1 (see the design, section 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BBox {
    pub row0: i32,
    pub row1: i32,
    pub col0: i32,
    pub col1: i32,
}

impl BBox {
    pub const NONE: BBox = BBox { row0: -1, row1: -1, col0: -1, col1: -1 };
}

/// The part of the frame on screen (`Window.ctl`): first row and column, and how many.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub row0: i32,
    pub col0: i32,
    pub rows: i32,
    pub cols: i32,
}

impl Window {
    pub fn full(rows: usize, cols: usize) -> Self {
        Self { row0: 0, col0: 0, rows: rows as i32, cols: cols as i32 }
    }

    fn fits(&self, h: i32, w: i32) -> bool {
        self.rows > 0 && self.cols > 0 && self.row0 >= 0 && self.col0 >= 0 && self.row0 + self.rows <= h && self.col0 + self.cols <= w
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ZoomMode {
    #[default]
    Auto,
    FullFrame,
    /// Keep the current window.
    Lock,
}

/// `ZoomSettings.ctl`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ZoomSettings {
    pub mode: ZoomMode,
    /// Detection level, % of the way from background to peak. 13.5 = the 1/e² level.
    pub threshold_pct: f64,
    /// Largest magnification; the smallest window is frame / max zoom.
    pub max_zoom: f64,
    /// Pad around the box, as a multiple of its shorter side.
    pub margin: f64,
}

impl Default for ZoomSettings {
    fn default() -> Self {
        Self { mode: ZoomMode::Auto, threshold_pct: 13.5, max_zoom: 16.0, margin: 0.5 }
    }
}

/// Default "Min pixels per line": rejects hot pixels that survive the despeckle.
pub const MIN_PIXELS_PER_LINE: u32 = 3;

/// Output of [`find_region`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Region {
    pub found: bool,
    pub bbox: BBox,
    /// Brightest despeckled pixel.
    pub peak: f64,
    /// Darkest of the four 32 x 32 corner means.
    pub background: f64,
    /// Frame size.
    pub rows: usize,
    pub cols: usize,
}

/// LabVIEW Formula Node `int(x)`: round to nearest, ties to even.
#[inline]
pub fn lv_int(x: f64) -> i32 {
    x.round_ties_even() as i32
}

/// `Beam_FindRegion.vi`: the box that holds every spot brighter than `threshold_pct` of the way from the
/// background to the peak. Pure: no state.
pub fn find_region(img: ImageRef, threshold_pct: f64, min_pixels_per_line: u32, was_zoomed: bool) -> Region {
    let (h, w) = (img.height, img.width);
    let mut out = Region { found: false, bbox: BBox::NONE, peak: 0.0, background: 0.0, rows: h, cols: w };
    if h < 2 || w < 2 {
        return out;
    }

    // 1. Despeckle: E[r, c] = min(Image[r, c], Image[r + 1, c + 1]), (h - 1) x (w - 1).
    //    Isolated hot pixels, hot pairs in a row or column and hot lines disappear.
    let e = |r: usize, c: usize| -> u16 { img.at(r, c).min(img.at(r + 1, c + 1)) };

    // 2. Peak of E.
    let mut pk = 0u16;
    for r in 0..h - 1 {
        let (a, b) = (img.row(r), img.row(r + 1));
        for c in 0..w - 1 {
            pk = pk.max(a[c].min(b[c + 1]));
        }
    }

    // 4. Background: mean of each 32 x 32 corner patch of the image.
    let (ph, pw) = (32.min(h), 32.min(w));
    let corner = |i: usize| -> f64 {
        let r0 = if i >= 2 { h - ph } else { 0 };
        let c0 = if i == 1 || i == 3 { w - pw } else { 0 };
        let mut s = 0u64;
        for r in r0..r0 + ph {
            s += img.row(r)[c0..c0 + pw].iter().map(|&v| v as u64).sum::<u64>();
        }
        s as f64 / (ph * pw) as f64
    };
    let q = [corner(0), corner(1), corner(2), corner(3)];

    // 5. Level node.
    let bg = q[0].min(q[1]).min(q[2].min(q[3]));
    let c = pk as f64 - bg;
    let lev = bg + (threshold_pct / 100.0 * c).max(10.0);
    let ok = c >= if was_zoomed { 20.0 } else { 30.0 };
    out.peak = pk as f64;
    out.background = bg;
    if !ok {
        return out;
    }

    // 6. Bright pixels per despeckled row and column, then the Box node.
    let level = lev.round_ties_even().clamp(0.0, u16::MAX as f64) as u16;
    let mut nr = vec![0u32; h - 1];
    let mut nc = vec![0u32; w - 1];
    for (r, nrr) in nr.iter_mut().enumerate() {
        for (col, ncc) in nc.iter_mut().enumerate() {
            if e(r, col) > level {
                *nrr += 1;
                *ncc += 1;
            }
        }
    }
    let extent = |n: &[u32]| -> (i32, i32) {
        let (mut a, mut b) = (-1i32, -1i32);
        for (j, &v) in n.iter().enumerate() {
            if v >= min_pixels_per_line {
                if a < 0 {
                    a = j as i32;
                }
                b = j as i32 + 1;
            }
        }
        (a, b)
    };
    let (r0, r1) = extent(&nr);
    let (c0, c1) = extent(&nc);
    if r0 >= 0 && c0 >= 0 {
        out.found = true;
        out.bbox = BBox { row0: r0, row1: r1, col0: c0, col1: c1 };
    }
    out
}

/// `Zoom_Window.vi`: the new display window and whether it is zoomed (smaller than the frame).
/// `aspect` = plot-area width / height. `prev` is the window of the last update (the caller keeps it).
pub fn zoom_window(prev: Window, region: &Region, aspect: f64, zs: &ZoomSettings) -> (Window, bool) {
    let (hi, wi) = (region.rows as i32, region.cols as i32);
    let full = Window::full(region.rows, region.cols);
    let prev = if prev.fits(hi, wi) { prev } else { full };
    let a = if aspect.is_finite() && aspect > 0.0 { aspect } else { 4.0 / 3.0 };

    let win = match zs.mode {
        ZoomMode::FullFrame => full,
        ZoomMode::Lock => prev,
        ZoomMode::Auto => {
            let (h, w) = (hi as f64, wi as f64);
            let b = region.bbox;
            let (r0, r1, c0, c1) = (b.row0 as f64, b.row1 as f64, b.col0 as f64, b.col1 as f64);
            let (pr, pc, ph, pw) = (prev.row0 as f64, prev.col0 as f64, prev.rows as f64, prev.cols as f64);
            let (mg, z) = (zs.margin, zs.max_zoom.max(1.0));

            // Target
            let bw = c1 - c0 + 1.0;
            let bh = r1 - r0 + 1.0;
            let d = (mg * bw.min(bh)).max(16.0);
            let th = (bh + 2.0 * d).max(h / z).max((bw + 2.0 * d).max(w / z) / a);
            let s = (a * th / w).max(th / h);
            let zm = region.found && s < if ph > 0.0 && ph < h { 0.8 } else { 0.6 };

            // Gain
            let k = d / 2.0;
            let keep = (c0 - k).max(0.0) >= pc
                && (c1 + 1.0 + k).min(w) <= pc + pw
                && (r0 - k).max(0.0) >= pr
                && (r1 + 1.0 + k).min(h) <= pr + ph;
            let e = (th - ph).abs() / ph
                + 0.5 * (c0 + c1 + 1.0 - 2.0 * pc - pw).abs() / pw
                + 0.5 * (r0 + r1 + 1.0 - 2.0 * pr - ph).abs() / ph;
            let g = if zm && ph < h && keep { if e < 0.1 { 0.0 } else { 0.3 } } else { 1.0 };

            // New size
            let rows = if zm { lv_int(ph + g * (th - ph)).clamp(1, hi) } else { hi };
            let cols = if zm { lv_int(a * rows as f64).min(wi).max(1) } else { wi };

            // New position: ease the centre, then shift (never shrink) into the frame.
            let y = pr + 0.5 * (ph - rows as f64) + g * (0.5 * (r0 + r1 + 1.0 - ph) - pr);
            let x = pc + 0.5 * (pw - cols as f64) + g * (0.5 * (c0 + c1 + 1.0 - pw) - pc);
            let row0 = lv_int(y).max(0).min(hi - rows);
            let col0 = lv_int(x).max(0).min(wi - cols);
            Window { row0, col0, rows, cols }
        }
    };
    (win, win.rows < hi)
}

/// What `Display_Image.vi` writes besides the crop.
#[derive(Clone, Debug, PartialEq)]
pub struct DisplayParams {
    /// White level: the brightest despeckled pixel, but at least background + 30 so noise stays dark.
    pub zmax: f64,
    pub zoom: f64,
    /// e.g. `Zoom 15.8x  |  91 x 68 px at col 675, row 506`
    pub text: String,
}

/// `unit` names an image pixel in the text: "px", or "cells" on a colour camera.
pub fn display_params(win: &Window, region: &Region, unit: &str) -> DisplayParams {
    let zmax = region.peak.max(region.background + 30.0);
    let zoom = (region.cols as f64 / win.cols.max(1) as f64).min(region.rows as f64 / win.rows.max(1) as f64);
    let text = format!("Zoom {:.1}x  |  {} x {} {unit} at col {}, row {}", zoom, win.cols, win.rows, win.col0, win.row0);
    DisplayParams { zmax, zoom, text }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::{render, Spot, SynthOptions};

    const W: usize = 1440;
    const H: usize = 1080;
    const A43: f64 = 1440.0 / 1080.0;

    fn spot(a: f64, x0: f64, y0: f64, sx: f64, sy: f64) -> Spot {
        Spot { a, x0, y0, sx, sy }
    }

    /// The `Inside?` node: the spot's 1/e² ellipse, clipped to the sensor, lies inside the window.
    fn inside(w: &Window, s: &Spot) -> bool {
        let (x, y, a, b) = (s.x0 + 0.5, s.y0 + 0.5, 2.0 * s.sx, 2.0 * s.sy);
        let (c0, r0, nc, nr) = (w.col0 as f64, w.row0 as f64, w.cols as f64, w.rows as f64);
        (x - a).max(0.0) >= c0 - 0.5
            && (x + a).min(W as f64) <= c0 + nc + 0.5
            && (y - b).max(0.0) >= r0 - 0.5
            && (y + b).min(H as f64) <= r0 + nr + 0.5
    }

    /// The Processing loop's state: Window and Zoomed? shift registers.
    struct Loop {
        win: Window,
        zoomed: bool,
        aspect: f64,
        seed: u64,
    }

    impl Loop {
        fn new(aspect: f64) -> Self {
            Self { win: Window::full(H, W), zoomed: false, aspect, seed: 1 }
        }

        fn update(&mut self, spots: &[Spot], hot: f64) -> Region {
            self.seed += 1;
            let opts = SynthOptions { hot_fraction: hot, ..SynthOptions::cs165() };
            let img = render(W, H, spots, &opts, self.seed);
            let reg = find_region(ImageRef::new(&img, W, H), 13.5, MIN_PIXELS_PER_LINE, self.zoomed);
            let (win, zoomed) = zoom_window(self.win, &reg, self.aspect, &ZoomSettings::default());
            assert!(win.fits(H as i32, W as i32), "window outside the frame: {win:?}");
            if zoomed {
                assert!((win.cols as f64 - self.aspect * win.rows as f64).abs() <= 1.0, "pixels not square: {win:?}");
            }
            self.win = win;
            self.zoomed = zoomed;
            reg
        }
    }

    enum Expect {
        Full,
        /// rows, cols, optional (row0, col0)
        Zoom(i32, i32, Option<(i32, i32)>),
    }

    fn static_case(name: &str, spots: &[Spot], hot: f64, expect: Expect) {
        let mut lp = Loop::new(A43);
        let mut settled = None;
        for k in 0..12 {
            let reg = lp.update(spots, hot);
            for s in spots {
                assert!(inside(&lp.win, s), "{name}: update {k} cuts {s:?}, window {:?}", lp.win);
            }
            if k == 3 {
                settled = Some(lp.win);
            }
            if k > 3 {
                assert_eq!(Some(lp.win), settled, "{name}: window still moves at update {k}");
            }
            if spots.is_empty() {
                assert!(!reg.found, "{name}: found a beam in a dark frame");
            }
        }
        let w = lp.win;
        let close = |got: i32, want: i32| (got - want).abs() as f64 <= (0.03 * want as f64).max(2.0);
        match expect {
            Expect::Full => assert_eq!(w, Window::full(H, W), "{name}: expected full frame"),
            Expect::Zoom(rows, cols, pos) => {
                assert!(close(w.rows, rows) && close(w.cols, cols), "{name}: {} x {} px, expected {cols} x {rows}", w.cols, w.rows);
                if let Some((r0, c0)) = pos {
                    assert!(close(w.row0, r0) && close(w.col0, c0), "{name}: at row {}, col {}, expected {r0}, {c0}", w.row0, w.col0);
                }
            }
        }
    }

    // The presets of Test_AutoZoom.vi (design section 8), with the results of the Python reference (4:3 plot).
    #[test]
    fn small_beam() {
        static_case("small", &[spot(600.0, 720.0, 540.0, 5.0, 5.0)], 0.0, Expect::Zoom(68, 91, None));
    }
    #[test]
    fn corner() {
        static_case("corner", &[spot(600.0, 25.0, 20.0, 8.0, 8.0)], 0.0, Expect::Zoom(68, 91, Some((0, 0))));
    }
    #[test]
    fn on_the_edge() {
        static_case("edge", &[spot(600.0, 1437.0, 600.0, 10.0, 10.0)], 0.0, Expect::Zoom(71, 95, Some((565, 1345))));
    }
    #[test]
    fn large_beam() {
        static_case("large", &[spot(600.0, 720.0, 540.0, 150.0, 150.0)], 0.0, Expect::Full);
    }
    #[test]
    fn medium_beam() {
        static_case("medium", &[spot(600.0, 720.0, 540.0, 60.0, 60.0)], 0.0, Expect::Zoom(482, 643, None));
    }
    #[test]
    fn two_close() {
        let s = [spot(600.0, 690.0, 530.0, 8.0, 8.0), spot(500.0, 750.0, 550.0, 8.0, 8.0)];
        static_case("two close", &s, 0.0, Expect::Zoom(106, 141, None));
    }
    #[test]
    fn two_far_apart() {
        let s = [spot(600.0, 200.0, 200.0, 8.0, 8.0), spot(600.0, 1300.0, 900.0, 8.0, 8.0)];
        static_case("two far", &s, 0.0, Expect::Full);
    }
    #[test]
    fn three_spots() {
        let s = [
            spot(800.0, 650.0, 480.0, 10.0, 10.0),
            spot(480.0, 780.0, 520.0, 10.0, 10.0),
            spot(240.0, 700.0, 640.0, 10.0, 10.0),
        ];
        static_case("three spots", &s, 0.0, Expect::Zoom(360, 480, None));
    }
    #[test]
    fn dark_frame() {
        static_case("dark", &[], 0.0, Expect::Full);
        static_case("dark + hot pixels", &[], 0.0002, Expect::Full);
    }
    #[test]
    fn hot_pixels_brighter_than_beam() {
        static_case("hot > beam", &[spot(150.0, 400.0, 300.0, 6.0, 6.0)], 0.0002, Expect::Zoom(68, 91, None));
    }
    #[test]
    fn saturated() {
        static_case("saturated", &[spot(3000.0, 720.0, 540.0, 10.0, 10.0)], 0.0, Expect::Zoom(98, 131, None));
    }
    #[test]
    fn elliptical() {
        static_case("elliptical", &[spot(600.0, 720.0, 540.0, 20.0, 5.0)], 0.0, Expect::Zoom(83, 111, None));
    }
    #[test]
    fn too_dim() {
        static_case("too dim", &[spot(20.0, 720.0, 540.0, 5.0, 5.0)], 0.0, Expect::Full);
    }

    fn toggles(states: &[bool]) -> usize {
        states.windows(2).filter(|p| p[0] != p[1]).count()
    }

    #[test]
    fn moving_beam_is_followed_and_never_cut() {
        for step in [1.0, 5.0, 25.0] {
            let mut lp = Loop::new(A43);
            for k in 0..44 {
                let s = spot(600.0, 300.0 + step * k as f64, 540.0, 5.0, 5.0);
                lp.update(&[s], 0.0);
                assert!(inside(&lp.win, &s), "step {step}: update {k} cuts the beam, window {:?}", lp.win);
                assert!(lp.zoomed);
            }
        }
    }

    #[test]
    fn jump_is_followed_on_the_same_update() {
        let mut lp = Loop::new(A43);
        for k in 0..40 {
            let x = if k < 20 { 500.0 } else { 800.0 };
            let s = spot(600.0, x, 540.0, 5.0, 5.0);
            lp.update(&[s], 0.0);
            assert!(inside(&lp.win, &s), "update {k} cuts the beam");
        }
    }

    #[test]
    fn grow_and_shrink_toggles_twice() {
        let mut lp = Loop::new(A43);
        let mut states = Vec::new();
        for k in 0..160 {
            let t = if k < 80 { k as f64 / 79.0 } else { (159 - k) as f64 / 79.0 };
            let sigma = 2.0 * 100f64.powf(t); // 2 -> 200 -> 2
            let s = spot(600.0, 720.0, 540.0, sigma, sigma);
            lp.update(&[s], 0.0);
            assert!(inside(&lp.win, &s), "update {k} (sigma {sigma:.1}) cuts the beam");
            states.push(lp.zoomed);
        }
        assert!(states[0]);
        assert_eq!(toggles(&states), 2, "zoom toggled {} times", toggles(&states));
    }

    #[test]
    fn beam_in_hysteresis_band_does_not_toggle() {
        let mut lp = Loop::new(A43);
        let mut states = Vec::new();
        for _ in 0..40 {
            lp.update(&[spot(600.0, 720.0, 540.0, 95.0, 95.0)], 0.0);
            states.push(lp.zoomed);
        }
        assert_eq!(toggles(&states), 0);
    }

    #[test]
    fn dim_beam_near_threshold_toggles_at_most_once() {
        for a in [24.0, 27.0, 30.0] {
            let mut lp = Loop::new(A43);
            let mut states = Vec::new();
            for _ in 0..40 {
                lp.update(&[spot(a, 720.0, 540.0, 5.0, 5.0)], 0.0);
                states.push(lp.zoomed);
            }
            assert!(toggles(&states) <= 1, "A = {a}: {} toggles", toggles(&states));
        }
    }

    #[test]
    fn full_frame_and_lock_modes() {
        let img = render(W, H, &[spot(600.0, 720.0, 540.0, 5.0, 5.0)], &SynthOptions::cs165(), 3);
        let reg = find_region(ImageRef::new(&img, W, H), 13.5, 3, false);
        let full = ZoomSettings { mode: ZoomMode::FullFrame, ..Default::default() };
        assert_eq!(zoom_window(Window::full(H, W), &reg, A43, &full), (Window::full(H, W), false));
        let locked = Window { row0: 100, col0: 200, rows: 300, cols: 400 };
        let lock = ZoomSettings { mode: ZoomMode::Lock, ..Default::default() };
        assert_eq!(zoom_window(locked, &reg, A43, &lock), (locked, true));
    }

    #[test]
    fn zoom_text() {
        let reg = Region { found: true, bbox: BBox::NONE, peak: 600.0, background: 12.0, rows: H, cols: W };
        let w = Window { row0: 506, col0: 675, rows: 68, cols: 91 };
        let p = display_params(&w, &reg, "px");
        assert_eq!(p.text, "Zoom 15.8x  |  91 x 68 px at col 675, row 506");
        assert_eq!(p.zmax, 600.0);
    }
}

#[cfg(test)]
mod timing {
    use super::*;
    use crate::synth::{render, Spot, SynthOptions};

    /// `cargo test -p beam --release -- --ignored --nocapture timing`
    #[test]
    #[ignore]
    fn timing() {
        let (w, h) = (1440, 1080);
        let t = std::time::Instant::now();
        let img = render(w, h, &[Spot { a: 600.0, x0: 720.0, y0: 540.0, sx: 5.0, sy: 5.0 }], &SynthOptions::cs165(), 1);
        println!("render        {:?}", t.elapsed());
        let t = std::time::Instant::now();
        for _ in 0..10 {
            std::hint::black_box(find_region(ImageRef::new(&img, w, h), 13.5, 3, false));
        }
        println!("find_region   {:?}", t.elapsed() / 10);
        let t = std::time::Instant::now();
        let p = crate::BeamParams { full_scale: 1023, pixel_w_um: 3.45, pixel_h_um: 3.45, roi_half: 0, ..Default::default() };
        for _ in 0..10 {
            std::hint::black_box(crate::analyze(ImageRef::new(&img, w, h), &p, None));
        }
        println!("analyze full  {:?}", t.elapsed() / 10);
    }
}

//! Painting the analysed frame as RGBA, the way the desktop app's display does (`thorcam/src/pipeline.rs`).

use beam::bayer::{cell_rgb, Pattern};
use beam::{ImageRef, Window};

pub type Rgba = [u8; 4];

/// An RGBA picture, row-major.
#[derive(Clone, Debug, Default)]
pub struct Picture {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

impl Picture {
    fn new(width: usize, height: usize, px: impl IntoIterator<Item = Rgba>) -> Self {
        let mut rgba = Vec::with_capacity(width * height * 4);
        px.into_iter().for_each(|p| rgba.extend_from_slice(&p));
        Self { width, height, rgba }
    }
}

const RED: Rgba = [255, 0, 0, 255];
const MAGENTA: Rgba = [255, 0, 255, 255];

/// Width limit of the full-frame overview image.
pub const FULL_VIEW_MAX: usize = 720;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Palette {
    #[default]
    Gray,
    Thermal,
}

pub fn lut(p: Palette) -> [Rgba; 256] {
    std::array::from_fn(|i| {
        let t = i as f32 / 255.0;
        match p {
            Palette::Gray => [i as u8, i as u8, i as u8, 255],
            // black -> blue -> magenta -> orange -> yellow -> white
            Palette::Thermal => {
                const STOPS: [(f32, [f32; 3]); 6] = [
                    (0.0, [0.0, 0.0, 0.0]),
                    (0.2, [0.1, 0.0, 0.55]),
                    (0.45, [0.75, 0.05, 0.55]),
                    (0.7, [1.0, 0.45, 0.0]),
                    (0.9, [1.0, 0.9, 0.1]),
                    (1.0, [1.0, 1.0, 1.0]),
                ];
                let j = STOPS.windows(2).position(|w| t <= w[1].0).unwrap_or(STOPS.len() - 2);
                let ((t0, a), (t1, b)) = (STOPS[j], STOPS[j + 1]);
                let u = ((t - t0) / (t1 - t0)).clamp(0.0, 1.0);
                let c = |k: usize| ((a[k] + u * (b[k] - a[k])) * 255.0).round() as u8;
                [c(0), c(1), c(2), 255]
            }
        }
    })
}

/// The crop as colours: 0 is black, `zmax` is white (or the top of the palette); saturated pixels are red.
/// `clipped` (colour cameras) marks saturated cells instead of the value test.
pub fn render(img: ImageRef, win: &Window, zmax: f64, full_scale: u32, lut: &[Rgba; 256], clipped: Option<&[bool]>) -> Picture {
    let (r0, c0, rows, cols) = (win.row0 as usize, win.col0 as usize, win.rows as usize, win.cols as usize);
    let k = 255.0 / zmax.max(1.0) as f32;
    let sat = full_scale.min(u16::MAX as u32) as u16;
    let mut px = Vec::with_capacity(rows * cols);
    for r in r0..r0 + rows {
        for (c, &v) in (c0..).zip(&img.row(r)[c0..c0 + cols]) {
            let sat = clipped.map_or(v >= sat, |m| m[r * img.width + c]);
            px.push(if sat { RED } else { lut[((v as f32 * k) as usize).min(255)] });
        }
    }
    Picture::new(cols, rows, px)
}

/// The whole frame, block-averaged so it is at most [`FULL_VIEW_MAX`] px wide.
pub fn render_overview(img: ImageRef, zmax: f64, lut: &[Rgba; 256]) -> Picture {
    let f = img.width.div_ceil(FULL_VIEW_MAX).max(1);
    let (cols, rows) = (img.width / f, img.height / f);
    let k = 255.0 / (zmax.max(1.0) * (f * f) as f64) as f32;
    let mut sums = vec![0u32; cols];
    let mut px = Vec::with_capacity(rows * cols);
    for br in 0..rows {
        sums.fill(0);
        for r in br * f..(br + 1) * f {
            let line = img.row(r);
            for (bc, s) in sums.iter_mut().enumerate() {
                *s += line[bc * f..(bc + 1) * f].iter().map(|&v| v as u32).sum::<u32>();
            }
        }
        px.extend(sums.iter().map(|&s| lut[((s as f32 * k) as usize).min(255)]));
    }
    Picture::new(cols, rows, px)
}

/// Colour picture of a Bayer frame, one display pixel per 2 x 2 cell: subtract black, apply the camera's
/// white balance and colour correction, scale so the brightest 0.1 % of the crop is white.
/// Cells with a clipped raw pixel are magenta (red would vanish in a red beam).
pub struct RgbView<'a> {
    pub raw: ImageRef<'a>,
    pub pat: Pattern,
    pub m: [f32; 9],
    pub black: f32,
    pub sat: u32,
}

impl RgbView<'_> {
    fn rgb(&self, i: usize, j: usize) -> [f32; 3] {
        let c = cell_rgb(self.raw, self.pat, i, j).map(|v| (v - self.black).max(0.0));
        let m = &self.m;
        std::array::from_fn(|r| (m[r * 3] * c[0] + m[r * 3 + 1] * c[1] + m[r * 3 + 2] * c[2]).max(0.0))
    }

    /// 255 / white level, from the 99.9th percentile of the brightest component in the window.
    pub fn scale(&self, win: &Window) -> f32 {
        let mut v: Vec<f32> = Vec::with_capacity((win.rows * win.cols) as usize);
        for i in win.row0 as usize..(win.row0 + win.rows) as usize {
            for j in win.col0 as usize..(win.col0 + win.cols) as usize {
                let [r, g, b] = self.rgb(i, j);
                v.push(r.max(g).max(b));
            }
        }
        if v.is_empty() {
            return 1.0;
        }
        let k = ((v.len() - 1) as f32 * 0.999) as usize;
        let (_, white, _) = v.select_nth_unstable_by(k, f32::total_cmp);
        255.0 / white.max(30.0)
    }

    pub fn render(&self, win: &Window, k: f32) -> Picture {
        let (r0, c0, rows, cols) = (win.row0 as usize, win.col0 as usize, win.rows as usize, win.cols as usize);
        let sat = self.sat.min(u16::MAX as u32) as u16;
        let mut px = Vec::with_capacity(rows * cols);
        for i in r0..r0 + rows {
            for j in c0..c0 + cols {
                let clipped = (0..2).any(|dr| (0..2).any(|dc| self.raw.at(2 * i + dr, 2 * j + dc) >= sat));
                px.push(if clipped {
                    MAGENTA
                } else {
                    let [r, g, b] = self.rgb(i, j).map(|v| (v * k).min(255.0) as u8);
                    [r, g, b, 255]
                });
            }
        }
        Picture::new(cols, rows, px)
    }
}

/// Despeckled maximum minus mean: how much light a would-be dark frame saw. Fixed hot pixels do not count.
pub fn light_in_dark(d: &[f64], w: usize, h: usize) -> f64 {
    if w < 2 || h < 2 {
        return 0.0;
    }
    let mean = d.iter().sum::<f64>() / d.len() as f64;
    let mut max = f64::MIN;
    for r in 0..h - 1 {
        for c in 0..w - 1 {
            max = max.max(d[r * w + c].min(d[(r + 1) * w + c + 1]));
        }
    }
    max - mean
}

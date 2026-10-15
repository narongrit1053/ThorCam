//! Colour (Bayer) sensors: one analysis pixel per 2 x 2 cell of the colour filter array.
//!
//! The raw image of a colour sensor is a mosaic: each pixel sits behind a red, green or blue filter, and the filters
//! pass a laser line very differently. Fitting the raw mosaic would fit a checkerboard, and demosaicing would add
//! interpolated values. So every 2 x 2 cell (R, G, G, B) becomes one pixel of a half-size image, and the effective
//! pixel size doubles. A cell is clipped when any of the raw pixels it uses is at full scale. Clipping is kept as a
//! separate mask, not written into the values: a hot pixel would otherwise become a full-scale outlier of the sum
//! and distort the profiles.

use crate::ImageRef;

/// Where red and blue sit in the 2 x 2 cell, as (row, col); the other two pixels are green.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pattern {
    pub red: (usize, usize),
    pub blue: (usize, usize),
}

impl Pattern {
    /// R G / G B
    pub const RGGB: Pattern = Pattern { red: (0, 0), blue: (1, 1) };
    /// B G / G R
    pub const BGGR: Pattern = Pattern { red: (1, 1), blue: (0, 0) };
    /// G R / B G
    pub const GRBG: Pattern = Pattern { red: (0, 1), blue: (1, 0) };
    /// G B / R G
    pub const GBRG: Pattern = Pattern { red: (1, 0), blue: (0, 1) };

    /// The two green positions.
    fn greens(&self) -> [(usize, usize); 2] {
        // Red and blue are always diagonal to each other; green fills the other diagonal.
        let (r, c) = self.red;
        [(r, 1 - c), (1 - r, c)]
    }
}

/// What the analysis uses from each cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Channel {
    /// R + G + G + B: all the light.
    #[default]
    Sum,
    Red,
    /// G + G.
    Green,
    Blue,
}

impl Channel {
    pub const ALL: [Channel; 4] = [Channel::Sum, Channel::Red, Channel::Green, Channel::Blue];

    pub fn name(self) -> &'static str {
        match self {
            Channel::Sum => "Sum (R+G+G+B)",
            Channel::Red => "Red",
            Channel::Green => "Green (G+G)",
            Channel::Blue => "Blue",
        }
    }

    /// Full scale of the channel image for a raw full scale `fs`.
    pub fn full_scale(self, fs: u32) -> u32 {
        let k = match self {
            Channel::Sum => 4,
            Channel::Green => 2,
            Channel::Red | Channel::Blue => 1,
        };
        (k * fs).min(u16::MAX as u32)
    }
}

/// A half-size image made from the cells, row-major.
#[derive(Clone, Debug)]
pub struct ChannelImage {
    pub data: Vec<u16>,
    pub width: usize,
    pub height: usize,
    /// Largest possible value of `data`.
    pub full_scale: u32,
    /// `true` where a raw pixel used by the cell is at full scale.
    pub clipped: Vec<bool>,
}

impl ChannelImage {
    pub fn as_ref(&self) -> ImageRef<'_> {
        ImageRef::new(&self.data, self.width, self.height)
    }

    /// Saturated: two clipped cells touch diagonally. An isolated hot pixel (one clipped cell) does not count,
    /// the same rule as the despeckle of the monochrome analysis.
    pub fn saturated(&self) -> bool {
        let (w, h) = (self.width, self.height);
        (0..h.saturating_sub(1)).any(|r| (0..w.saturating_sub(1)).any(|c| self.clipped[r * w + c] && self.clipped[(r + 1) * w + c + 1]))
    }
}

/// Builds the channel image. `raw_full_scale` is the raw sensor's full scale, e.g. 1023 for 10 bit.
/// An odd last row or column (half a cell) is dropped.
pub fn channel_image(raw: ImageRef, pat: Pattern, ch: Channel, raw_full_scale: u32) -> ChannelImage {
    let (w, h) = (raw.width / 2, raw.height / 2);
    let fs = raw_full_scale.min(u16::MAX as u32) as u16;
    let out_fs = ch.full_scale(raw_full_scale);
    let at = |i: usize, j: usize, (dr, dc): (usize, usize)| raw.at(2 * i + dr, 2 * j + dc);
    let [g1, g2] = pat.greens();
    let mut data = Vec::with_capacity(w * h);
    let mut clipped = Vec::with_capacity(w * h);
    for i in 0..h {
        for j in 0..w {
            let used: &[u16] = match ch {
                Channel::Sum => &[at(i, j, pat.red), at(i, j, g1), at(i, j, g2), at(i, j, pat.blue)],
                Channel::Red => &[at(i, j, pat.red)],
                Channel::Green => &[at(i, j, g1), at(i, j, g2)],
                Channel::Blue => &[at(i, j, pat.blue)],
            };
            clipped.push(used.iter().any(|&v| v >= fs));
            data.push(used.iter().map(|&v| v as u32).sum::<u32>().min(out_fs) as u16);
        }
    }
    ChannelImage { data, width: w, height: h, full_scale: out_fs, clipped }
}

/// Camera R, G, B of cell (`i`, `j`); green is the mean of the two green pixels.
#[inline]
pub fn cell_rgb(raw: ImageRef, pat: Pattern, i: usize, j: usize) -> [f32; 3] {
    let at = |(dr, dc): (usize, usize)| raw.at(2 * i + dr, 2 * j + dc) as f32;
    let [g1, g2] = pat.greens();
    [at(pat.red), 0.5 * (at(g1) + at(g2)), at(pat.blue)]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4 x 6 raw image whose cells hold R = 10, G = 20 and 30, B = 40 in the given pattern.
    fn mosaic(p: Pattern) -> Vec<u16> {
        let (w, h) = (6, 4);
        let mut v = vec![0u16; w * h];
        let [g1, g2] = p.greens();
        for i in 0..h / 2 {
            for j in 0..w / 2 {
                for ((dr, dc), val) in [(p.red, 10), (g1, 20), (g2, 30), (p.blue, 40)] {
                    v[(2 * i + dr) * w + 2 * j + dc] = val;
                }
            }
        }
        v
    }

    #[test]
    fn every_pattern_gives_the_right_channels() {
        for p in [Pattern::RGGB, Pattern::BGGR, Pattern::GRBG, Pattern::GBRG] {
            let raw = mosaic(p);
            let img = ImageRef::new(&raw, 6, 4);
            let got = |ch| channel_image(img, p, ch, 1023);
            assert_eq!(got(Channel::Red).data, vec![10; 6], "{p:?}");
            assert_eq!(got(Channel::Green).data, vec![50; 6], "{p:?}");
            assert_eq!(got(Channel::Blue).data, vec![40; 6], "{p:?}");
            let s = got(Channel::Sum);
            assert_eq!((s.width, s.height, s.full_scale), (3, 2, 4092));
            assert_eq!(s.data, vec![100; 6]);
            assert_eq!(cell_rgb(img, p, 1, 2), [10.0, 25.0, 40.0]);
        }
    }

    #[test]
    fn named_patterns_match_the_layout() {
        // R G / G B: red at top-left, greens beside and below it.
        assert_eq!(Pattern::RGGB.greens(), [(0, 1), (1, 0)]);
        // G R / B G: greens at top-left and bottom-right.
        assert_eq!(Pattern::GRBG.greens(), [(0, 0), (1, 1)]);
    }

    #[test]
    fn one_clipped_raw_pixel_saturates_the_cell() {
        let p = Pattern::RGGB;
        let mut raw = mosaic(p);
        raw[0] = 1023; // the red pixel of cell (0, 0)
        let img = ImageRef::new(&raw, 6, 4);
        let sum = channel_image(img, p, Channel::Sum, 1023);
        assert_eq!(sum.data[0], 1023 + 20 + 30 + 40, "values are not inflated");
        assert!(sum.clipped[0] && !sum.clipped[1]);
        assert!(!sum.saturated(), "one clipped cell is a hot pixel, not saturation");
        assert!(channel_image(img, p, Channel::Red, 1023).clipped[0]);
        assert!(!channel_image(img, p, Channel::Green, 1023).clipped[0], "green does not use the red pixel");
        // A clipped neighbour on the diagonal: saturated.
        raw[2 * 6 + 2] = 1023; // red pixel of cell (1, 1)
        assert!(channel_image(ImageRef::new(&raw, 6, 4), p, Channel::Sum, 1023).saturated());
    }

    /// A red laser on an RGGB sensor (R 100 %, G 15 %, B 4 %), with hot pixels, measured through every channel.
    #[test]
    fn red_laser_on_a_bayer_sensor() {
        use crate::synth::{render, Spot, SynthOptions};
        use crate::{analyze, BeamParams};
        let (w, h) = (1440, 1080);
        let spot = Spot { a: 600.0, x0: 700.0, y0: 520.0, sx: 40.0, sy: 24.0 };
        let opts = SynthOptions { black: 0.0, noise_sigma: 0.0, ..SynthOptions::cs165() };
        let beam = render(w, h, &[spot], &opts, 1);
        let resp = [[1.0, 0.15], [0.15, 0.04]];
        let mut raw: Vec<u16> = (0..w * h).map(|i| (12.0 + beam[i] as f64 * resp[(i / w) % 2][(i % w) % 2]).round() as u16).collect();
        for i in [5000, 300_000, 700_001, 1_200_000] {
            raw[i] = 1023;
        }
        let img = ImageRef::new(&raw, w, h);
        for ch in Channel::ALL {
            let c = channel_image(img, Pattern::RGGB, ch, 1023);
            let p = BeamParams { full_scale: u32::MAX, pixel_w_um: 6.9, pixel_h_um: 6.9, ..Default::default() };
            let r = analyze(c.as_ref(), &p, None);
            if ch == Channel::Blue {
                // 4 % of 600 = 24 counts: a red laser is too faint in blue, and must be reported so.
                assert!(matches!(r.no_beam, Some(crate::analyze::NoBeam::LowContrast { .. })), "{:?}", r.no_beam);
                continue;
            }
            assert!(r.detected(), "{ch:?}: {:?}", r.no_beam);
            // 40 x 24 px = 20 x 12 cells; the cell centre is half a pixel off, the 1/e² diameter in µm is unchanged.
            assert!((r.x.sigma() - 20.0).abs() < 0.15 && (r.y.sigma() - 12.0).abs() < 0.15, "{ch:?}: {} {}", r.x.sigma(), r.y.sigma());
            assert!((r.dx_um - 4.0 * 40.0 * 3.45).abs() < 3.0, "{ch:?}: Dx {}", r.dx_um);
            assert!((r.x.sigma2 / r.x.sigma() - 1.0).abs() < 0.05, "{ch:?}: hot pixels distort sigma2 = {}", r.x.sigma2);
            assert!(!c.saturated(), "{ch:?}: hot pixels flagged as saturation");
        }
    }

    #[test]
    fn odd_edges_are_dropped() {
        let raw = vec![7u16; 5 * 3];
        let c = channel_image(ImageRef::new(&raw, 5, 3), Pattern::RGGB, Channel::Sum, 1023);
        assert_eq!((c.width, c.height), (2, 1));
    }
}

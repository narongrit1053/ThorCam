//! Beam-profile analysis for the ThorCam beam profiler.
//!
//! This crate has no camera and no GUI code, so every part can be tested on synthetic frames.
//!
//! | Module | Replaces (LabVIEW) |
//! |---|---|
//! | [`fit`] | `Profile_Fit.vi` |
//! | [`analyze`] | `Beam_Analyze.vi` |
//! | [`autozoom`] | `Beam_FindRegion.vi`, `Zoom_Window.vi`, the maths of `Display_Image.vi` |
//! | [`bayer`] | (new) colour sensors: one analysis pixel per 2 x 2 Bayer cell |
//! | [`synth`] | the synthetic frames of `Test_Beam_Analyze.vi` and `Test_AutoZoom.vi` |

pub mod analyze;
pub mod autozoom;
pub mod bayer;
pub mod fit;
pub mod synth;

pub use analyze::{analyze, AxisResult, BeamParams, BeamResult};
pub use autozoom::{find_region, zoom_window, Region, Window, ZoomMode, ZoomSettings};
pub use fit::{fit_gaussian, GaussFit};

/// A borrowed monochrome image, row-major: `data[row * width + col]`.
#[derive(Clone, Copy, Debug)]
pub struct ImageRef<'a> {
    pub data: &'a [u16],
    pub width: usize,
    pub height: usize,
}

impl<'a> ImageRef<'a> {
    pub fn new(data: &'a [u16], width: usize, height: usize) -> Self {
        assert_eq!(data.len(), width * height, "image buffer does not match width x height");
        Self { data, width, height }
    }

    #[inline]
    pub fn at(&self, row: usize, col: usize) -> u16 {
        self.data[row * self.width + col]
    }

    #[inline]
    pub fn row(&self, row: usize) -> &'a [u16] {
        &self.data[row * self.width..(row + 1) * self.width]
    }
}

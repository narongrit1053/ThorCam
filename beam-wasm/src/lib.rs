//! The beam profiler's processing for the browser, compiled to WebAssembly.
//!
//! This is the desktop app's processing thread (`thorcam/src/pipeline.rs::processing`) without files and threads:
//! decode a USB frame (or simulate one), analyse it with the unchanged `beam` crate, keep the auto-zoom window and
//! the dark frame, and paint the display images. The page's worker calls the C-ABI functions at the bottom; data
//! goes through buffers in WebAssembly memory, results come back as JSON.

pub mod raw10;
pub mod render;
pub mod sim;

use std::cell::RefCell;
use std::fmt::Write as _;

use beam::autozoom::{display_params, MIN_PIXELS_PER_LINE};
use beam::bayer::{channel_image, Channel, ChannelImage, Pattern};
use beam::{analyze, find_region, zoom_window, AxisResult, BeamParams, BeamResult, ImageRef, Window, ZoomMode, ZoomSettings};
use render::{lut, render, render_overview, light_in_dark, Palette, Picture, RgbView};

/// A dark frame is refused when its despeckled maximum is this many counts above its mean: it saw light.
const DARK_MAX_CONTRAST: f64 = 25.0;

#[derive(Clone, Debug)]
pub struct CameraConfig {
    pub width: usize,
    pub height: usize,
    pub bit_depth: u32,
    /// Effective pixel size in µm (sensor pixel × binning).
    pub pixel_w_um: f64,
    pub pixel_h_um: f64,
    /// Bayer pattern and display matrix (white balance then colour correction); `None` for monochrome.
    pub color: Option<(Pattern, [f32; 9])>,
}

impl CameraConfig {
    pub fn full_scale(&self) -> u32 {
        (1u32 << self.bit_depth.min(16)) - 1
    }
}

#[derive(Clone, Debug)]
pub struct Controls {
    pub roi_half: usize,
    pub min_contrast: f64,
    pub min_snr: f64,
    pub zoom: ZoomSettings,
    pub palette: Palette,
    pub channel: Channel,
    pub color_view: bool,
    pub plot_aspect: f64,
}

impl Default for Controls {
    fn default() -> Self {
        Self {
            roi_half: 150,
            min_contrast: 30.0,
            min_snr: 8.0,
            zoom: ZoomSettings::default(),
            palette: Palette::Gray,
            channel: Channel::Sum,
            color_view: true,
            plot_aspect: 4.0 / 3.0,
        }
    }
}

struct DarkRecorder {
    sum: Vec<f64>,
    n: usize,
    target: usize,
}

/// Processing state of one camera session.
pub struct Processor {
    pub config: CameraConfig,
    pub controls: Controls,
    /// The frame being processed, row-major, `width * height`.
    pub frame: Vec<u16>,
    /// Packed USB stream of one frame, written by the page before [`Processor::decode`].
    pub stream: Vec<u8>,
    window: Option<Window>,
    zoomed: bool,
    beam_present: bool,
    dark: Option<Vec<f64>>,
    dark_rec: Option<DarkRecorder>,
    dark_status: String,
    last_channel: Option<Channel>,
    /// Display levels of the last display update: (white level, background, colour scale).
    zmax: f64,
    background: f64,
    rgb_k: Option<f32>,
    zoom_text: String,
    pub zoom_pic: Picture,
    pub full_pic: Picture,
    pub json: String,
    sim: Option<sim::SimCamera>,
}

impl Processor {
    pub fn new(config: CameraConfig) -> Self {
        let n = config.width * config.height;
        Self {
            config,
            controls: Controls::default(),
            frame: vec![0; n],
            stream: Vec::new(),
            window: None,
            zoomed: false,
            beam_present: false,
            dark: None,
            dark_rec: None,
            dark_status: "No dark frame".into(),
            last_channel: None,
            zmax: 1.0,
            background: 0.0,
            rgb_k: None,
            zoom_text: String::new(),
            zoom_pic: Picture::default(),
            full_pic: Picture::default(),
            json: String::new(),
            sim: None,
        }
    }

    /// Decodes [`Processor::stream`] (packet payloads already joined, headers removed) into the frame.
    /// Returns false when the stream holds fewer pixels than the frame.
    pub fn decode(&mut self, calibration_lines: usize) -> bool {
        let skip = calibration_lines * self.config.width;
        raw10::unpack(&self.stream, skip, &mut self.frame) == self.frame.len()
    }

    pub fn simulate(&mut self, t: f64, exposure_ms: f64, gain_db: f64, black_level: f64, blocked: bool) {
        let color = self.config.color.is_some();
        let sim = self.sim.get_or_insert_with(|| sim::SimCamera::new(color));
        sim.render(&mut self.frame, t, exposure_ms, gain_db, black_level, blocked);
    }

    pub fn record_dark(&mut self, frames: usize) {
        self.dark_rec = Some(DarkRecorder { sum: Vec::new(), n: 0, target: frames.max(1) });
        self.dark_status = format!("Recording dark frame 0/{}", frames.max(1));
    }

    pub fn clear_dark(&mut self) {
        self.dark = None;
        self.dark_rec = None;
        self.dark_status = "No dark frame".into();
    }

    /// Analyses the frame. `live`: a new frame (counts for the dark frame); `display`: also update the auto-zoom
    /// window and paint the display images (the page asks for it at most 10 times a second).
    /// Leaves the result in [`Processor::json`].
    pub fn process(&mut self, live: bool, display: bool) {
        let cfg = &self.config;
        let ctl = &self.controls;
        let raw = ImageRef::new(&self.frame, cfg.width, cfg.height);
        let channel_img: Option<ChannelImage> = cfg.color.map(|(pat, _)| channel_image(raw, pat, ctl.channel, cfg.full_scale()));
        let (img, full_scale, unit) = match &channel_img {
            Some(c) => (c.as_ref(), c.full_scale, "cell"),
            None => (raw, cfg.full_scale(), "px"),
        };
        let k = if cfg.color.is_some() { 2.0 } else { 1.0 };
        let (pw, ph) = (k * cfg.pixel_w_um, k * cfg.pixel_h_um);
        let n = img.width * img.height;

        if cfg.color.is_some() && self.last_channel.is_some_and(|c| c != ctl.channel) && (self.dark.is_some() || self.dark_rec.is_some()) {
            self.dark = None;
            self.dark_rec = None;
            self.dark_status = "Dark frame cleared: the colour channel changed".into();
        }
        self.last_channel = Some(ctl.channel);

        // Dark frame: average N live frames recorded with the beam blocked (of the analysed image).
        if let Some(rec) = self.dark_rec.as_mut().filter(|_| live) {
            if rec.sum.len() != n {
                rec.sum = vec![0.0; n];
                rec.n = 0;
            }
            rec.sum.iter_mut().zip(img.data).for_each(|(s, &v)| *s += v as f64);
            rec.n += 1;
            self.dark_status = format!("Recording dark frame {}/{}", rec.n, rec.target);
            if rec.n >= rec.target {
                let k = 1.0 / rec.n as f64;
                let d: Vec<f64> = rec.sum.iter().map(|s| s * k).collect();
                let contrast = light_in_dark(&d, img.width, img.height);
                if contrast > DARK_MAX_CONTRAST {
                    self.dark_status = format!("Dark frame refused: it has light {contrast:.0} counts above its mean. Block the beam and record again.");
                } else {
                    self.dark = Some(d);
                    self.dark_status = format!("Dark frame on ({} frames)", rec.n);
                }
                self.dark_rec = None;
            }
        }

        // Hysteresis: the full contrast to find a beam, 2/3 of it to keep it.
        let min_contrast = if self.beam_present { ctl.min_contrast * 2.0 / 3.0 } else { ctl.min_contrast };
        let params = BeamParams { full_scale, pixel_w_um: pw, pixel_h_um: ph, roi_half: ctl.roi_half, min_contrast, min_snr: ctl.min_snr };
        let dark = self.dark.as_deref().filter(|d| d.len() == n);
        let mut result = analyze(img, &params, dark);
        if let Some(c) = &channel_img {
            result.saturated = result.detected() && c.saturated();
        }
        self.beam_present = result.detected();

        if display {
            let region = find_region(img, ctl.zoom.threshold_pct, MIN_PIXELS_PER_LINE, self.zoomed);
            let prev = self.window.unwrap_or_else(|| Window::full(img.height, img.width));
            let (win, z) = zoom_window(prev, &region, ctl.plot_aspect, &ctl.zoom);
            self.window = Some(win);
            self.zoomed = z;
            let dp = display_params(&win, &region, if cfg.color.is_some() { "cells" } else { "px" });
            self.zmax = dp.zmax;
            self.background = region.background;
            self.rgb_k = None;
            self.zoom_text = dp.text;

            let palette = lut(ctl.palette);
            let rgb = match cfg.color {
                Some((pat, m)) if ctl.color_view => {
                    let per_cell = ctl.channel.full_scale(1) as f32;
                    Some(RgbView { raw, pat, m, black: self.background as f32 / per_cell, sat: cfg.full_scale() })
                }
                _ => None,
            };
            match &rgb {
                Some(v) => {
                    let k = v.scale(&win);
                    self.rgb_k = Some(k);
                    self.zoom_pic = v.render(&win, k);
                    self.full_pic = v.render(&Window::full(img.height, img.width), k);
                }
                None => {
                    let clipped = channel_img.as_ref().map(|c| c.clipped.as_slice());
                    self.zoom_pic = render(img, &win, self.zmax, full_scale, &palette, clipped);
                    self.full_pic = render_overview(img, self.zmax, &palette);
                }
            }
        }
        let win = self.window.unwrap_or_else(|| Window::full(img.height, img.width));
        self.json = result_json(&result, &win, display.then_some(self.zoom_text.as_str()), unit, (pw, ph), (img.width, img.height), full_scale, &self.dark_status);
    }
}

fn num(s: &mut String, v: f64) {
    if v.is_finite() {
        let _ = write!(s, "{v}");
    } else {
        s.push_str("null");
    }
}

fn str_lit(s: &mut String, v: &str) {
    s.push('"');
    for ch in v.chars() {
        match ch {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(s, "\\u{:04x}", c as u32);
            }
            c => s.push(c),
        }
    }
    s.push('"');
}

fn arr(s: &mut String, v: &[f64]) {
    s.push('[');
    for (i, &x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        // Profiles are sums of counts; 3 decimals keep the JSON small without losing anything visible.
        if x.is_finite() {
            let _ = write!(s, "{}", (x * 1000.0).round() / 1000.0);
        } else {
            s.push_str("null");
        }
    }
    s.push(']');
}

fn axis_json(s: &mut String, a: &AxisResult) {
    let f = a.fit;
    s.push_str("{\"x0\":");
    let _ = write!(s, "{}", a.x0);
    for (k, v) in [
        ("center", a.center()),
        ("sigma", a.sigma()),
        ("amplitude", f.map_or(f64::NAN, |f| f.amplitude)),
        ("offset", f.map_or(f64::NAN, |f| f.offset)),
        ("fwhm", f.map_or(f64::NAN, |f| f.fwhm())),
        ("sigma2", a.sigma2),
        ("r2", a.r2),
        ("snr", a.snr),
    ] {
        let _ = write!(s, ",\"{k}\":");
        num(s, v);
    }
    s.push_str(",\"profile\":");
    arr(s, &a.profile);
    s.push_str(",\"fit\":");
    arr(s, &a.fit_curve);
    s.push('}');
}

#[allow(clippy::too_many_arguments)]
fn result_json(r: &BeamResult, win: &Window, zoom_text: Option<&str>, unit: &str, pixel_um: (f64, f64), size: (usize, usize), full_scale: u32, dark_status: &str) -> String {
    let mut s = String::with_capacity(8192);
    s.push_str("{\"detected\":");
    s.push_str(if r.detected() { "true" } else { "false" });
    s.push_str(",\"no_beam\":");
    match &r.no_beam {
        Some(nb) => str_lit(&mut s, &nb.to_string()),
        None => s.push_str("null"),
    }
    for (k, v) in [("dx_um", r.dx_um), ("dy_um", r.dy_um), ("background", r.background), ("contrast", r.contrast)] {
        let _ = write!(s, ",\"{k}\":");
        num(&mut s, v);
    }
    let _ = write!(
        s,
        ",\"peak\":{},\"peak_row\":{},\"peak_col\":{},\"saturated\":{},\"roi\":[{},{},{},{}],\"full_scale\":{full_scale}",
        r.peak, r.peak_row, r.peak_col, r.saturated, r.roi.0, r.roi.1, r.roi.2, r.roi.3
    );
    let _ = write!(s, ",\"window\":[{},{},{},{}],\"size\":[{},{}],\"unit\":", win.row0, win.col0, win.rows, win.cols, size.0, size.1);
    str_lit(&mut s, unit);
    s.push_str(",\"pixel_um\":[");
    num(&mut s, pixel_um.0);
    s.push(',');
    num(&mut s, pixel_um.1);
    s.push_str("],\"zoom_text\":");
    match zoom_text {
        Some(t) => str_lit(&mut s, t),
        None => s.push_str("null"),
    }
    s.push_str(",\"dark_status\":");
    str_lit(&mut s, dark_status);
    s.push_str(",\"x\":");
    axis_json(&mut s, &r.x);
    s.push_str(",\"y\":");
    axis_json(&mut s, &r.y);
    s.push('}');
    s
}

// ---- C ABI for the page ----------------------------------------------------------------------------------------
//
// WebAssembly is single-threaded here (one worker owns the module), so one global processor is enough.

thread_local! {
    static PROC: RefCell<Option<Processor>> = const { RefCell::new(None) };
    static SCRATCH: RefCell<[f32; 9]> = const { RefCell::new([0.0; 9]) };
}

fn with<R>(f: impl FnOnce(&mut Processor) -> R) -> Option<R> {
    PROC.with(|p| p.borrow_mut().as_mut().map(f))
}

/// Allocates `len` bytes for the page (freed with [`bw_free`]).
#[no_mangle]
pub extern "C" fn bw_alloc(len: usize) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(len.max(1));
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

/// # Safety
/// `ptr` and `len` must come from one [`bw_alloc`] call.
#[no_mangle]
pub unsafe extern "C" fn bw_free(ptr: *mut u8, len: usize) {
    drop(Vec::from_raw_parts(ptr, 0, len.max(1)));
}

/// Nine floats the page fills with the display matrix before [`bw_configure`].
#[no_mangle]
pub extern "C" fn bw_matrix_ptr() -> *mut f32 {
    SCRATCH.with(|m| m.as_ptr() as *mut f32)
}

/// Starts a session. `cfa`: -1 monochrome, 0 red (RGGB), 1 blue (BGGR), 2 green left of red (GRBG),
/// 3 green left of blue (GBRG), as `color_filter_phase` / `tl_camera_get_color_filter_array_phase`.
#[no_mangle]
pub extern "C" fn bw_configure(width: u32, height: u32, bit_depth: u32, pixel_w_um: f64, pixel_h_um: f64, cfa: i32) {
    let pat = match cfa {
        0 => Some(Pattern::RGGB),
        1 => Some(Pattern::BGGR),
        2 => Some(Pattern::GRBG),
        3 => Some(Pattern::GBRG),
        _ => None,
    };
    let m = SCRATCH.with(|m| *m.borrow());
    let config = CameraConfig { width: width as usize, height: height as usize, bit_depth, pixel_w_um, pixel_h_um, color: pat.map(|p| (p, m)) };
    PROC.with(|p| *p.borrow_mut() = Some(Processor::new(config)));
}

/// `zoom_mode`: 0 auto, 1 full frame, 2 lock. `palette`: 0 gray, 1 thermal. `channel`: 0 sum, 1 red, 2 green, 3 blue.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn bw_set_controls(
    roi_half: u32,
    min_contrast: f64,
    min_snr: f64,
    zoom_mode: u32,
    threshold_pct: f64,
    max_zoom: f64,
    margin: f64,
    palette: u32,
    channel: u32,
    color_view: u32,
    plot_aspect: f64,
) {
    with(|p| {
        p.controls = Controls {
            roi_half: roi_half as usize,
            min_contrast,
            min_snr,
            zoom: ZoomSettings {
                mode: match zoom_mode {
                    1 => ZoomMode::FullFrame,
                    2 => ZoomMode::Lock,
                    _ => ZoomMode::Auto,
                },
                threshold_pct,
                max_zoom,
                margin,
            },
            palette: if palette == 1 { Palette::Thermal } else { Palette::Gray },
            channel: match channel {
                1 => Channel::Red,
                2 => Channel::Green,
                3 => Channel::Blue,
                _ => Channel::Sum,
            },
            color_view: color_view != 0,
            plot_aspect,
        }
    });
}

/// The frame buffer (`width * height` u16), for frames decoded by the page itself.
#[no_mangle]
pub extern "C" fn bw_frame_ptr() -> *mut u16 {
    with(|p| p.frame.as_mut_ptr()).unwrap_or(std::ptr::null_mut())
}

/// A buffer of `len` bytes for one frame's packed stream (payloads only, in packet order).
#[no_mangle]
pub extern "C" fn bw_stream_ptr(len: usize) -> *mut u8 {
    with(|p| {
        p.stream.resize(len, 0);
        p.stream.as_mut_ptr()
    })
    .unwrap_or(std::ptr::null_mut())
}

/// Decodes the stream into the frame; 1 on success.
#[no_mangle]
pub extern "C" fn bw_decode(calibration_lines: u32) -> u32 {
    with(|p| p.decode(calibration_lines as usize) as u32).unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn bw_simulate(t: f64, exposure_ms: f64, gain_db: f64, black_level: f64, blocked: u32) {
    with(|p| p.simulate(t, exposure_ms, gain_db, black_level, blocked != 0));
}

#[no_mangle]
pub extern "C" fn bw_record_dark(frames: u32) {
    with(|p| p.record_dark(frames as usize));
}

#[no_mangle]
pub extern "C" fn bw_clear_dark() {
    with(|p| p.clear_dark());
}

/// Analyses the frame; returns the length of the JSON result at [`bw_json_ptr`].
#[no_mangle]
pub extern "C" fn bw_process(live: u32, display: u32) -> usize {
    with(|p| {
        p.process(live != 0, display != 0);
        p.json.len()
    })
    .unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn bw_json_ptr() -> *const u8 {
    with(|p| p.json.as_ptr()).unwrap_or(std::ptr::null())
}

/// The zoom crop (0) or the overview (1) as RGBA: pointer, then width and height.
#[no_mangle]
pub extern "C" fn bw_pic_ptr(which: u32) -> *const u8 {
    with(|p| if which == 0 { p.zoom_pic.rgba.as_ptr() } else { p.full_pic.rgba.as_ptr() }).unwrap_or(std::ptr::null())
}

#[no_mangle]
pub extern "C" fn bw_pic_width(which: u32) -> u32 {
    with(|p| if which == 0 { p.zoom_pic.width } else { p.full_pic.width } as u32).unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn bw_pic_height(which: u32) -> u32 {
    with(|p| if which == 0 { p.zoom_pic.height } else { p.full_pic.height } as u32).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam::synth::{render as synth, Spot, SynthOptions};

    fn mono() -> CameraConfig {
        CameraConfig { width: 1440, height: 1080, bit_depth: 10, pixel_w_um: 3.45, pixel_h_um: 3.45, color: None }
    }

    /// The processor gives the same numbers as calling `beam::analyze` directly, from a USB-packed frame.
    #[test]
    fn packed_frame_analyses_like_native() {
        let cfg = mono();
        let spot = Spot { a: 400.0, x0: 700.0, y0: 500.0, sx: 20.0, sy: 12.0 };
        let img = synth(cfg.width, cfg.height, &[spot], &SynthOptions::cs165(), 3);
        let mut stream = raw10::pack(&vec![7u16; 6 * cfg.width]);
        stream.extend(raw10::pack(&img));
        stream.extend(vec![0u8; 3000]); // padding of the last packet
        let mut p = Processor::new(cfg.clone());
        p.stream = stream;
        assert!(p.decode(6));
        assert_eq!(p.frame, img);
        p.process(true, true);
        let params = BeamParams { full_scale: 1023, pixel_w_um: 3.45, pixel_h_um: 3.45, ..Default::default() };
        let native = analyze(ImageRef::new(&img, cfg.width, cfg.height), &params, None);
        assert!(native.detected());
        assert!(p.json.contains(&format!("\"dx_um\":{}", native.dx_um)), "{}", &p.json[..200]);
        assert!(p.json.starts_with("{\"detected\":true"));
        assert!(p.zoom_pic.width > 0 && p.zoom_pic.rgba.len() == p.zoom_pic.width * p.zoom_pic.height * 4);
        assert_eq!(p.full_pic.width, 720);
    }

    #[test]
    fn simulator_colour_frame_is_detected() {
        let mut cfg = mono();
        cfg.color = Some((Pattern::RGGB, [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]));
        let mut p = Processor::new(cfg);
        p.simulate(1.0, 5.0, 0.0, 12.0, false);
        p.process(true, true);
        assert!(p.json.starts_with("{\"detected\":true"), "{}", &p.json[..300]);
        assert!(p.json.contains("\"unit\":\"cell\""));
    }

    #[test]
    fn dark_frame_records_and_clears() {
        let mut p = Processor::new(mono());
        p.record_dark(2);
        for _ in 0..2 {
            p.simulate(0.0, 5.0, 0.0, 12.0, true);
            p.process(true, false);
        }
        assert!(p.dark.is_some(), "{}", p.dark_status);
        p.clear_dark();
        assert!(p.dark.is_none());
    }
}

//! Safe wrapper for the Thorlabs TSI scientific camera SDK (native C API, `thorlabs_tsi_camera_sdk.dll`).
//!
//! * [`Sdk`] opens the SDK once per process (`tl_camera_open_sdk`) and closes it when the last user is gone.
//! * [`Camera`] is one open camera. Dropping it disarms and closes the camera, also after an error or a panic,
//!   so a camera is never left locked (the job of `Camera_Close.vi` in the LabVIEW version).
//!
//! ```no_run
//! let sdk = tlcam::Sdk::open(std::path::Path::new(r"C:\ThorCam\Library_X64"))?;
//! let mut cam = tlcam::Camera::open(&sdk, 0)?;
//! cam.start()?;
//! if let Some(frame) = cam.poll_frame()? {
//!     println!("{} x {} px, frame {}", frame.width, frame.height, frame.frame_count);
//! }
//! # Ok::<(), tlcam::Error>(())
//! ```

#![cfg(windows)]

mod ffi;

use std::ffi::{c_char, c_int, CStr};
use std::path::Path;
use std::sync::{Arc, Mutex};

pub use ffi::Library;

#[derive(Clone, Debug)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// What the rest of the program needs to know about a camera (`CameraInfo.ctl`).
#[derive(Clone, Debug, PartialEq)]
pub struct CameraInfo {
    pub model: String,
    pub serial: String,
    pub firmware: String,
    pub width: usize,
    pub height: usize,
    /// Sensor pixel size in µm, without binning.
    pub pixel_w_um: f64,
    pub pixel_h_um: f64,
    pub bin_x: u32,
    pub bin_y: u32,
    pub bit_depth: u32,
    pub monochrome: bool,
    /// Bayer colour sensors only.
    pub color: Option<ColorInfo>,
    pub exposure_range_us: (i64, i64),
    pub gain_range: (i32, i32),
    pub black_level_range: (i32, i32),
    pub led_supported: bool,
    /// Values read at open, so the panel can start from what the camera has.
    pub initial: Applied,
}

impl CameraInfo {
    /// Pixel size in µm including binning: (width, height).
    pub fn effective_pixel_um(&self) -> (f64, f64) {
        (self.pixel_w_um * self.bin_x.max(1) as f64, self.pixel_h_um * self.bin_y.max(1) as f64)
    }

    pub fn full_scale(&self) -> u32 {
        (1u32 << self.bit_depth.min(16)) - 1
    }
}

/// Which colour the top-left pixel of the sensor's 2 x 2 Bayer pattern has (`TL_COLOR_FILTER_ARRAY_PHASE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CfaPhase {
    /// R G / G B
    Red,
    /// B G / G R
    Blue,
    /// G R / B G
    GreenLeftOfRed,
    /// G B / R G
    GreenLeftOfBlue,
}

impl CfaPhase {
    pub fn from_sdk(v: i32) -> Option<Self> {
        Some(match v {
            0 => Self::Red,
            1 => Self::Blue,
            2 => Self::GreenLeftOfRed,
            3 => Self::GreenLeftOfBlue,
            _ => return None,
        })
    }

    /// Pattern name, top-left first, row by row.
    pub fn name(self) -> &'static str {
        match self {
            Self::Red => "RGGB",
            Self::Blue => "BGGR",
            Self::GreenLeftOfRed => "GRBG",
            Self::GreenLeftOfBlue => "GBRG",
        }
    }
}

/// Colour data of a Bayer sensor, as the SDK reports it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorInfo {
    pub phase: CfaPhase,
    /// 3 x 3, row-major: camera RGB (after white balance) to output RGB.
    pub color_correction: [f32; 9],
    /// 3 x 3, row-major, normally diagonal.
    pub white_balance: [f32; 9],
}

impl ColorInfo {
    /// The matrix that turns camera R, G, B into display RGB: colour correction × white balance.
    pub fn display_matrix(&self) -> [f32; 9] {
        let (a, b) = (&self.color_correction, &self.white_balance);
        std::array::from_fn(|i| {
            let (r, c) = (i / 3, i % 3);
            (0..3).map(|k| a[r * 3 + k] * b[k * 3 + c]).sum()
        })
    }
}

pub const IDENTITY3: [f32; 9] = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];

/// Settings requested by the user (`Settings.ctl`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraSettings {
    pub exposure_ms: f64,
    /// Gain index (not dB).
    pub gain: i32,
    /// `None` keeps the camera's value.
    pub black_level: Option<i32>,
    pub led_on: bool,
}

/// What the camera actually accepted, read back after writing.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Applied {
    pub exposure_ms: f64,
    pub gain: i32,
    pub gain_db: f64,
    pub black_level: i32,
    pub led_on: bool,
}

/// One monochrome frame, row-major (`data[row * width + col]`).
#[derive(Clone, Debug)]
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u16>,
    pub frame_count: u64,
}

/// The opened SDK. There is one per process; [`Sdk::open`] returns the existing one while it is alive.
pub struct Sdk {
    lib: Library,
}

static CURRENT: Mutex<std::sync::Weak<Sdk>> = Mutex::new(std::sync::Weak::new());

impl Sdk {
    /// Loads the DLL from `dll_dir` and opens the SDK, or returns the SDK that is already open.
    pub fn open(dll_dir: &Path) -> Result<Arc<Sdk>> {
        let mut cur = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(sdk) = cur.upgrade() {
            return Ok(sdk);
        }
        let lib = Library::load(dll_dir).map_err(Error)?;
        let rc = unsafe { (lib.api.open_sdk)() };
        if rc != 0 {
            return Err(Error(format!(
                "tl_camera_open_sdk failed (code {rc}): {}. If the SDK is already open in another program \
                 (ThorCam, LabVIEW), close that program first.",
                lib.last_error()
            )));
        }
        let sdk = Arc::new(Sdk { lib });
        *cur = Arc::downgrade(&sdk);
        Ok(sdk)
    }

    fn check(&self, call: &str, rc: c_int) -> Result<()> {
        if rc == 0 {
            Ok(())
        } else {
            Err(Error(format!("{call} failed (code {rc}): {}", self.lib.last_error())))
        }
    }

    /// Serial numbers of the connected cameras.
    pub fn discover(&self) -> Result<Vec<String>> {
        let mut buf = vec![0 as c_char; 4096];
        let rc = unsafe { (self.lib.api.discover_available_cameras)(buf.as_mut_ptr(), buf.len() as c_int) };
        self.check("tl_camera_discover_available_cameras", rc)?;
        let s = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy();
        Ok(s.split_whitespace().map(str::to_owned).collect())
    }
}

impl Drop for Sdk {
    fn drop(&mut self) {
        unsafe {
            (self.lib.api.close_sdk)();
        }
    }
}

/// One open camera. Drop disarms and closes it.
pub struct Camera {
    sdk: Arc<Sdk>,
    h: ffi::Handle,
    armed: bool,
    info: CameraInfo,
}

// A camera handle is used by one thread at a time (`&mut self`), which the SDK allows.
unsafe impl Send for Camera {}

/// How long `poll_frame` waits inside the SDK for a frame, in ms. Short, so stop requests are seen quickly.
const POLL_TIMEOUT_MS: c_int = 20;

impl Camera {
    /// Opens the camera with this index in the discovery list (`Camera_Open.vi`).
    pub fn open(sdk: &Arc<Sdk>, index: usize) -> Result<Camera> {
        let serials = sdk.discover()?;
        let serial = serials
            .get(index)
            .ok_or_else(|| Error(if serials.is_empty() { "No camera found.".into() } else { format!("No camera with index {index}; found {}.", serials.len()) }))?;
        Self::open_serial(sdk, serial)
    }

    pub fn open_serial(sdk: &Arc<Sdk>, serial: &str) -> Result<Camera> {
        let mut cs: Vec<c_char> = serial.bytes().map(|b| b as c_char).collect();
        cs.push(0);
        let mut h: ffi::Handle = std::ptr::null_mut();
        let rc = unsafe { (sdk.lib.api.open_camera)(cs.as_mut_ptr(), &mut h) };
        sdk.check("tl_camera_open_camera", rc)?;
        if h.is_null() {
            return Err(Error(format!("tl_camera_open_camera returned no handle for {serial}")));
        }
        // From here on, Drop closes the camera, also if reading the info fails.
        let mut cam = Camera { sdk: sdk.clone(), h, armed: false, info: placeholder_info() };
        cam.info = cam.read_info()?;
        Ok(cam)
    }

    pub fn info(&self) -> &CameraInfo {
        &self.info
    }

    fn api(&self) -> &ffi::Api {
        &self.sdk.lib.api
    }

    fn get_int(&self, call: &str, f: unsafe extern "C" fn(ffi::Handle, *mut c_int) -> c_int) -> Result<i32> {
        let mut v: c_int = 0;
        self.sdk.check(call, unsafe { f(self.h, &mut v) })?;
        Ok(v)
    }

    fn get_range(&self, call: &str, f: unsafe extern "C" fn(ffi::Handle, *mut c_int, *mut c_int) -> c_int) -> Result<(i32, i32)> {
        let (mut a, mut b): (c_int, c_int) = (0, 0);
        self.sdk.check(call, unsafe { f(self.h, &mut a, &mut b) })?;
        Ok((a, b))
    }

    fn get_f64(&self, call: &str, f: unsafe extern "C" fn(ffi::Handle, *mut f64) -> c_int) -> Result<f64> {
        let mut v = 0.0;
        self.sdk.check(call, unsafe { f(self.h, &mut v) })?;
        Ok(v)
    }

    fn get_string(&self, call: &str, f: unsafe extern "C" fn(ffi::Handle, *mut c_char, c_int) -> c_int) -> Result<String> {
        let mut buf = vec![0 as c_char; 256];
        self.sdk.check(call, unsafe { f(self.h, buf.as_mut_ptr(), buf.len() as c_int) })?;
        Ok(unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().trim().to_owned())
    }

    fn read_info(&self) -> Result<CameraInfo> {
        let a = self.api();
        let mut exp = (0i64, 0i64);
        self.sdk.check("tl_camera_get_exposure_time_range", unsafe { (a.get_exposure_time_range)(self.h, &mut exp.0, &mut exp.1) })?;
        let led_supported = self.get_int("tl_camera_get_is_led_supported", a.get_is_led_supported).unwrap_or(0) != 0;
        let sensor = self.get_int("tl_camera_get_camera_sensor_type", a.get_camera_sensor_type).unwrap_or(ffi::SENSOR_TYPE_MONOCHROME);
        let color = if sensor == ffi::SENSOR_TYPE_BAYER { Some(self.read_color()?) } else { None };
        Ok(CameraInfo {
            model: self.get_string("tl_camera_get_model", a.get_model)?,
            serial: self.get_string("tl_camera_get_serial_number", a.get_serial_number)?,
            firmware: self.get_string("tl_camera_get_firmware_version", a.get_firmware_version).unwrap_or_default(),
            width: self.get_int("tl_camera_get_image_width", a.get_image_width)? as usize,
            height: self.get_int("tl_camera_get_image_height", a.get_image_height)? as usize,
            pixel_w_um: self.get_f64("tl_camera_get_sensor_pixel_width", a.get_sensor_pixel_width)?,
            pixel_h_um: self.get_f64("tl_camera_get_sensor_pixel_height", a.get_sensor_pixel_height)?,
            bin_x: self.get_int("tl_camera_get_binx", a.get_binx).unwrap_or(1).max(1) as u32,
            bin_y: self.get_int("tl_camera_get_biny", a.get_biny).unwrap_or(1).max(1) as u32,
            bit_depth: self.get_int("tl_camera_get_bit_depth", a.get_bit_depth)? as u32,
            monochrome: sensor == ffi::SENSOR_TYPE_MONOCHROME,
            color,
            exposure_range_us: exp,
            gain_range: self.get_range("tl_camera_get_gain_range", a.get_gain_range).unwrap_or((0, 0)),
            black_level_range: self.get_range("tl_camera_get_black_level_range", a.get_black_level_range).unwrap_or((0, 0)),
            led_supported,
            initial: self.read_back(led_supported)?,
        })
    }

    fn read_color(&self) -> Result<ColorInfo> {
        let a = self.api();
        let phase = self.get_int("tl_camera_get_color_filter_array_phase", a.get_color_filter_array_phase)?;
        let phase = CfaPhase::from_sdk(phase).ok_or_else(|| Error(format!("unknown colour filter phase {phase}")))?;
        // Without the matrices the image is still correct for analysis; only the display colours are off.
        let mut ccm = IDENTITY3;
        if unsafe { (a.get_color_correction_matrix)(self.h, ccm.as_mut_ptr()) } != 0 {
            ccm = IDENTITY3;
        }
        let mut wb = IDENTITY3;
        if unsafe { (a.get_default_white_balance_matrix)(self.h, wb.as_mut_ptr()) } != 0 {
            wb = IDENTITY3;
        }
        Ok(ColorInfo { phase, color_correction: ccm, white_balance: wb })
    }

    fn read_back(&self, led_supported: bool) -> Result<Applied> {
        let a = self.api();
        let mut exp_us = 0i64;
        self.sdk.check("tl_camera_get_exposure_time", unsafe { (a.get_exposure_time)(self.h, &mut exp_us) })?;
        let gain = self.get_int("tl_camera_get_gain", a.get_gain).unwrap_or(0);
        let mut gain_db = 0.0;
        if unsafe { (a.convert_gain_to_decibels)(self.h, gain, &mut gain_db) } != 0 {
            gain_db = f64::NAN;
        }
        Ok(Applied {
            exposure_ms: exp_us as f64 / 1000.0,
            gain,
            gain_db,
            black_level: self.get_int("tl_camera_get_black_level", a.get_black_level).unwrap_or(0),
            led_on: led_supported && self.get_int("tl_camera_get_is_led_on", a.get_is_led_on).unwrap_or(0) != 0,
        })
    }

    /// Free-running acquisition (`Camera_Start.vi`): hot-pixel correction on, software-triggered mode with
    /// unlimited frames per trigger, arm, then one trigger.
    pub fn start(&mut self) -> Result<()> {
        // Borrow only the `sdk` field, so `self.armed` can be set below.
        let a = &self.sdk.lib.api;
        // Not every camera has hot-pixel correction; ignore its error.
        unsafe { (a.set_is_hot_pixel_correction_enabled)(self.h, 1) };
        self.sdk.check("tl_camera_set_operation_mode", unsafe { (a.set_operation_mode)(self.h, ffi::OPERATION_MODE_SOFTWARE_TRIGGERED) })?;
        self.sdk.check("tl_camera_set_frames_per_trigger_zero_for_unlimited", unsafe { (a.set_frames_per_trigger_zero_for_unlimited)(self.h, 0) })?;
        self.sdk.check("tl_camera_set_image_poll_timeout", unsafe { (a.set_image_poll_timeout)(self.h, POLL_TIMEOUT_MS) })?;
        self.sdk.check("tl_camera_arm", unsafe { (a.arm)(self.h, 2) })?;
        self.armed = true;
        self.sdk.check("tl_camera_issue_software_trigger", unsafe { (a.issue_software_trigger)(self.h) })
    }

    pub fn stop(&mut self) -> Result<()> {
        if !self.armed {
            return Ok(());
        }
        self.armed = false;
        let rc = unsafe { (self.api().disarm)(self.h) };
        self.sdk.check("tl_camera_disarm", rc)
    }

    /// Clamps each value to the camera's range, writes it and reads back what the camera accepted
    /// (`Camera_ApplySettings.vi`). If the camera refuses a change while armed, it is disarmed, written and
    /// re-armed.
    pub fn apply(&mut self, s: &CameraSettings) -> Result<Applied> {
        if let Err(e) = self.write_settings(s) {
            if !self.armed {
                return Err(e);
            }
            self.stop()?;
            let r = self.write_settings(s);
            self.start()?;
            r?;
        }
        self.read_back(self.info.led_supported)
    }

    fn write_settings(&self, s: &CameraSettings) -> Result<()> {
        let a = self.api();
        let i = &self.info;
        let exp_us = ((s.exposure_ms * 1000.0).round() as i64).clamp(i.exposure_range_us.0, i.exposure_range_us.1.max(i.exposure_range_us.0));
        self.sdk.check("tl_camera_set_exposure_time", unsafe { (a.set_exposure_time)(self.h, exp_us) })?;
        if i.gain_range.1 > i.gain_range.0 {
            let g = s.gain.clamp(i.gain_range.0, i.gain_range.1);
            self.sdk.check("tl_camera_set_gain", unsafe { (a.set_gain)(self.h, g) })?;
        }
        if let Some(b) = s.black_level {
            if i.black_level_range.1 > i.black_level_range.0 {
                let b = b.clamp(i.black_level_range.0, i.black_level_range.1);
                self.sdk.check("tl_camera_set_black_level", unsafe { (a.set_black_level)(self.h, b) })?;
            }
        }
        if i.led_supported {
            self.sdk.check("tl_camera_set_is_led_on", unsafe { (a.set_is_led_on)(self.h, s.led_on as c_int) })?;
        }
        Ok(())
    }

    /// Waits up to 20 ms for the next frame and copies it (`Camera_GetFrame.vi`). `Ok(None)` when there is none.
    pub fn poll_frame(&mut self) -> Result<Option<Frame>> {
        let mut buf: *mut u16 = std::ptr::null_mut();
        let mut count: c_int = 0;
        let mut meta: *mut u8 = std::ptr::null_mut();
        let mut meta_len: c_int = 0;
        let rc = unsafe { (self.api().get_pending_frame_or_null)(self.h, &mut buf, &mut count, &mut meta, &mut meta_len) };
        self.sdk.check("tl_camera_get_pending_frame_or_null", rc)?;
        if buf.is_null() {
            return Ok(None);
        }
        let (w, h) = (self.info.width, self.info.height);
        // The buffer belongs to the SDK and is valid until the next call; copy it now.
        let data = unsafe { std::slice::from_raw_parts(buf, w * h) }.to_vec();
        Ok(Some(Frame { width: w, height: h, data, frame_count: count.max(0) as u64 }))
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        // Every step runs, whatever failed before: a skipped disarm or close leaves the camera locked.
        unsafe {
            if self.armed {
                (self.api().disarm)(self.h);
            }
            (self.api().close_camera)(self.h);
        }
    }
}

fn placeholder_info() -> CameraInfo {
    CameraInfo {
        model: String::new(),
        serial: String::new(),
        firmware: String::new(),
        width: 0,
        height: 0,
        pixel_w_um: 0.0,
        pixel_h_um: 0.0,
        bin_x: 1,
        bin_y: 1,
        bit_depth: 0,
        monochrome: true,
        color: None,
        exposure_range_us: (0, 0),
        gain_range: (0, 0),
        black_level_range: (0, 0),
        led_supported: false,
        initial: Applied::default(),
    }
}

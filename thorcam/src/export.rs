//! Saving what the profiler sees: PNG pictures, raw 16-bit TIFF frames, CSV results, and recordings.
//!
//! * Pictures ([`save_png`]) are what the display shows: palette or colour, saturated pixels marked.
//! * Raw frames ([`save_raw_tiff`]) are the camera's counts, 16-bit, untouched (the Bayer mosaic on a colour
//!   camera). The TIFF carries an ImageJ header with the pixel size in µm, so ImageJ / Fiji opens it calibrated.
//! * A [`Recorder`] writes a video on its own thread: MJPEG AVI of the display, or a raw 16-bit TIFF stack
//!   (BigTIFF, no size limit). Each recording also gets `<name>_frames.csv` with the results of every frame it
//!   holds. JPEG encoding runs on a few threads in parallel; the writer puts the frames back in order. When the
//!   recorder cannot keep up, frames are dropped and counted, never queued without bound.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use eframe::egui::ColorImage;
use tiff::encoder::{colortype, Rational, TiffEncoder, TiffKind};
use tiff::tags::{ResolutionUnit, Tag};
use tlcam::{Applied, CameraInfo};

use crate::pipeline::Analysis;

/// Local time for file names, e.g. `20261012_143205`.
pub fn timestamp() -> String {
    chrono::Local::now().format("%Y%m%d_%H%M%S").to_string()
}

/// `dir/prefix_<time>`, with `_2`, `_3`, ... appended when a file of that name (any extension) exists.
pub fn unique_stem(dir: &Path, prefix: &str) -> PathBuf {
    let base = format!("{prefix}_{}", timestamp());
    let taken = |stem: &str| {
        let (dot, under) = (format!("{stem}."), format!("{stem}_"));
        std::fs::read_dir(dir).is_ok_and(|mut it| {
            it.any(|e| e.is_ok_and(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.starts_with(&dot) || name.starts_with(&under)
            }))
        })
    };
    if !taken(&base) {
        return dir.join(base);
    }
    (2..).map(|i| format!("{base}-{i}")).find(|s| !taken(s)).map(|s| dir.join(s)).unwrap_or_else(|| dir.join(base))
}

/// `stem` + `suffix`, e.g. `snap_20261012_143205` + `_raw.tif`.
pub fn with_suffix(stem: &Path, suffix: &str) -> PathBuf {
    let mut s = stem.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

fn create(path: &Path) -> Result<BufWriter<File>, String> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("Cannot create folder {}: {e}", dir.display()))?;
    }
    File::create(path).map(BufWriter::new).map_err(|e| format!("Cannot create {}: {e}", path.display()))
}

fn rgb_bytes(img: &ColorImage) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(img.pixels.len() * 3);
    for c in &img.pixels {
        let [r, g, b, _] = c.to_srgba_unmultiplied();
        rgb.extend_from_slice(&[r, g, b]);
    }
    rgb
}

pub fn save_png(path: &Path, img: &ColorImage) -> Result<(), String> {
    let [w, h] = img.size;
    let mut out = create(path)?;
    image::ImageEncoder::write_image(image::codecs::png::PngEncoder::new(&mut out), &rgb_bytes(img), w as u32, h as u32, image::ExtendedColorType::Rgb8)
        .and_then(|_| out.flush().map_err(image::ImageError::IoError))
        .map_err(|e| format!("Cannot write {}: {e}", path.display()))
}

/// What a raw TIFF says about itself.
#[derive(Clone, Debug)]
pub struct RawMeta {
    /// Size of one raw sensor pixel including binning, µm: (width, height).
    pub pixel_um: (f64, f64),
    /// Largest count, e.g. 1023: ImageJ's display range, so a 10-bit frame does not look black.
    pub full_scale: u32,
    pub model: String,
    pub serial: String,
}

impl RawMeta {
    pub fn new(info: &CameraInfo) -> Self {
        Self { pixel_um: info.effective_pixel_um(), full_scale: info.full_scale(), model: info.model.clone(), serial: info.serial.clone() }
    }
}

fn pixels_per_um(um: f64) -> Rational {
    // XResolution is pixels per unit; with ImageJ's "unit=micron" that is pixels per µm.
    let d = (um * 1e6).round().clamp(1.0, u32::MAX as f64) as u32;
    Rational { n: 1_000_000, d }
}

fn write_tiff_page<W: Write + Seek, K: TiffKind>(
    enc: &mut TiffEncoder<W, K>,
    w: usize,
    h: usize,
    data: &[u16],
    meta: &RawMeta,
) -> tiff::TiffResult<()> {
    let mut page = enc.new_image::<colortype::Gray16>(w as u32, h as u32)?;
    page.resolution(ResolutionUnit::None, pixels_per_um(meta.pixel_um.0));
    page.y_resolution(pixels_per_um(meta.pixel_um.1));
    let desc = format!("ImageJ=1.54f\nunit=micron\nmin=0\nmax={}\n", meta.full_scale);
    page.encoder().write_tag(Tag::ImageDescription, desc.as_str())?;
    page.encoder().write_tag(Tag::Make, "Thorlabs")?;
    page.encoder().write_tag(Tag::Model, format!("{} {}", meta.model, meta.serial).as_str())?;
    page.encoder().write_tag(Tag::Software, concat!("ThorCam Beam Profiler ", env!("CARGO_PKG_VERSION")))?;
    page.encoder().write_tag(Tag::DateTime, chrono::Local::now().format("%Y:%m:%d %H:%M:%S").to_string().as_str())?;
    page.write_data(data)
}

pub fn save_raw_tiff(path: &Path, w: usize, h: usize, data: &[u16], meta: &RawMeta) -> Result<(), String> {
    let mut out = create(path)?;
    let err = |e: &dyn std::fmt::Display| format!("Cannot write {}: {e}", path.display());
    {
        let mut enc = TiffEncoder::new(&mut out).map_err(|e| err(&e))?;
        write_tiff_page(&mut enc, w, h, data, meta).map_err(|e| err(&e))?;
    }
    out.flush().map_err(|e| err(&e))
}

// ----- CSV -----

/// Header of the per-frame CSV (the log, and the `_frames.csv` of a recording).
pub fn csv_header(unit: &str) -> String {
    let u = unit;
    format!("t_s,frame,beam,sigma_x_{u},sigma_y_{u},sigma2_x_{u},sigma2_y_{u},Dx_um,Dy_um,center_x_{u},center_y_{u},peak,saturated")
}

pub fn csv_row(t: f64, a: &Analysis) -> String {
    let r = &a.result;
    format!(
        "{:.3},{},{},{:.4},{:.4},{:.4},{:.4},{:.3},{:.3},{:.3},{:.3},{},{}",
        t,
        a.frame_count,
        r.detected() as u8,
        r.x.sigma(),
        r.y.sigma(),
        r.x.sigma2,
        r.y.sigma2,
        r.dx_um,
        r.dy_um,
        r.x.center(),
        r.y.center(),
        r.peak,
        r.saturated as u8
    )
}

/// Which profile(s) a profile export holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profiles {
    X,
    Y,
    Both,
    /// The results only.
    None,
}

/// Camera context for a results table.
pub struct ResultsMeta<'a> {
    pub info: Option<&'a CameraInfo>,
    pub applied: Option<Applied>,
    /// "px", or "cell" on a colour camera.
    pub unit: &'a str,
    /// Size of one analysis pixel in µm: (width, height).
    pub pixel_um: (f64, f64),
}

/// The results of one frame and its profiles as a table, separated by `sep` (`,` for CSV, tab for the
/// clipboard): a header block, one row per quantity, then the profiles in long form.
pub fn results_table(a: &Analysis, m: &ResultsMeta, which: Profiles, sep: char) -> String {
    let r = &a.result;
    let (u, (pw, ph)) = (m.unit, m.pixel_um);
    let f = |v: f64, d: usize| if v.is_finite() { format!("{v:.d$}") } else { String::new() };
    let mut s = String::new();
    let mut row = |cells: &[String]| {
        let _ = writeln!(s, "{}", cells.join(&sep.to_string()));
    };
    let c = |v: &str| v.to_owned();
    row(&[c("# ThorCam Beam Profiler"), chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()]);
    if let Some(i) = m.info {
        row(&[c("# camera"), i.model.clone(), c("serial"), i.serial.clone(), c("bit_depth"), i.bit_depth.to_string()]);
    }
    if let Some(ap) = m.applied {
        row(&[c("# exposure_ms"), f(ap.exposure_ms, 3), c("gain"), ap.gain.to_string(), c("gain_dB"), f(ap.gain_db, 2), c("black_level"), ap.black_level.to_string()]);
    }
    row(&[c("# analysis_pixel"), c(u), c("width_um"), f(pw, 4), c("height_um"), f(ph, 4)]);
    row(&[c("# frame"), a.frame_count.to_string(), c("beam_detected"), (r.detected() as u8).to_string(), c("reason"), r.no_beam.as_ref().map_or(String::new(), |n| n.to_string().replace(sep, ";"))]);
    row(&[]);
    row(&[c("quantity"), c("x"), c("y"), c("unit")]);
    let (fx, fy) = (r.x.fit.unwrap_or_default(), r.y.fit.unwrap_or_default());
    let (sx, sy) = (r.x.sigma(), r.y.sigma());
    row(&[c("D_1/e2 (4 sigma)"), f(r.dx_um, 3), f(r.dy_um, 3), c("um")]);
    row(&[c("w_1/e2 radius (2 sigma)"), f(r.dx_um / 2.0, 3), f(r.dy_um / 2.0, 3), c("um")]);
    row(&[c("FWHM (2.3548 sigma)"), f(2.354_820_045 * sx * pw, 3), f(2.354_820_045 * sy * ph, 3), c("um")]);
    row(&[c("sigma (fit)"), f(sx, 4), f(sy, 4), c(u)]);
    row(&[c("sigma (2nd moment)"), f(r.x.sigma2, 4), f(r.y.sigma2, 4), c(u)]);
    row(&[c("center"), f(r.x.center(), 3), f(r.y.center(), 3), c(u)]);
    row(&[c("fit amplitude A"), f(if r.x.fit.is_some() { fx.amplitude } else { f64::NAN }, 3), f(if r.y.fit.is_some() { fy.amplitude } else { f64::NAN }, 3), c("counts (summed)")]);
    row(&[c("fit offset B"), f(if r.x.fit.is_some() { fx.offset } else { f64::NAN }, 3), f(if r.y.fit.is_some() { fy.offset } else { f64::NAN }, 3), c("counts (summed)")]);
    row(&[c("fit S/N"), f(r.x.snr, 2), f(r.y.snr, 2), c("")]);
    row(&[c("fit R^2"), f(r.x.r2, 5), f(r.y.r2, 5), c("")]);
    row(&[c("peak"), r.peak.to_string(), c(""), c("counts")]);
    row(&[c("saturated"), (r.saturated as u8).to_string(), c(""), c("")]);
    row(&[c("background (darkest corner)"), f(r.background, 2), c(""), c("counts")]);
    if which == Profiles::None {
        return s;
    }
    row(&[]);
    row(&[c("axis"), c("index"), format!("position_{u}"), c("position_um"), c("data"), c("fit")]);
    let mut axis = |name: &str, ax: &beam::AxisResult, p_um: f64| {
        for (i, &v) in ax.profile.iter().enumerate() {
            let x = ax.x0 + i;
            let fit = ax.fit_curve.get(i).map_or(String::new(), |v| f(*v, 3));
            row(&[c(name), i.to_string(), x.to_string(), f(x as f64 * p_um, 3), f(v, 3), fit]);
        }
    };
    if matches!(which, Profiles::X | Profiles::Both) {
        axis("X", &r.x, pw);
    }
    if matches!(which, Profiles::Y | Profiles::Both) {
        axis("Y", &r.y, ph);
    }
    s
}

pub fn save_text(path: &Path, text: &str) -> Result<(), String> {
    let mut out = create(path)?;
    out.write_all(text.as_bytes()).and_then(|_| out.flush()).map_err(|e| format!("Cannot write {}: {e}", path.display()))
}

// ----- recording -----

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum VideoFormat {
    /// MJPEG AVI of the display (palette / colour, as shown). Plays in VLC, Windows Media Player, ffmpeg.
    #[default]
    Avi,
    /// Raw 16-bit counts of every frame, one TIFF page each (BigTIFF).
    TiffStack,
}

impl VideoFormat {
    pub const ALL: [VideoFormat; 2] = [VideoFormat::Avi, VideoFormat::TiffStack];

    pub fn name(self) -> &'static str {
        match self {
            VideoFormat::Avi => "AVI (MJPEG)",
            VideoFormat::TiffStack => "TIFF stack (raw)",
        }
    }

    pub fn tooltip(self) -> &'static str {
        match self {
            VideoFormat::Avi => "MJPEG video of the full frame as displayed (palette or colour). Plays in VLC, Windows Media Player, ffmpeg",
            VideoFormat::TiffStack => "Raw 16-bit counts of every frame, one TIFF page each (BigTIFF). Opens as a stack in ImageJ / Fiji",
        }
    }

    pub fn ext(self) -> &'static str {
        match self {
            VideoFormat::Avi => "avi",
            VideoFormat::TiffStack => "tif",
        }
    }
}

/// A recording the UI asks for. A new `id` is a new recording.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordSpec {
    pub id: u64,
    /// Path without extension; the video is `<stem>.avi` or `<stem>.tif`, the results `<stem>_frames.csv`.
    pub stem: PathBuf,
    pub format: VideoFormat,
}

#[derive(Clone, Debug, Default)]
pub struct RecStats {
    pub id: u64,
    /// The file being written (AVI files roll over to `_002.avi` ... before 2 GB).
    pub path: PathBuf,
    pub frames: u64,
    pub dropped: u64,
    pub bytes: u64,
    /// Recording time without pauses, s.
    pub secs: f64,
    pub error: Option<String>,
    pub finished: bool,
}

pub enum RecImage {
    /// The display picture (AVI).
    Color(ColorImage),
    /// Raw counts (TIFF stack).
    Gray16 { w: usize, h: usize, data: Vec<u16> },
    /// A display picture after encoding (inside the recorder).
    Jpeg { w: usize, h: usize, data: Vec<u8> },
    /// Encoding failed (inside the recorder).
    Failed(String),
}

struct RecFrame {
    /// Order of the frame; the encoders may finish out of order.
    seq: u64,
    image: RecImage,
    t: f64,
    csv: String,
}

/// Frames in the recorder (waiting, being encoded, waiting to be written); one more is dropped.
const REC_QUEUE: usize = 12;

pub fn encode_jpeg(img: &ColorImage) -> Result<Vec<u8>, String> {
    let [w, h] = img.size;
    let mut jpeg = Vec::with_capacity(w * h / 4);
    image::ImageEncoder::write_image(
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90),
        &rgb_bytes(img),
        w as u32,
        h as u32,
        image::ExtendedColorType::Rgb8,
    )
    .map_err(|e| format!("JPEG encoding failed: {e}"))?;
    Ok(jpeg)
}

fn encoder(rx: &Mutex<Receiver<RecFrame>>, tx: Sender<RecFrame>) {
    loop {
        let next = lock(rx).recv();
        let Ok(mut f) = next else { return };
        if let RecImage::Color(img) = &f.image {
            let [w, h] = img.size;
            f.image = match encode_jpeg(img) {
                Ok(data) => RecImage::Jpeg { w, h, data },
                Err(e) => RecImage::Failed(e),
            };
        }
        if tx.send(f).is_err() {
            return;
        }
    }
}

pub struct Recorder {
    pub spec: RecordSpec,
    tx: Option<SyncSender<RecFrame>>,
    /// Next frame number; locked while sending, so the numbers have no gaps.
    seq: Mutex<u64>,
    encoders: Vec<JoinHandle<()>>,
    thread: Option<JoinHandle<()>>,
    stats: Arc<Mutex<RecStats>>,
    queued: Arc<AtomicUsize>,
    start: Instant,
    paused_since: Option<Instant>,
    paused_total: Duration,
}

impl Recorder {
    pub fn start(spec: RecordSpec, unit: &str, meta: RawMeta) -> Self {
        let (tx, rx) = mpsc::sync_channel(REC_QUEUE);
        let stats = Arc::new(Mutex::new(RecStats { id: spec.id, path: with_suffix(&spec.stem, &format!(".{}", spec.format.ext())), ..Default::default() }));
        let queued = Arc::new(AtomicUsize::new(0));
        let header = format!("video_frame,{}", csv_header(unit));
        let (enc_tx, enc_rx) = mpsc::channel();
        let rx = Arc::new(Mutex::new(rx));
        let n = match spec.format {
            VideoFormat::Avi => std::thread::available_parallelism().map_or(2, |n| n.get().saturating_sub(2)).clamp(1, 4),
            VideoFormat::TiffStack => 1,
        };
        let encoders = (0..n)
            .map(|i| {
                let (rx, tx) = (rx.clone(), enc_tx.clone());
                std::thread::Builder::new().name(format!("encoder {i}")).spawn(move || encoder(&rx, tx)).expect("spawn encoder thread")
            })
            .collect();
        drop(enc_tx);
        let thread = {
            let (spec, stats, queued) = (spec.clone(), stats.clone(), queued.clone());
            std::thread::Builder::new()
                .name("recorder".into())
                .spawn(move || writer(spec, enc_rx, &stats, &queued, header, meta))
                .expect("spawn recorder thread")
        };
        Self {
            spec,
            tx: Some(tx),
            seq: Mutex::new(0),
            encoders,
            thread: Some(thread),
            stats,
            queued,
            start: Instant::now(),
            paused_since: None,
            paused_total: Duration::ZERO,
        }
    }

    /// While paused the recording clock stands still, so the video's frame rate stays right.
    pub fn set_paused(&mut self, paused: bool) {
        match (paused, self.paused_since) {
            (true, None) => self.paused_since = Some(Instant::now()),
            (false, Some(t)) => {
                self.paused_total += t.elapsed();
                self.paused_since = None;
            }
            _ => {}
        }
    }

    /// Recording time without pauses, s.
    pub fn time(&self) -> f64 {
        let now = self.paused_since.unwrap_or_else(Instant::now);
        now.duration_since(self.start).saturating_sub(self.paused_total).as_secs_f64()
    }

    /// False when the writer is busy: the caller should not even prepare a frame. Counts the drop.
    pub fn has_room(&self) -> bool {
        let room = self.queued.load(Ordering::SeqCst) < REC_QUEUE;
        if !room {
            lock(&self.stats).dropped += 1;
        }
        room
    }

    pub fn push(&self, image: RecImage, csv: String) {
        let Some(tx) = &self.tx else { return };
        let mut seq = lock(&self.seq);
        self.queued.fetch_add(1, Ordering::SeqCst);
        if tx.try_send(RecFrame { seq: *seq, image, t: self.time(), csv }).is_ok() {
            *seq += 1;
        } else {
            self.queued.fetch_sub(1, Ordering::SeqCst);
            lock(&self.stats).dropped += 1;
        }
    }

    pub fn failed(&self) -> bool {
        lock(&self.stats).error.is_some()
    }

    pub fn stats(&self) -> RecStats {
        let mut s = lock(&self.stats).clone();
        s.secs = self.time();
        s
    }

    /// Writes what is queued, closes the files, and returns the final numbers.
    pub fn finish(mut self) -> RecStats {
        let secs = self.time();
        self.close();
        let mut s = lock(&self.stats).clone();
        s.secs = secs;
        s
    }

    fn close(&mut self) {
        self.tx = None;
        for t in self.encoders.drain(..) {
            let _ = t.join();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.close();
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

enum Sink {
    Avi(AviWriter),
    Tiff { enc: TiffEncoder<BufWriter<File>, tiff::encoder::TiffKindBig>, path: PathBuf, bytes: u64 },
}

impl Sink {
    fn open(spec: &RecordSpec) -> Result<Sink, String> {
        let path = with_suffix(&spec.stem, &format!(".{}", spec.format.ext()));
        Ok(match spec.format {
            VideoFormat::Avi => Sink::Avi(AviWriter::create(spec.stem.clone())?),
            VideoFormat::TiffStack => {
                let out = create(&path)?;
                let enc = TiffEncoder::new_big(out).map_err(|e| format!("Cannot write {}: {e}", path.display()))?;
                Sink::Tiff { enc, path, bytes: 0 }
            }
        })
    }

    fn write(&mut self, f: &RecFrame, meta: &RawMeta) -> Result<(), String> {
        match (self, &f.image) {
            (_, RecImage::Failed(e)) => Err(e.clone()),
            (Sink::Avi(a), RecImage::Jpeg { w, h, data }) => a.write_jpeg(data, *w, *h, f.t),
            (Sink::Tiff { enc, path, bytes }, RecImage::Gray16 { w, h, data }) => {
                write_tiff_page(enc, *w, *h, data, meta).map_err(|e| format!("Cannot write {}: {e}", path.display()))?;
                *bytes += (data.len() * 2) as u64;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn path(&self) -> &Path {
        match self {
            Sink::Avi(a) => &a.path,
            Sink::Tiff { path, .. } => path,
        }
    }

    fn bytes(&self) -> u64 {
        match self {
            Sink::Avi(a) => a.done_bytes + a.pos,
            Sink::Tiff { bytes, .. } => *bytes,
        }
    }

    fn finish(self) -> Result<(), String> {
        match self {
            Sink::Avi(a) => a.finish(),
            Sink::Tiff { enc, path, .. } => {
                // Every page is complete once written; dropping the encoder flushes the file.
                drop(enc);
                let _ = path;
                Ok(())
            }
        }
    }
}

fn writer(spec: RecordSpec, rx: Receiver<RecFrame>, stats: &Mutex<RecStats>, queued: &AtomicUsize, header: String, meta: RawMeta) {
    let mut sink: Option<Sink> = None;
    let mut csv: Option<BufWriter<File>> = None;
    let mut n: u64 = 0;
    let mut failed = false;
    let mut write = |f: RecFrame| {
        queued.fetch_sub(1, Ordering::SeqCst);
        if failed {
            return;
        }
        let r = (|| -> Result<(), String> {
            if csv.is_none() {
                let p = with_suffix(&spec.stem, "_frames.csv");
                let mut out = create(&p)?;
                writeln!(out, "{header}").map_err(|e| format!("Cannot write {}: {e}", p.display()))?;
                csv = Some(out);
            }
            if sink.is_none() {
                sink = Some(Sink::open(&spec)?);
            }
            let s = sink.as_mut().expect("sink was just opened");
            s.write(&f, &meta)?;
            if let Some(out) = csv.as_mut() {
                writeln!(out, "{n},{}", f.csv).map_err(|e| format!("Cannot write frames CSV: {e}"))?;
            }
            n += 1;
            let mut st = lock(stats);
            st.frames = n;
            st.bytes = s.bytes();
            st.path = s.path().to_path_buf();
            Ok(())
        })();
        if let Err(e) = r {
            lock(stats).error = Some(e);
            failed = true;
        }
    };
    // The encoders may finish out of order: frames that arrive early wait here for the ones before them.
    let mut early: BTreeMap<u64, RecFrame> = BTreeMap::new();
    let mut next_seq = 0;
    for f in rx {
        early.insert(f.seq, f);
        while let Some(f) = early.remove(&next_seq) {
            next_seq += 1;
            write(f);
        }
    }
    // Only reached with a gap in the numbers, which push() prevents.
    early.into_values().for_each(&mut write);
    drop(write);

    let mut err = None;
    if let Some(s) = sink {
        err = s.finish().err();
    }
    if let Some(mut out) = csv {
        let _ = out.flush();
    }
    let mut st = lock(stats);
    if st.error.is_none() {
        st.error = err;
    }
    st.finished = true;
}

/// AVI 1.0 files stay below this; the recording continues in `<stem>_002.avi`, ...
const AVI_LIMIT: u64 = 1_900_000_000;

/// Offsets in the fixed AVI header (see [`avi_header`]).
mod avi_at {
    pub const RIFF_SIZE: u64 = 4;
    pub const US_PER_FRAME: u64 = 32;
    pub const MAX_BYTES_PER_SEC: u64 = 36;
    pub const TOTAL_FRAMES: u64 = 48;
    pub const SUGGESTED_BUFFER: u64 = 60;
    pub const RATE: u64 = 132;
    pub const LENGTH: u64 = 140;
    pub const STRH_BUFFER: u64 = 144;
    pub const MOVI_SIZE: u64 = 216;
    /// The `movi` FourCC; idx1 offsets count from here.
    pub const MOVI: u64 = 220;
    pub const HEADER_LEN: u64 = 224;
}

/// RIFF AVI header for one MJPEG video stream; sizes and rates are patched by [`AviWriter::finish`].
fn avi_header(w: u32, h: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(avi_at::HEADER_LEN as usize);
    let u32le = |b: &mut Vec<u8>, v: u32| b.extend_from_slice(&v.to_le_bytes());
    b.extend_from_slice(b"RIFF");
    u32le(&mut b, 0);
    b.extend_from_slice(b"AVI LIST");
    u32le(&mut b, 192); // hdrl size
    b.extend_from_slice(b"hdrlavih");
    u32le(&mut b, 56);
    for v in [0, 0, 0, 0x10 /* AVIF_HASINDEX */, 0, 0, 1, 0, w, h, 0, 0, 0, 0] {
        u32le(&mut b, v);
    }
    b.extend_from_slice(b"LIST");
    u32le(&mut b, 116); // strl size
    b.extend_from_slice(b"strlstrh");
    u32le(&mut b, 56);
    b.extend_from_slice(b"vidsMJPG");
    // flags, priority + language, initial frames, scale, rate, start, length, buffer, quality, sample size
    for v in [0, 0, 0, 1000, 10_000, 0, 0, 0, u32::MAX, 0] {
        u32le(&mut b, v);
    }
    for v in [0i16, 0, w as i16, h as i16] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b.extend_from_slice(b"strf");
    u32le(&mut b, 40);
    // BITMAPINFOHEADER
    u32le(&mut b, 40);
    u32le(&mut b, w);
    u32le(&mut b, h);
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&24u16.to_le_bytes());
    b.extend_from_slice(b"MJPG");
    for v in [w * h * 3, 0, 0, 0, 0] {
        u32le(&mut b, v);
    }
    b.extend_from_slice(b"LIST");
    u32le(&mut b, 0); // movi size
    b.extend_from_slice(b"movi");
    debug_assert_eq!(b.len() as u64, avi_at::HEADER_LEN);
    b
}

struct AviWriter {
    stem: PathBuf,
    part: u32,
    path: PathBuf,
    out: Option<BufWriter<File>>,
    size: (usize, usize),
    /// (offset from `movi`, size) of every frame of the current file.
    index: Vec<(u32, u32)>,
    pos: u64,
    max_chunk: u32,
    t_first: f64,
    t_last: f64,
    /// Bytes of the files already finished.
    done_bytes: u64,
}

impl AviWriter {
    fn create(stem: PathBuf) -> Result<Self, String> {
        let path = with_suffix(&stem, ".avi");
        Ok(Self { stem, part: 1, path, out: None, size: (0, 0), index: Vec::new(), pos: 0, max_chunk: 0, t_first: 0.0, t_last: 0.0, done_bytes: 0 })
    }

    fn write_jpeg(&mut self, jpeg: &[u8], w: usize, h: usize, t: f64) -> Result<(), String> {
        if self.out.is_some() && (w, h) != self.size {
            return Ok(()); // the video keeps its first frame size
        }
        if self.out.is_some() && self.pos >= AVI_LIMIT {
            self.finish_file()?;
            self.part += 1;
            self.path = with_suffix(&self.stem, &format!("_{:03}.avi", self.part));
        }
        if self.out.is_none() {
            let mut out = create(&self.path)?;
            out.write_all(&avi_header(w as u32, h as u32)).map_err(|e| self.err(e))?;
            self.out = Some(out);
            self.size = (w, h);
            self.index.clear();
            self.pos = avi_at::HEADER_LEN;
            self.max_chunk = 0;
            self.t_first = t;
        }
        let len = jpeg.len() as u32;
        let out = self.out.as_mut().expect("file is open");
        let r = (|| {
            out.write_all(b"00dc")?;
            out.write_all(&len.to_le_bytes())?;
            out.write_all(jpeg)?;
            if len % 2 == 1 {
                out.write_all(&[0])?;
            }
            io::Result::Ok(())
        })();
        r.map_err(|e| format!("Cannot write {}: {e}", self.path.display()))?;
        self.index.push(((self.pos - avi_at::MOVI) as u32, len));
        self.pos += 8 + len as u64 + (len % 2) as u64;
        self.max_chunk = self.max_chunk.max(len);
        self.t_last = t;
        Ok(())
    }

    fn err(&self, e: io::Error) -> String {
        format!("Cannot write {}: {e}", self.path.display())
    }

    /// Writes the index and patches the header of the current file.
    fn finish_file(&mut self) -> Result<(), String> {
        let Some(mut out) = self.out.take() else { return Ok(()) };
        let n = self.index.len() as u32;
        let span = self.t_last - self.t_first;
        let fps = if n > 1 && span > 0.0 { (n - 1) as f64 / span } else { 10.0 }.clamp(0.1, 1000.0);
        let r = (|| {
            out.write_all(b"idx1")?;
            out.write_all(&(16 * n).to_le_bytes())?;
            for &(off, len) in &self.index {
                out.write_all(b"00dc")?;
                out.write_all(&0x10u32.to_le_bytes())?; // AVIIF_KEYFRAME
                out.write_all(&off.to_le_bytes())?;
                out.write_all(&len.to_le_bytes())?;
            }
            let end = self.pos + 8 + 16 * n as u64;
            let mut patch = |at: u64, v: u32| -> io::Result<()> {
                out.seek(SeekFrom::Start(at))?;
                out.write_all(&v.to_le_bytes())
            };
            patch(avi_at::RIFF_SIZE, (end - 8) as u32)?;
            patch(avi_at::US_PER_FRAME, (1e6 / fps).round() as u32)?;
            patch(avi_at::MAX_BYTES_PER_SEC, (self.max_chunk as f64 * fps).round() as u32)?;
            patch(avi_at::TOTAL_FRAMES, n)?;
            patch(avi_at::SUGGESTED_BUFFER, self.max_chunk + 8)?;
            patch(avi_at::RATE, (fps * 1000.0).round() as u32)?;
            patch(avi_at::LENGTH, n)?;
            patch(avi_at::STRH_BUFFER, self.max_chunk + 8)?;
            patch(avi_at::MOVI_SIZE, (self.pos - avi_at::MOVI) as u32)?;
            out.flush()
        })();
        r.map_err(|e| self.err(e))?;
        self.done_bytes += self.pos + 8 + 16 * n as u64;
        self.pos = 0;
        Ok(())
    }

    fn finish(mut self) -> Result<(), String> {
        self.finish_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::Color32;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join("thorcam_export_tests");
        std::fs::create_dir_all(&d).unwrap();
        d.join(name)
    }

    #[test]
    fn avi_header_layout() {
        let h = avi_header(1440, 1080);
        assert_eq!(h.len() as u64, avi_at::HEADER_LEN);
        assert_eq!(&h[avi_at::MOVI as usize..], b"movi");
        assert_eq!(&h[108..116], b"vidsMJPG");
        assert_eq!(u32::from_le_bytes(h[128..132].try_into().unwrap()), 1000); // scale
        assert_eq!(u32::from_le_bytes(h[64..68].try_into().unwrap()), 1440);
    }

    #[test]
    fn avi_round_trip() {
        let stem = scratch("avi_round_trip");
        let mut a = AviWriter::create(stem.clone()).unwrap();
        for i in 0..5 {
            let img = ColorImage::new([64, 48], vec![Color32::from_gray(i * 40); 64 * 48]);
            a.write_jpeg(&encode_jpeg(&img).unwrap(), 64, 48, i as f64 * 0.04).unwrap();
        }
        a.finish().unwrap();
        let b = std::fs::read(with_suffix(&stem, ".avi")).unwrap();
        let rd = |at: u64| u32::from_le_bytes(b[at as usize..at as usize + 4].try_into().unwrap());
        assert_eq!(rd(avi_at::RIFF_SIZE) as usize, b.len() - 8);
        assert_eq!(rd(avi_at::TOTAL_FRAMES), 5);
        assert_eq!(rd(avi_at::RATE), 25_000); // 25 fps
        // The first index entry points at the first 00dc chunk.
        let idx = b.windows(4).rposition(|w| w == b"idx1").unwrap();
        let off = u32::from_le_bytes(b[idx + 16..idx + 20].try_into().unwrap()) as usize;
        assert_eq!(&b[avi_at::MOVI as usize + off..avi_at::MOVI as usize + off + 4], b"00dc");
        assert_eq!(&b[avi_at::MOVI as usize + off + 8..avi_at::MOVI as usize + off + 10], &[0xFF, 0xD8]); // JPEG SOI
    }

    #[test]
    fn recorder_keeps_frame_order() {
        let stem = scratch("recorder_order");
        let spec = RecordSpec { id: 1, stem: stem.clone(), format: VideoFormat::Avi };
        let meta = RawMeta { pixel_um: (3.45, 3.45), full_scale: 1023, model: "Test".into(), serial: "1".into() };
        let rec = Recorder::start(spec, "px", meta);
        let mut sent = 0;
        for i in 0..40u8 {
            if rec.has_room() {
                rec.push(RecImage::Color(ColorImage::new([96, 64], vec![Color32::from_gray(i * 6); 96 * 64])), format!("row{i}"));
                sent += 1;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let st = rec.finish();
        assert!(st.error.is_none(), "{st:?}");
        assert_eq!(st.frames, sent);
        let csv = std::fs::read_to_string(with_suffix(&stem, "_frames.csv")).unwrap();
        let idx: Vec<u64> = csv.lines().skip(1).map(|l| l.split(',').next().unwrap().parse().unwrap()).collect();
        assert_eq!(idx, (0..sent).collect::<Vec<_>>());
        let rows: Vec<u32> = csv.lines().skip(1).map(|l| l.rsplit("row").next().unwrap().parse().unwrap()).collect();
        assert!(rows.windows(2).all(|w| w[0] < w[1]), "frames out of order: {rows:?}");
    }

    #[test]
    fn raw_tiff_is_written() {
        let p = scratch("raw.tif");
        let data: Vec<u16> = (0..64 * 48).map(|i| (i % 1024) as u16).collect();
        let meta = RawMeta { pixel_um: (3.45, 3.45), full_scale: 1023, model: "Test".into(), serial: "1".into() };
        save_raw_tiff(&p, 64, 48, &data, &meta).unwrap();
        let mut dec = tiff::decoder::Decoder::new(File::open(&p).unwrap()).unwrap();
        assert_eq!(dec.dimensions().unwrap(), (64, 48));
        match dec.read_image().unwrap() {
            tiff::decoder::DecodingResult::U16(v) => assert_eq!(v, data),
            _ => panic!("not 16-bit"),
        }
    }
}

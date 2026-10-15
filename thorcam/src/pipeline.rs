//! The two worker threads of a camera session. The UI thread is the third loop.
//!
//! * **Acquisition** (producer) is the only code that touches the camera. It applies settings only when they
//!   change and puts each frame in a one-element lossy queue.
//! * **Processing** (consumer) analyses the newest frame, logs and records it, and at most every 100 ms builds
//!   the auto-zoomed display image. While paused it keeps the frame on screen and analyses that one again, so
//!   the analysis and display controls still work on it, and it saves the exports the UI asks for.
//!
//! Dropping the [`Session`] stops both threads and waits for them, so the camera is always disarmed and closed.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use beam::autozoom::{display_params, MIN_PIXELS_PER_LINE};
use beam::bayer::{cell_rgb, channel_image, Channel, Pattern};
use beam::{analyze, find_region, zoom_window, BeamParams, BeamResult, ImageRef, Window, ZoomSettings};
use eframe::egui::{Color32, ColorImage, Context};
use tlcam::{Applied, CameraInfo, CameraSettings, CfaPhase, Frame};

use crate::export::{self, csv_header, csv_row, with_suffix, Profiles, RawMeta, RecImage, RecStats, RecordSpec, Recorder, ResultsMeta, VideoFormat};
use crate::slot::{Latest, Take};
use crate::source::{self, SourceKind};

const DISPLAY_PERIOD: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Palette {
    #[default]
    Gray,
    Thermal,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DarkCmd {
    Record(usize),
    Clear,
}

/// Which image an export takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    /// The whole frame, full resolution, as displayed.
    Full,
    /// The zoom window's crop, as displayed.
    Zoom,
}

/// A one-shot export, done by the processing thread on the frame on screen (the held one while paused).
#[derive(Clone, Debug, PartialEq)]
pub enum ExportCmd {
    /// `<stem>.png` (full frame as displayed), `<stem>_raw.tif` (16-bit counts) and `<stem>.csv` (results and
    /// profiles), each when asked for.
    Snapshot { stem: PathBuf, png: bool, raw: bool, csv: bool },
    Png { path: PathBuf, view: View },
    Raw { path: PathBuf },
    /// The picture goes to [`Shared::clipboard`].
    Copy { view: View },
}

/// Settings the processing thread reads on every frame. Written by the UI.
#[derive(Clone, Debug)]
pub struct Controls {
    pub roi_half: usize,
    /// Beam detection: counts above background needed to start detecting; 2/3 of it keeps a beam detected.
    pub min_contrast: f64,
    /// Beam detection: smallest fit amplitude / RMS residual.
    pub min_snr: f64,
    pub zoom: ZoomSettings,
    pub palette: Palette,
    /// Colour cameras: what the analysis uses from each 2 x 2 Bayer cell.
    pub channel: Channel,
    /// Colour cameras: show the RGB image (true) or the analysed channel in the palette (false).
    pub color_view: bool,
    /// Plot-area width / height of the image graph, for square pixels.
    pub plot_aspect: f64,
    pub log_path: Option<PathBuf>,
    /// A one-shot command; the processing thread takes it.
    pub dark: Option<DarkCmd>,
    /// Pause: new frames are not analysed, the one on screen stays (and is analysed again as controls change).
    pub paused: bool,
    /// One-shot exports; the processing thread takes them.
    pub exports: Vec<ExportCmd>,
    /// The recording to write; `None` stops it.
    pub record: Option<RecordSpec>,
}

impl Default for Controls {
    fn default() -> Self {
        Self { roi_half: 150, min_contrast: 30.0, min_snr: 8.0, zoom: ZoomSettings::default(), palette: Palette::Gray, channel: Channel::Sum, color_view: true, plot_aspect: 4.0 / 3.0, log_path: None, dark: None, paused: false, exports: Vec::new(), record: None }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum SessionState {
    #[default]
    Idle,
    Connecting,
    Running,
    /// Stopped, with the error that stopped it.
    Stopped(Option<String>),
}

#[derive(Clone, Debug)]
pub struct Analysis {
    pub result: BeamResult,
    pub frame_count: u64,
    pub fps: f64,
}

pub struct DisplayImage {
    /// The zoom window's crop, full resolution.
    pub image: ColorImage,
    pub window: Window,
    pub text: String,
    /// The whole frame, block-averaged down to at most [`FULL_VIEW_MAX`] px wide.
    pub full: ColorImage,
    /// Frame size: (rows, cols).
    pub frame_size: (usize, usize),
    /// The fitted beam of the same frame, for the outline overlay.
    pub outline: Option<Outline>,
}

/// The fitted 1/e² ellipse, in plot coordinates: pixel `c` spans `[c, c + 1)`, so its centre is `c + 0.5`.
#[derive(Clone, Copy, Debug)]
pub struct Outline {
    pub cx: f64,
    pub cy: f64,
    /// 1/e² radii (2 sigma), px.
    pub rx: f64,
    pub ry: f64,
    pub dx_um: f64,
    pub dy_um: f64,
}

impl Outline {
    fn from_result(r: &BeamResult) -> Option<Self> {
        let (fx, fy) = (r.x.fit?, r.y.fit?);
        let ok = [fx.center, fy.center, fx.sigma, fy.sigma, r.dx_um, r.dy_um].iter().all(|v| v.is_finite());
        ok.then(|| Self { cx: fx.center + 0.5, cy: fy.center + 0.5, rx: 2.0 * fx.sigma, ry: 2.0 * fy.sigma, dx_um: r.dx_um, dy_um: r.dy_um })
    }
}

/// Width limit of the full-frame overview image.
const FULL_VIEW_MAX: usize = 720;

/// A dark frame is refused when its despeckled maximum is this many counts above its mean: it saw light.
const DARK_MAX_CONTRAST: f64 = 25.0;

/// What the worker threads report to the UI.
#[derive(Default)]
pub struct Shared {
    pub state: SessionState,
    pub info: Option<CameraInfo>,
    pub applied: Option<Applied>,
    pub settings_error: Option<String>,
    pub analysis: Option<Analysis>,
    pub display: Option<DisplayImage>,
    /// Increments with every new display image.
    pub display_gen: u64,
    pub dark_status: String,
    pub log_status: String,
    /// Whether the analysis on screen is of a held frame.
    pub paused: bool,
    /// Outcome of the last export or recording, for the status bar: (message, is an error). The UI takes it.
    pub export_msg: Option<(String, bool)>,
    /// A picture for the clipboard ("Copy image"). The UI takes it.
    pub clipboard: Option<ColorImage>,
    /// The running or the last recording.
    pub recording: Option<RecStats>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct Session {
    stop: Arc<AtomicBool>,
    frames: Arc<Latest<Frame>>,
    settings_tx: Sender<CameraSettings>,
    threads: Vec<JoinHandle<()>>,
}

impl Session {
    pub fn start(
        kind: SourceKind,
        dll_dir: Option<PathBuf>,
        initial: CameraSettings,
        shared: Arc<Mutex<Shared>>,
        controls: Arc<Mutex<Controls>>,
        ctx: Context,
    ) -> Session {
        {
            let mut s = lock(&shared);
            *s = Shared { state: SessionState::Connecting, ..Default::default() };
        }
        let stop = Arc::new(AtomicBool::new(false));
        let frames = Arc::new(Latest::new());
        let (settings_tx, settings_rx) = mpsc::channel();

        let acq = {
            let (stop, frames, shared, ctx) = (stop.clone(), frames.clone(), shared.clone(), ctx.clone());
            std::thread::Builder::new()
                .name("acquisition".into())
                .spawn(move || acquisition(kind, dll_dir, initial, settings_rx, &frames, &stop, &shared, &ctx))
                .expect("spawn acquisition thread")
        };
        let proc = {
            let (frames, shared) = (frames.clone(), shared.clone());
            std::thread::Builder::new()
                .name("processing".into())
                .spawn(move || processing(&frames, shared, &controls, &ctx))
                .expect("spawn processing thread")
        };
        Session { stop, frames, settings_tx, threads: vec![acq, proc] }
    }

    pub fn send_settings(&self, s: CameraSettings) {
        let _ = self.settings_tx.send(s);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.frames.close();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn acquisition(
    kind: SourceKind,
    dll_dir: Option<PathBuf>,
    initial: CameraSettings,
    settings_rx: Receiver<CameraSettings>,
    frames: &Latest<Frame>,
    stop: &AtomicBool,
    shared: &Mutex<Shared>,
    ctx: &Context,
) {
    let fail = |msg: String| {
        lock(shared).state = SessionState::Stopped(Some(msg));
        frames.close();
        ctx.request_repaint();
    };

    let mut cam = match source::open(kind, dll_dir.as_deref()) {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    lock(shared).info = Some(cam.info().clone());

    // The camera starts with what the panel shows.
    let applied = cam.apply(&initial);
    let started = cam.start();
    {
        let mut s = lock(shared);
        match applied {
            Ok(a) => s.applied = Some(a),
            Err(e) => s.settings_error = Some(e),
        }
    }
    if let Err(e) = started {
        drop(cam);
        return fail(e);
    }
    lock(shared).state = SessionState::Running;
    ctx.request_repaint();

    let mut error = None;
    while !stop.load(Ordering::SeqCst) {
        // Settings are written only when they change; only the newest request matters.
        if let Some(s) = settings_rx.try_iter().last() {
            let r = cam.apply(&s);
            let mut sh = lock(shared);
            match r {
                Ok(a) => {
                    sh.applied = Some(a);
                    sh.settings_error = None;
                }
                Err(e) => sh.settings_error = Some(e),
            }
            ctx.request_repaint();
        }
        match cam.next_frame() {
            Ok(Some(f)) => frames.put(f),
            Ok(None) => {}
            Err(e) => {
                error = Some(e);
                break;
            }
        }
    }

    // Disarm and close before telling anyone the session is over.
    drop(cam);
    frames.close();
    lock(shared).state = SessionState::Stopped(error);
    ctx.request_repaint();
}

/// Frame-rate counter: Δ frame number ÷ Δ t over about one second.
struct Fps {
    mark: Option<(Instant, u64)>,
    value: f64,
}

impl Fps {
    fn update(&mut self, count: u64) -> f64 {
        let now = Instant::now();
        match self.mark {
            None => self.mark = Some((now, count)),
            Some((t, c)) => {
                let dt = now.duration_since(t).as_secs_f64();
                if dt >= 1.0 {
                    self.value = count.saturating_sub(c) as f64 / dt;
                    self.mark = Some((now, count));
                }
            }
        }
        self.value
    }
}

struct Logger {
    path: PathBuf,
    out: BufWriter<File>,
    t0: Instant,
}

impl Logger {
    fn open(path: PathBuf, unit: &str) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let empty = file.metadata()?.len() == 0;
        let mut out = BufWriter::new(file);
        if empty {
            writeln!(out, "{}", csv_header(unit))?;
        }
        Ok(Self { path, out, t0: Instant::now() })
    }

    fn write(&mut self, a: &Analysis) -> std::io::Result<()> {
        writeln!(self.out, "{}", csv_row(self.t0.elapsed().as_secs_f64(), a))
    }
}

struct DarkRecorder {
    sum: Vec<f64>,
    n: usize,
    target: usize,
}

/// Display levels of the last display update, reused by recordings between updates.
#[derive(Clone, Copy, Debug, Default)]
struct Levels {
    /// White level of the palette image.
    zmax: f64,
    /// Background of the analysed image (black level of the colour image).
    background: f64,
    /// Colour image: 255 / white level, from the zoom window.
    rgb_k: Option<f32>,
}

/// Everything needed to paint the analysed frame the way the display shows it.
struct Renderer<'a> {
    img: ImageRef<'a>,
    /// Colour view of a Bayer frame; `None` paints `img` with the palette.
    rgb: Option<RgbView<'a>>,
    rgb_k: f32,
    clipped: Option<&'a [bool]>,
    zmax: f64,
    full_scale: u32,
    lut: &'a [Color32; 256],
}

impl Renderer<'_> {
    fn crop(&self, win: &Window) -> ColorImage {
        match &self.rgb {
            Some(v) => v.render(win, self.rgb_k),
            None => render(self.img, win, self.zmax, self.full_scale, self.lut, self.clipped),
        }
    }

    /// The whole frame at full resolution (one pixel per cell on a colour camera).
    fn full(&self) -> ColorImage {
        self.crop(&Window::full(self.img.height, self.img.width))
    }

    /// The whole frame for the overview graph: block-averaged to at most [`FULL_VIEW_MAX`] px wide.
    fn overview(&self) -> ColorImage {
        match &self.rgb {
            Some(_) => self.full(),
            None => render_overview(self.img, self.zmax, self.lut),
        }
    }
}

/// Runs a file write off the processing thread and reports the outcome in [`Shared::export_msg`].
fn save_async(shared: &Arc<Mutex<Shared>>, ctx: &Context, job: impl FnOnce() -> Result<String, String> + Send + 'static) {
    let (shared, ctx) = (shared.clone(), ctx.clone());
    std::thread::spawn(move || {
        let msg = match job() {
            Ok(m) => (m, false),
            Err(e) => (e, true),
        };
        lock(&shared).export_msg = Some(msg);
        ctx.request_repaint();
    });
}

fn file_name(p: &std::path::Path) -> String {
    p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
}

fn rec_message(st: &RecStats) -> (String, bool) {
    if let Some(e) = &st.error {
        return (format!("Recording stopped: {e}"), true);
    }
    let dropped = if st.dropped > 0 { format!(", {} dropped", st.dropped) } else { String::new() };
    (
        format!("Recording saved: {} ({} frames{dropped}, {:.1} s, {:.1} MB)", st.path.display(), st.frames, st.secs, st.bytes as f64 / 1e6),
        false,
    )
}

fn processing(frames: &Latest<Frame>, shared_arc: Arc<Mutex<Shared>>, controls: &Mutex<Controls>, ctx: &Context) {
    let shared: &Mutex<Shared> = &shared_arc;
    let mut info: Option<CameraInfo> = None;
    let mut window: Option<Window> = None;
    let mut zoomed = false;
    let mut last_display: Option<Instant> = None;
    let mut fps = Fps { mark: None, value: 0.0 };
    let mut beam_present = false;
    let mut dark: Option<Vec<f64>> = None;
    let mut dark_rec: Option<DarkRecorder> = None;
    let mut logger: Option<Logger> = None;
    let mut last_channel: Option<Channel> = None;
    let palettes = (lut(Palette::Gray), lut(Palette::Thermal));
    // The frame on screen: the newest one, or while paused the one that was shown when Pause was pressed.
    let mut held: Option<Frame> = None;
    let mut last_rerun: Option<Instant> = None;
    let mut pending: Vec<ExportCmd> = Vec::new();
    let mut pending_dark: Option<DarkCmd> = None;
    let mut levels = Levels::default();
    let mut recorder: Option<Recorder> = None;
    // Recordings being finished (queued frames, index) off this thread; joined before the session ends.
    let mut finishing: Vec<JoinHandle<()>> = Vec::new();

    loop {
        let fresh = match frames.take(Duration::from_millis(200)) {
            Take::Value(f) => Some(f),
            Take::Timeout => None,
            Take::Closed => break,
        };
        let ctl = {
            let mut c = lock(controls);
            let mut snapshot = c.clone();
            c.dark = None;
            c.exports.clear();
            pending.append(&mut snapshot.exports);
            if let Some(d) = snapshot.dark.take() {
                pending_dark = Some(d);
            }
            snapshot
        };
        if info.is_none() {
            info = lock(shared).info.clone();
        }
        let Some(info) = info.as_ref() else { continue };

        // Recording: start or stop with what the UI asks for. Stopping writes the queued frames and the index.
        if recorder.as_ref().map(|r| &r.spec) != ctl.record.as_ref() {
            if let Some(r) = recorder.take() {
                let (shared, ctx) = (shared_arc.clone(), ctx.clone());
                finishing.retain(|t| !t.is_finished());
                finishing.push(std::thread::spawn(move || finish_recording(r, &shared, &ctx)));
            }
            if let Some(spec) = &ctl.record {
                let unit = if info.color.is_some() { "cell" } else { "px" };
                recorder = Some(Recorder::start(spec.clone(), unit, RawMeta::new(info)));
            }
        }
        if let Some(r) = recorder.as_mut() {
            r.set_paused(ctl.paused);
        }

        // Live: analyse the new frame. Paused: analyse the held frame again, at the display rate, so changed
        // controls apply to it; and whenever an export is waiting.
        let live = match fresh {
            Some(f) if !ctl.paused => {
                held = Some(f);
                true
            }
            _ => false,
        };
        let rerun = !live
            && held.is_some()
            && (!pending.is_empty()
                || matches!(pending_dark, Some(DarkCmd::Clear))
                || (ctl.paused && (pending_dark.is_some() || last_rerun.is_none_or(|t| t.elapsed() >= DISPLAY_PERIOD))));
        if !live && !rerun {
            let mut s = lock(shared);
            s.paused = ctl.paused;
            if let Some(r) = &recorder {
                s.recording = Some(r.stats());
            }
            continue;
        }
        if rerun {
            last_rerun = Some(Instant::now());
        }
        let frame = held.as_ref().expect("a frame is held");
        let raw = ImageRef::new(&frame.data, frame.width, frame.height);

        // Colour sensors: analyse one value per 2 x 2 Bayer cell; the effective pixel is twice as large.
        let bayer = info.color.map(|c| (pattern(c.phase), c.display_matrix()));
        let channel_img = bayer.map(|(pat, _)| channel_image(raw, pat, ctl.channel, info.full_scale()));
        // Colour: the analysis gets the channel's full scale (for "overexposed"); its Saturated flag is then
        // replaced by the clipped-cell test below, so hot pixels never count.
        let (img, full_scale, unit) = match &channel_img {
            Some(c) => (c.as_ref(), c.full_scale, "cell"),
            None => (raw, info.full_scale(), "px"),
        };
        let (pw, ph) = info.effective_pixel_um();
        let (pw, ph) = if bayer.is_some() { (2.0 * pw, 2.0 * ph) } else { (pw, ph) };
        let n = img.width * img.height;

        let mut dark_status = None;
        if bayer.is_some() && last_channel.is_some_and(|c| c != ctl.channel) && (dark.is_some() || dark_rec.is_some()) {
            dark = None;
            dark_rec = None;
            dark_status = Some("Dark frame cleared: the colour channel changed".to_owned());
        }
        last_channel = Some(ctl.channel);

        // Dark frame: average N live frames recorded with the beam blocked (of the analysed image).
        match pending_dark.take() {
            Some(DarkCmd::Record(target)) if live => dark_rec = Some(DarkRecorder { sum: vec![0.0; n], n: 0, target: target.max(1) }),
            Some(DarkCmd::Record(_)) if ctl.paused => dark_status = Some("Press Play first: a dark frame is recorded from live frames".to_owned()),
            Some(cmd @ DarkCmd::Record(_)) => pending_dark = Some(cmd),
            Some(DarkCmd::Clear) => {
                dark = None;
                dark_rec = None;
                dark_status = Some("No dark frame".to_owned());
            }
            None => {}
        }
        if let Some(rec) = dark_rec.as_mut().filter(|_| live) {
            if rec.sum.len() == n {
                rec.sum.iter_mut().zip(img.data).for_each(|(s, &v)| *s += v as f64);
                rec.n += 1;
                dark_status = Some(format!("Recording dark frame {}/{}", rec.n, rec.target));
                if rec.n >= rec.target {
                    let k = 1.0 / rec.n as f64;
                    let d: Vec<f64> = rec.sum.iter().map(|s| s * k).collect();
                    let contrast = light_in_dark(&d, img.width, img.height);
                    if contrast > DARK_MAX_CONTRAST {
                        dark_status = Some(format!(
                            "Dark frame refused: it has light {contrast:.0} counts above its mean. Block the beam and record again."
                        ));
                    } else {
                        dark = Some(d);
                        dark_status = Some(format!("Dark frame on ({} frames)", rec.n));
                    }
                    dark_rec = None;
                }
            } else {
                dark_rec = None;
            }
        }

        // Hysteresis: a beam needs the full contrast to be found and 2/3 of it to stay found, so a beam near the
        // limit does not flicker between "beam" and "no beam".
        let min_contrast = if beam_present { ctl.min_contrast * 2.0 / 3.0 } else { ctl.min_contrast };
        let params = BeamParams {
            full_scale,
            pixel_w_um: pw,
            pixel_h_um: ph,
            roi_half: ctl.roi_half,
            min_contrast,
            min_snr: ctl.min_snr,
        };
        let mut result = analyze(img, &params, dark.as_deref());
        if let Some(c) = &channel_img {
            result.saturated = result.detected() && c.saturated();
        }
        beam_present = result.detected();
        let rate = if live { fps.update(frame.frame_count) } else { fps.value };
        let analysis = Analysis { result, frame_count: frame.frame_count, fps: rate };

        // CSV log, of live frames only.
        let mut log_status = None;
        if logger.as_ref().map(|l| &l.path) != ctl.log_path.as_ref() {
            logger = None;
            if let Some(p) = &ctl.log_path {
                match Logger::open(p.clone(), unit) {
                    Ok(l) => {
                        log_status = Some(format!("Logging to {}", p.display()));
                        logger = Some(l);
                    }
                    Err(e) => log_status = Some(format!("Cannot open {}: {e}", p.display())),
                }
            } else {
                log_status = Some(String::new());
            }
        }
        if let Some(l) = logger.as_mut().filter(|_| live) {
            if let Err(e) = l.write(&analysis) {
                log_status = Some(format!("Log write failed: {e}"));
                logger = None;
            }
        }

        // Auto-zoom window and display levels, at most 10 times a second (every re-analysis while paused).
        let mut zoom_text = None;
        if rerun || last_display.is_none_or(|t| t.elapsed() >= DISPLAY_PERIOD) {
            last_display = Some(Instant::now());
            let region = find_region(img, ctl.zoom.threshold_pct, MIN_PIXELS_PER_LINE, zoomed);
            let prev = window.unwrap_or_else(|| Window::full(img.height, img.width));
            let (win, z) = zoom_window(prev, &region, ctl.plot_aspect, &ctl.zoom);
            window = Some(win);
            zoomed = z;
            let dp = display_params(&win, &region, if bayer.is_some() { "cells" } else { "px" });
            levels = Levels { zmax: dp.zmax, background: region.background, rgb_k: None };
            zoom_text = Some(dp.text);
        }
        let win = window.unwrap_or_else(|| Window::full(img.height, img.width));
        let rgb = match bayer {
            Some((pat, m)) if ctl.color_view => {
                // Black level per raw pixel: the background of the channel image over the pixels it sums.
                let per_cell = ctl.channel.full_scale(1) as f32;
                Some(RgbView { raw, pat, m, black: levels.background as f32 / per_cell, sat: info.full_scale() })
            }
            _ => None,
        };
        if let (Some(v), None) = (&rgb, levels.rgb_k) {
            levels.rgb_k = Some(v.scale(&win));
        }
        let view = Renderer {
            img,
            rgb,
            rgb_k: levels.rgb_k.unwrap_or(1.0),
            clipped: channel_img.as_ref().map(|c| c.clipped.as_slice()),
            zmax: levels.zmax,
            full_scale,
            lut: if ctl.palette == Palette::Gray { &palettes.0 } else { &palettes.1 },
        };
        let display = zoom_text.map(|text| DisplayImage {
            image: view.crop(&win),
            window: win,
            text,
            full: view.overview(),
            frame_size: (img.height, img.width),
            outline: Outline::from_result(&analysis.result),
        });

        // Recording, of live frames only. A busy writer drops the frame before it is even painted.
        if let Some(r) = recorder.as_ref().filter(|r| live && !r.failed()) {
            if r.has_room() {
                let image = match r.spec.format {
                    VideoFormat::Avi => RecImage::Color(view.full()),
                    VideoFormat::TiffStack => RecImage::Gray16 { w: frame.width, h: frame.height, data: frame.data.clone() },
                };
                r.push(image, csv_row(r.time(), &analysis));
            }
        }

        // Exports of the frame on screen.
        if !pending.is_empty() {
            let applied = lock(shared).applied;
            for cmd in pending.drain(..) {
                match cmd {
                    ExportCmd::Copy { view: v } => {
                        lock(shared).clipboard = Some(if v == View::Full { view.full() } else { view.crop(&win) });
                    }
                    ExportCmd::Png { path, view: v } => {
                        let img = if v == View::Full { view.full() } else { view.crop(&win) };
                        save_async(&shared_arc, ctx, move || export::save_png(&path, &img).map(|_| format!("Saved {}", path.display())));
                    }
                    ExportCmd::Raw { path } => {
                        let (w, h, data, meta) = (frame.width, frame.height, frame.data.clone(), RawMeta::new(info));
                        save_async(&shared_arc, ctx, move || {
                            export::save_raw_tiff(&path, w, h, &data, &meta).map(|_| format!("Saved {}", path.display()))
                        });
                    }
                    ExportCmd::Snapshot { stem, png, raw, csv } => {
                        let img = png.then(|| view.full());
                        let data = raw.then(|| (frame.width, frame.height, frame.data.clone(), RawMeta::new(info)));
                        let meta = ResultsMeta { info: Some(info), applied, unit, pixel_um: (pw, ph) };
                        let table = csv.then(|| export::results_table(&analysis, &meta, Profiles::Both, ','));
                        save_async(&shared_arc, ctx, move || {
                            let mut saved = Vec::new();
                            if let Some(img) = img {
                                let p = with_suffix(&stem, ".png");
                                export::save_png(&p, &img)?;
                                saved.push(file_name(&p));
                            }
                            if let Some((w, h, data, meta)) = data {
                                let p = with_suffix(&stem, "_raw.tif");
                                export::save_raw_tiff(&p, w, h, &data, &meta)?;
                                saved.push(file_name(&p));
                            }
                            if let Some(t) = table {
                                let p = with_suffix(&stem, ".csv");
                                export::save_text(&p, &t)?;
                                saved.push(file_name(&p));
                            }
                            let dir = stem.parent().map_or(String::new(), |d| d.display().to_string());
                            Ok(format!("Snapshot saved in {dir}: {}", saved.join(", ")))
                        });
                    }
                }
            }
        }

        let mut s = lock(shared);
        s.analysis = Some(analysis);
        s.paused = ctl.paused;
        if let Some(r) = &recorder {
            s.recording = Some(r.stats());
        }
        if let Some(d) = display {
            s.display = Some(d);
            s.display_gen += 1;
        }
        if let Some(d) = dark_status {
            s.dark_status = d;
        }
        if let Some(l) = log_status {
            s.log_status = l;
        }
        drop(s);
        ctx.request_repaint();
    }
    if let Some(mut l) = logger {
        let _ = l.out.flush();
    }
    if let Some(r) = recorder {
        finish_recording(r, shared, ctx);
    }
    for t in finishing {
        let _ = t.join();
    }
}

fn finish_recording(r: Recorder, shared: &Mutex<Shared>, ctx: &Context) {
    let st = r.finish();
    let mut s = lock(shared);
    s.export_msg = Some(rec_message(&st));
    s.recording = Some(st);
    drop(s);
    ctx.request_repaint();
}

/// The crop as colours: 0 is black, `zmax` is white (or the top of the palette); saturated pixels are red.
/// `clipped` (colour cameras) marks saturated cells instead of the value test.
fn render(img: ImageRef, win: &Window, zmax: f64, full_scale: u32, lut: &[Color32; 256], clipped: Option<&[bool]>) -> ColorImage {
    let (r0, c0, rows, cols) = (win.row0 as usize, win.col0 as usize, win.rows as usize, win.cols as usize);
    let k = 255.0 / zmax.max(1.0) as f32;
    let sat = full_scale.min(u16::MAX as u32) as u16;
    let mut pixels = Vec::with_capacity(rows * cols);
    for r in r0..r0 + rows {
        for (c, &v) in (c0..).zip(&img.row(r)[c0..c0 + cols]) {
            let sat = clipped.map_or(v >= sat, |m| m[r * img.width + c]);
            pixels.push(if sat { Color32::RED } else { lut[((v as f32 * k) as usize).min(255)] });
        }
    }
    ColorImage::new([cols, rows], pixels)
}

fn pattern(p: CfaPhase) -> Pattern {
    match p {
        CfaPhase::Red => Pattern::RGGB,
        CfaPhase::Blue => Pattern::BGGR,
        CfaPhase::GreenLeftOfRed => Pattern::GRBG,
        CfaPhase::GreenLeftOfBlue => Pattern::GBRG,
    }
}

/// Colour picture of a Bayer frame, one display pixel per 2 x 2 cell: subtract black, apply the camera's
/// white balance and colour correction, scale so the brightest 0.1 % of the crop is white.
/// Cells with a clipped raw pixel are magenta (red would vanish in a red beam).
struct RgbView<'a> {
    raw: ImageRef<'a>,
    pat: Pattern,
    m: [f32; 9],
    black: f32,
    sat: u32,
}

impl RgbView<'_> {
    fn rgb(&self, i: usize, j: usize) -> [f32; 3] {
        let c = cell_rgb(self.raw, self.pat, i, j).map(|v| (v - self.black).max(0.0));
        let m = &self.m;
        std::array::from_fn(|r| (m[r * 3] * c[0] + m[r * 3 + 1] * c[1] + m[r * 3 + 2] * c[2]).max(0.0))
    }

    /// 255 / white level, from the 99.9th percentile of the brightest component in the window.
    fn scale(&self, win: &Window) -> f32 {
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

    fn render(&self, win: &Window, k: f32) -> ColorImage {
        let (r0, c0, rows, cols) = (win.row0 as usize, win.col0 as usize, win.rows as usize, win.cols as usize);
        let sat = self.sat.min(u16::MAX as u32) as u16;
        let mut pixels = Vec::with_capacity(rows * cols);
        for i in r0..r0 + rows {
            for j in c0..c0 + cols {
                let clipped = (0..2).any(|dr| (0..2).any(|dc| self.raw.at(2 * i + dr, 2 * j + dc) >= sat));
                pixels.push(if clipped {
                    Color32::from_rgb(255, 0, 255)
                } else {
                    let [r, g, b] = self.rgb(i, j).map(|v| (v * k).min(255.0) as u8);
                    Color32::from_rgb(r, g, b)
                });
            }
        }
        ColorImage::new([cols, rows], pixels)
    }
}

/// The whole frame, block-averaged so it is at most [`FULL_VIEW_MAX`] px wide.
fn render_overview(img: ImageRef, zmax: f64, lut: &[Color32; 256]) -> ColorImage {
    let f = img.width.div_ceil(FULL_VIEW_MAX).max(1);
    let (cols, rows) = (img.width / f, img.height / f);
    let k = 255.0 / (zmax.max(1.0) * (f * f) as f64) as f32;
    let mut sums = vec![0u32; cols];
    let mut pixels = Vec::with_capacity(rows * cols);
    for br in 0..rows {
        sums.fill(0);
        for r in br * f..(br + 1) * f {
            let line = img.row(r);
            for (bc, s) in sums.iter_mut().enumerate() {
                *s += line[bc * f..(bc + 1) * f].iter().map(|&v| v as u32).sum::<u32>();
            }
        }
        pixels.extend(sums.iter().map(|&s| lut[((s as f32 * k) as usize).min(255)]));
    }
    ColorImage::new([cols, rows], pixels)
}

/// Despeckled maximum minus mean: how much light a would-be dark frame saw. Fixed hot pixels do not count.
fn light_in_dark(d: &[f64], w: usize, h: usize) -> f64 {
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

fn lut(p: Palette) -> [Color32; 256] {
    std::array::from_fn(|i| {
        let t = i as f32 / 255.0;
        match p {
            Palette::Gray => Color32::from_gray(i as u8),
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
                Color32::from_rgb(c(0), c(1), c(2))
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam::synth::{render, Spot, SynthOptions};

    fn as_f64(v: &[u16]) -> Vec<f64> {
        v.iter().map(|&x| x as f64).collect()
    }

    #[test]
    fn dark_frame_with_beam_is_detected() {
        let (w, h) = (640, 480);
        let mut dark = render(w, h, &[], &SynthOptions::cs165(), 1);
        for i in [100, 5000, 77_777] {
            dark[i] = 1023; // fixed hot pixels belong in a dark frame
        }
        assert!(light_in_dark(&as_f64(&dark), w, h) < DARK_MAX_CONTRAST);
        let lit = render(w, h, &[Spot { a: 150.0, x0: 300.0, y0: 200.0, sx: 20.0, sy: 15.0 }], &SynthOptions::cs165(), 2);
        assert!(light_in_dark(&as_f64(&lit), w, h) > DARK_MAX_CONTRAST);
    }

    #[test]
    fn overview_is_block_averaged() {
        let (w, h) = (1440, 1080);
        let img = vec![500u16; w * h];
        let o = render_overview(ImageRef::new(&img, w, h), 1000.0, &lut(Palette::Gray));
        assert_eq!(o.size, [720, 540]);
        assert_eq!(o.pixels[0], Color32::from_gray(127));
    }
}

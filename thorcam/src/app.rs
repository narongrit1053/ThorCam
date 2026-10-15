//! The window: a toolbar (play / pause, snapshot, record, about), controls on the left, results on top, the
//! full frame and the zoomed image side by side, the X / Y profiles below, and a status bar.
//! Right-click an image or a plot for its menu (save, copy, export CSV).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use beam::bayer::Channel;
use beam::{Window, ZoomMode};
use eframe::egui::{self, Color32, Key, KeyboardShortcut, Modifiers, Rect, RichText, TextureHandle, TextureOptions};
use egui_plot::{HoverPosition, Legend, Line, LineStyle, Plot, PlotBounds, PlotImage, PlotPoint, PlotUi, Points, Text};
use tlcam::{Applied, CameraInfo, CameraSettings};

use crate::about;
use crate::export::{self, Profiles, RecStats, RecordSpec, ResultsMeta, VideoFormat};
use crate::pipeline::{Analysis, Controls, DarkCmd, ExportCmd, Outline, Palette, Session, SessionState, Shared, View};
use crate::source::{self, SourceKind};

const SHORTCUT_SNAPSHOT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::S);
const SHORTCUT_RECORD: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::R);

/// Something a menu or button asked for, done at the end of the frame (file dialogs block, so not mid-layout).
#[derive(Clone, Copy, Debug)]
enum Action {
    TogglePause,
    Snapshot,
    ToggleRecord,
    /// "Save image as PNG": the picture as displayed, full resolution.
    SavePng(View),
    SaveRaw,
    CopyImage(View),
    /// "Save view / plot as PNG": a screenshot of that part of the window, overlays and axes included.
    SaveScreen(Rect, &'static str),
    ExportCsv(Profiles),
    CopyTable(Profiles),
    ResetProfile(usize),
    OpenFolder,
    BrowseFolder,
}

/// A screenshot to crop and save once the frame without the menu has been painted.
struct ShotRequest {
    rect: Rect,
    path: PathBuf,
}

const DARK_FRAMES: usize = 20;

pub struct ThorCamApp {
    kind: SourceKind,
    dll_dir: Option<PathBuf>,
    /// Why the SDK DLLs could not be found or unpacked.
    dll_error: Option<String>,

    settings: CameraSettings,
    last_sent: Option<CameraSettings>,
    /// Serial of the camera whose values were copied into `settings`.
    synced_serial: Option<String>,

    controls: Controls,
    log_enabled: bool,
    log_path: String,

    shared: Arc<Mutex<Shared>>,
    shared_controls: Arc<Mutex<Controls>>,
    session: Option<Session>,

    // Snapshot of the worker results for this repaint.
    state: SessionState,
    info: Option<CameraInfo>,
    applied: Option<Applied>,
    settings_error: Option<String>,
    analysis: Option<Analysis>,
    dark_status: String,
    log_status: String,

    texture: Option<TextureHandle>,
    texture_gen: u64,
    window: Option<Window>,
    zoom_text: String,
    full_texture: Option<TextureHandle>,
    frame_size: Option<(usize, usize)>,
    outline: Option<Outline>,

    show_full: bool,
    show_outline: bool,
    beam_blocked: bool,

    // Record & export
    capture_dir: String,
    snap_png: bool,
    snap_raw: bool,
    snap_csv: bool,
    video_format: VideoFormat,
    next_record_id: u64,
    recording: Option<RecStats>,
    /// Status bar: (time, message, is an error).
    status: Option<(String, String, bool)>,
    actions: Vec<Action>,
    /// (frames to wait, request): the screenshot is asked for once the menu is gone.
    shot: Option<(u8, ShotRequest)>,
    reset_profile: [bool; 2],
    show_about: bool,
}

impl ThorCamApp {
    /// `start` picks the source; `None` means the Thorlabs camera when its DLLs are found, else the simulator.
    pub fn new(cc: &eframe::CreationContext<'_>, start: Option<SourceKind>) -> Self {
        let (dll_dir, dll_error) = match source::find_dll_dir() {
            Ok(d) => (Some(d), None),
            Err(e) => (None, Some(e)),
        };
        let kind = start.unwrap_or(if dll_dir.is_some() { SourceKind::Thorlabs } else { SourceKind::Simulator });
        let mut app = Self {
            kind,
            dll_dir,
            dll_error,
            settings: CameraSettings { exposure_ms: 5.0, gain: 0, black_level: None, led_on: false },
            last_sent: None,
            synced_serial: None,
            controls: Controls::default(),
            log_enabled: false,
            log_path: default_log_path(),
            shared: Arc::new(Mutex::new(Shared::default())),
            shared_controls: Arc::new(Mutex::new(Controls::default())),
            session: None,
            state: SessionState::Idle,
            info: None,
            applied: None,
            settings_error: None,
            analysis: None,
            dark_status: String::new(),
            log_status: String::new(),
            texture: None,
            texture_gen: 0,
            window: None,
            zoom_text: String::new(),
            full_texture: None,
            frame_size: None,
            outline: None,
            show_full: true,
            show_outline: true,
            beam_blocked: false,
            capture_dir: default_capture_dir(),
            snap_png: true,
            // A snapshot is a PNG unless more is ticked under Record & export.
            snap_raw: false,
            snap_csv: false,
            video_format: VideoFormat::Avi,
            next_record_id: 0,
            recording: None,
            status: None,
            actions: Vec::new(),
            shot: None,
            reset_profile: [false; 2],
            show_about: false,
        };
        app.connect(&cc.egui_ctx);
        app
    }

    fn connect(&mut self, ctx: &egui::Context) {
        // Drop the old session first: that closes its camera.
        self.session = None;
        self.synced_serial = None;
        self.last_sent = Some(self.settings);
        self.analysis = None;
        self.window = None;
        self.texture = None;
        self.full_texture = None;
        self.outline = None;
        self.controls.paused = false;
        self.controls.record = None;
        self.controls.exports.clear();
        self.recording = None;
        {
            let mut c = self.shared_controls.lock().unwrap_or_else(|e| e.into_inner());
            c.exports.clear();
            c.record = None;
            c.paused = false;
        }
        self.session = Some(Session::start(
            self.kind,
            self.dll_dir.clone(),
            self.settings,
            self.shared.clone(),
            self.shared_controls.clone(),
            ctx.clone(),
        ));
    }

    fn disconnect(&mut self) {
        // Stop a recording first, so the worker finishes the file before the session ends.
        self.controls.record = None;
        self.push_to_workers();
        self.session = None;
    }

    /// Unit of positions: a sensor pixel, or a 2×2 Bayer cell on a colour camera.
    fn unit(&self) -> &'static str {
        if self.info.as_ref().is_some_and(|i| i.color.is_some()) { "cell" } else { "px" }
    }

    fn running(&self) -> bool {
        self.state == SessionState::Running
    }

    /// Copies what the worker threads reported, and uploads a new display image.
    fn sync_from_workers(&mut self, ctx: &egui::Context) {
        let mut s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        self.state = s.state.clone();
        self.info = s.info.clone();
        self.applied = s.applied;
        self.settings_error = s.settings_error.clone();
        self.analysis = s.analysis.clone();
        self.dark_status = s.dark_status.clone();
        self.log_status = s.log_status.clone();
        self.recording = s.recording.clone();
        let msg = s.export_msg.take();
        let clip = s.clipboard.take();
        if s.display_gen != self.texture_gen {
            self.texture_gen = s.display_gen;
            if let Some(d) = s.display.take() {
                self.window = Some(d.window);
                self.zoom_text = d.text;
                self.frame_size = Some(d.frame_size);
                self.outline = d.outline;
                match &mut self.full_texture {
                    Some(t) => t.set(d.full, TextureOptions::LINEAR),
                    None => self.full_texture = Some(ctx.load_texture("full", d.full, TextureOptions::LINEAR)),
                }
                match &mut self.texture {
                    Some(t) => t.set(d.image, TextureOptions::NEAREST),
                    None => self.texture = Some(ctx.load_texture("frame", d.image, TextureOptions::NEAREST)),
                }
            }
        }
        drop(s);
        if let Some((msg, err)) = msg {
            self.set_status(msg, err);
        }
        if let Some(img) = clip {
            ctx.copy_image(img);
            self.set_status("Image copied to the clipboard".into(), false);
        }

        // A recording that failed (disk full, ...) is stopped; the worker then reports why.
        let failed = self.recording.as_ref().is_some_and(|r| r.error.is_some());
        if failed && self.controls.record.as_ref().map(|r| r.id) == self.recording.as_ref().map(|r| r.id) {
            self.controls.record = None;
        }
        if !matches!(self.state, SessionState::Running | SessionState::Connecting) {
            self.controls.record = None;
            self.controls.paused = false;
        }

        // A newly opened camera: start the panel from the camera's own black level and keep values in range.
        if let Some(info) = &self.info {
            if self.synced_serial.as_deref() != Some(info.serial.as_str()) {
                self.synced_serial = Some(info.serial.clone());
                self.settings.black_level = Some(info.initial.black_level);
                self.settings.led_on &= info.led_supported;
            }
        }
    }

    fn push_to_workers(&mut self) {
        {
            let mut c = self.shared_controls.lock().unwrap_or_else(|e| e.into_inner());
            let dark = c.dark.take().or(self.controls.dark.take());
            let mut exports = std::mem::take(&mut c.exports);
            exports.append(&mut self.controls.exports);
            *c = Controls { dark, exports, ..self.controls.clone() };
        }
        if self.last_sent != Some(self.settings) {
            if let Some(s) = &self.session {
                if self.running() {
                    s.send_settings(self.settings);
                    self.last_sent = Some(self.settings);
                }
            }
        }
    }

    // ----- left panel -----

    fn controls_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let busy = matches!(self.state, SessionState::Connecting | SessionState::Running);

        section(ui, "Camera");
        ui.add_enabled_ui(!busy, |ui| {
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.kind, SourceKind::Thorlabs, "Thorlabs");
                ui.radio_value(&mut self.kind, SourceKind::Simulator, "Simulator");
                ui.radio_value(&mut self.kind, SourceKind::SimulatorColor, "Sim. colour");
            });
        });
        if self.kind == SourceKind::Thorlabs {
            match &self.dll_dir {
                Some(d) => {
                    ui.label(RichText::new(format!("SDK: {}", d.display())).small().weak());
                }
                None => {
                    ui.colored_label(Color32::from_rgb(220, 120, 40), self.dll_error.as_deref().unwrap_or("SDK DLLs not found."));
                }
            }
        }
        ui.horizontal(|ui| {
            if busy {
                if ui.button("Disconnect").clicked() {
                    self.disconnect();
                }
            } else if ui.button("Connect").clicked() {
                self.connect(&ctx);
            }
            match &self.state {
                SessionState::Idle => ui.label("Not connected"),
                SessionState::Connecting => ui.label("Connecting…"),
                SessionState::Running => ui.colored_label(Color32::from_rgb(60, 170, 80), "Running"),
                SessionState::Stopped(None) => ui.label("Stopped"),
                SessionState::Stopped(Some(_)) => ui.colored_label(Color32::from_rgb(220, 60, 60), "Error"),
            };
        });
        if let SessionState::Stopped(Some(e)) = &self.state {
            ui.colored_label(Color32::from_rgb(220, 60, 60), e);
        }
        if let Some(i) = &self.info {
            egui::CollapsingHeader::new(format!("{} · {}", i.model, i.serial)).default_open(false).show(ui, |ui| {
                let (pw, ph) = i.effective_pixel_um();
                ui.label(format!("{} × {} px, {}-bit", i.width, i.height, i.bit_depth));
                ui.label(format!("Pixel {pw:.2} × {ph:.2} µm (binning {} × {})", i.bin_x, i.bin_y));
                if !i.firmware.is_empty() {
                    ui.label(format!("Firmware {}", i.firmware));
                }
                if let Some(c) = &i.color {
                    ui.label(format!("Colour sensor, Bayer {}", c.phase.name()));
                    ui.label(format!("Analysed per 2×2 cell: {:.2} × {:.2} µm", 2.0 * pw, 2.0 * ph));
                } else if !i.monochrome {
                    ui.colored_label(Color32::from_rgb(220, 120, 40), "Polarization sensor: raw data is analysed.");
                }
            });
        }

        if matches!(self.kind, SourceKind::Simulator | SourceKind::SimulatorColor) {
            ui.checkbox(&mut self.beam_blocked, "Block beam (simulator shutter)").on_hover_text("Block it while recording a dark frame");
            crate::sim::BEAM_BLOCKED.store(self.beam_blocked, std::sync::atomic::Ordering::Relaxed);
        }

        section(ui, "Exposure & gain");
        let (exp_min, exp_max, gain_rng, black_rng, led) = match &self.info {
            Some(i) => (
                (i.exposure_range_us.0 as f64 / 1000.0).max(0.001),
                (i.exposure_range_us.1 as f64 / 1000.0).min(5000.0),
                i.gain_range,
                i.black_level_range,
                i.led_supported,
            ),
            None => (0.04, 1000.0, (0, 480), (0, 100), false),
        };
        ui.add_enabled_ui(self.running(), |ui| {
            egui::Grid::new("cam").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                ui.label("Exposure (ms)");
                ui.add(egui::Slider::new(&mut self.settings.exposure_ms, exp_min..=exp_max.max(exp_min)).logarithmic(true).max_decimals(3));
                ui.end_row();
                ui.label("Gain (index)");
                ui.add_enabled(gain_rng.1 > gain_rng.0, egui::Slider::new(&mut self.settings.gain, gain_rng.0..=gain_rng.1));
                ui.end_row();
                ui.label("Black level");
                let mut b = self.settings.black_level.unwrap_or(black_rng.0);
                if ui.add_enabled(black_rng.1 > black_rng.0, egui::Slider::new(&mut b, black_rng.0..=black_rng.1)).changed() {
                    self.settings.black_level = Some(b);
                }
                ui.end_row();
                ui.label("LED");
                ui.add_enabled(led, egui::Checkbox::without_text(&mut self.settings.led_on));
                ui.end_row();
            });
        });
        if let Some(a) = &self.applied {
            ui.label(RichText::new(format!("Camera: {:.3} ms · gain {} ({:.1} dB) · black {}", a.exposure_ms, a.gain, a.gain_db, a.black_level)).small().weak());
        }
        if let Some(e) = &self.settings_error {
            ui.colored_label(Color32::from_rgb(220, 60, 60), e);
        }

        section(ui, "Analysis");
        egui::Grid::new("ana").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
            ui.label("ROI half-size (px)");
            ui.add(egui::DragValue::new(&mut self.controls.roi_half).range(0..=5000)).on_hover_text("0 = whole sensor");
            ui.end_row();
            if self.info.as_ref().is_some_and(|i| i.color.is_some()) {
                ui.label("Colour channel").on_hover_text(
                    "What each 2×2 Bayer cell contributes. Pick the channel of your laser colour for the best S/N; Sum uses all the light",
                );
                egui::ComboBox::from_id_salt("channel").selected_text(self.controls.channel.name()).show_ui(ui, |ui| {
                    for ch in Channel::ALL {
                        ui.selectable_value(&mut self.controls.channel, ch, ch.name());
                    }
                });
                ui.end_row();
            }
            ui.label("Min contrast (counts)").on_hover_text(
                "A beam must stand this far above the background (darkest corner) to be detected; 2/3 of it keeps it detected",
            );
            ui.add(egui::DragValue::new(&mut self.controls.min_contrast).range(5.0..=1000.0).speed(0.5));
            ui.end_row();
            ui.label("Min fit S/N").on_hover_text("Fitted peak height / RMS residual, per axis. A fit to pure noise gives about 3 to 4");
            ui.add(egui::DragValue::new(&mut self.controls.min_snr).range(2.0..=100.0).speed(0.1));
            ui.end_row();
            ui.label("Dark frame");
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(self.running() && !self.controls.paused, egui::Button::new(format!("Record {DARK_FRAMES}")))
                    .on_hover_text("Block the beam first")
                    .on_disabled_hover_text("Needs live frames: press Play")
                    .clicked()
                {
                    self.controls.dark = Some(DarkCmd::Record(DARK_FRAMES));
                }
                if ui.button("Clear").clicked() {
                    self.controls.dark = Some(DarkCmd::Clear);
                }
            });
            ui.end_row();
        });
        if !self.dark_status.is_empty() {
            ui.label(RichText::new(&self.dark_status).small().weak());
        }

        section(ui, "Display");
        egui::Grid::new("disp").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
            let z = &mut self.controls.zoom;
            ui.label("Display mode");
            egui::ComboBox::from_id_salt("mode")
                .selected_text(match z.mode {
                    ZoomMode::Auto => "Auto zoom",
                    ZoomMode::FullFrame => "Full frame",
                    ZoomMode::Lock => "Lock",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut z.mode, ZoomMode::Auto, "Auto zoom");
                    ui.selectable_value(&mut z.mode, ZoomMode::FullFrame, "Full frame");
                    ui.selectable_value(&mut z.mode, ZoomMode::Lock, "Lock");
                });
            ui.end_row();
            ui.label("Threshold (%)");
            ui.add(egui::DragValue::new(&mut z.threshold_pct).range(1.0..=90.0).speed(0.1));
            ui.end_row();
            ui.label("Max zoom (×)");
            ui.add(egui::DragValue::new(&mut z.max_zoom).range(1.0..=64.0).speed(0.1));
            ui.end_row();
            ui.label("Margin (×)");
            ui.add(egui::DragValue::new(&mut z.margin).range(0.0..=3.0).speed(0.01));
            ui.end_row();
            ui.label("Show");
            ui.vertical(|ui| {
                ui.checkbox(&mut self.show_full, "Full frame beside zoom");
                ui.checkbox(&mut self.show_outline, "Fit outline (1/e²)").on_hover_text("The fitted 1/e² ellipse with its width Dx and height Dy");
                if self.info.as_ref().is_some_and(|i| i.color.is_some()) {
                    ui.checkbox(&mut self.controls.color_view, "Colour image").on_hover_text(
                        "On: white-balanced RGB (clipped cells magenta). Off: the analysed channel in the palette (clipped red)",
                    );
                }
            });
            ui.end_row();
            ui.label("Palette");
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.controls.palette, Palette::Gray, "Gray");
                ui.radio_value(&mut self.controls.palette, Palette::Thermal, "Thermal");
            });
            ui.end_row();
        });

        section(ui, "Log");
        ui.checkbox(&mut self.log_enabled, "Log to CSV");
        ui.add_enabled(!self.log_enabled, egui::TextEdit::singleline(&mut self.log_path).desired_width(f32::INFINITY));
        self.controls.log_path = (self.log_enabled && !self.log_path.trim().is_empty()).then(|| PathBuf::from(self.log_path.trim()));
        if !self.log_status.is_empty() {
            ui.label(RichText::new(&self.log_status).small().weak());
        }

        self.export_ui(ui);
    }

    fn export_ui(&mut self, ui: &mut egui::Ui) {
        section(ui, "Record & export");
        let idle = self.controls.record.is_none();
        ui.horizontal(|ui| {
            ui.label("Folder");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(idle, egui::Button::new("Browse…")).on_hover_text("Folder for snapshots and recordings").clicked() {
                    self.actions.push(Action::BrowseFolder);
                }
                ui.add_enabled(idle, egui::TextEdit::singleline(&mut self.capture_dir).desired_width(f32::INFINITY))
                    .on_hover_text("File ▸ Open capture folder shows it in Explorer");
            });
        });
        let has_frame = self.running() && self.frame_size.is_some();
        egui::Grid::new("export").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
            ui.label("Snapshot saves");
            ui.vertical(|ui| {
                ui.checkbox(&mut self.snap_png, "Image (PNG)").on_hover_text("The full frame as displayed (palette or colour)");
                ui.checkbox(&mut self.snap_raw, "Raw frame (16-bit TIFF)").on_hover_text(
                    "The camera's counts, untouched (the Bayer mosaic on a colour camera). Opens calibrated in µm in ImageJ / Fiji",
                );
                ui.checkbox(&mut self.snap_csv, "Results + profiles (CSV)");
            });
            ui.end_row();
            ui.label("Video");
            ui.add_enabled_ui(idle, |ui| {
                egui::ComboBox::from_id_salt("video")
                    .width(ui.available_width())
                    .selected_text(self.video_format.name())
                    .show_ui(ui, |ui| {
                        for f in VideoFormat::ALL {
                            ui.selectable_value(&mut self.video_format, f, f.name()).on_hover_text(f.tooltip());
                        }
                    })
                    .response
                    .on_hover_text(format!("{}\nEach recording also gets a _frames.csv with the results of every frame in it.", self.video_format.tooltip()));
            });
            ui.end_row();
            ui.label("");
            ui.vertical(|ui| {
                let size = egui::vec2(ui.available_width(), 30.0);
                let image = egui::Button::new(RichText::new("📷  Save image").size(15.0)).min_size(size);
                if ui.add_enabled(has_frame, image).on_hover_text("Snapshot: saves the files ticked above (Ctrl+S)").clicked() {
                    self.actions.push(Action::Snapshot);
                }
                let video = if idle {
                    egui::Button::new(RichText::new("🎥  Save video").size(15.0).color(Color32::from_rgb(230, 70, 70)))
                } else {
                    egui::Button::new(RichText::new("⏹  Stop video").size(15.0).color(Color32::WHITE)).fill(Color32::from_rgb(190, 40, 40))
                };
                if ui.add_enabled(!idle || self.running(), video.min_size(size)).on_hover_text("Start / stop recording (Ctrl+R)").clicked() {
                    self.actions.push(Action::ToggleRecord);
                }
            });
            ui.end_row();
        });
        if let Some(r) = &self.recording {
            ui.label(RichText::new(record_text(r)).small().weak());
        }
    }

    fn record_button(&mut self, ui: &mut egui::Ui) {
        let rec = self.controls.record.is_some();
        let b = if rec {
            egui::Button::new(RichText::new("⏹ Stop recording").color(Color32::WHITE)).fill(Color32::from_rgb(190, 40, 40))
        } else {
            egui::Button::new(RichText::new("⏺ Record").color(Color32::from_rgb(230, 70, 70)))
        };
        if ui.add_enabled(rec || self.running(), b).on_hover_text("Ctrl+R").clicked() {
            self.actions.push(Action::ToggleRecord);
        }
    }

    // ----- toolbar and status bar -----

    fn toolbar_ui(&mut self, ui: &mut egui::Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                let has_frame = self.running() && self.frame_size.is_some();
                if ui.add_enabled(has_frame, egui::Button::new("📷 Snapshot").shortcut_text("Ctrl+S")).clicked() {
                    self.actions.push(Action::Snapshot);
                }
                let rec = if self.controls.record.is_some() { "⏹ Stop recording" } else { "⏺ Start recording" };
                if ui.add_enabled(self.running() || self.controls.record.is_some(), egui::Button::new(rec).shortcut_text("Ctrl+R")).clicked() {
                    self.actions.push(Action::ToggleRecord);
                }
                ui.separator();
                if ui.add_enabled(has_frame, egui::Button::new("Save full frame as PNG…")).clicked() {
                    self.actions.push(Action::SavePng(View::Full));
                }
                if ui.add_enabled(has_frame, egui::Button::new("Save zoom view as PNG…")).clicked() {
                    self.actions.push(Action::SavePng(View::Zoom));
                }
                if ui.add_enabled(has_frame, egui::Button::new("Save raw frame as 16-bit TIFF…")).clicked() {
                    self.actions.push(Action::SaveRaw);
                }
                if ui.add_enabled(self.analysis.is_some(), egui::Button::new("Export results + profiles to CSV…")).clicked() {
                    self.actions.push(Action::ExportCsv(Profiles::Both));
                }
                ui.separator();
                if ui.button("Open capture folder").clicked() {
                    self.actions.push(Action::OpenFolder);
                }
                if ui.button("Exit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("Help", |ui| {
                if ui.add(egui::Button::new("About · how the beam size is measured").shortcut_text("F1")).clicked() {
                    self.show_about = true;
                }
            });
            ui.separator();

            let paused = self.controls.paused;
            let (text, tip) = if paused { ("▶ Play", "Resume the live image (Space)") } else { ("⏸ Pause", "Freeze the image and the results (Space)") };
            let mut b = egui::Button::new(RichText::new(text).strong()).min_size(egui::vec2(84.0, 0.0));
            if paused {
                b = b.fill(Color32::from_rgb(200, 130, 20));
            }
            if ui.add_enabled(self.running(), b).on_hover_text(tip).clicked() {
                self.actions.push(Action::TogglePause);
            }
            let has_frame = self.running() && self.frame_size.is_some();
            if ui
                .add_enabled(has_frame, egui::Button::new("📷 Snapshot"))
                .on_hover_text("Save the frame on screen as PNG, plus the TIFF / CSV ticked under Record & export (Ctrl+S)")
                .clicked()
            {
                self.actions.push(Action::Snapshot);
            }
            self.record_button(ui);
            if let Some(r) = self.recording.as_ref().filter(|_| self.controls.record.is_some()) {
                let blink = (ui.input(|i| i.time) * 2.0) as i64 % 2 == 0;
                let dot = if blink && !paused { Color32::RED } else { Color32::from_gray(90) };
                ui.label(RichText::new("●").color(dot));
                ui.label(RichText::new(record_text(r)).monospace());
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
            }
            if paused {
                ui.label(RichText::new("PAUSED").strong().color(Color32::from_rgb(240, 150, 40)));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("ℹ About").on_hover_text("How pixels become µm, and the fit (F1)").clicked() {
                    self.show_about = true;
                }
            });
        });
    }

    fn status_ui(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| match &self.status {
            Some((t, msg, err)) => {
                ui.label(RichText::new(t).weak().monospace());
                let text = RichText::new(msg);
                ui.label(if *err { text.color(Color32::from_rgb(230, 70, 70)) } else { text });
            }
            None => {
                ui.label(RichText::new("Right-click an image or a plot to save, copy or export it.").weak());
            }
        });
    }

    fn set_status(&mut self, msg: String, err: bool) {
        self.status = Some((chrono::Local::now().format("%H:%M:%S").to_string(), msg, err));
    }

    fn capture_dir(&self) -> PathBuf {
        PathBuf::from(self.capture_dir.trim())
    }

    fn results_meta(&self) -> ResultsMeta<'_> {
        ResultsMeta { info: self.info.as_ref(), applied: self.applied, unit: self.unit(), pixel_um: self.analysis_pixel_um() }
    }

    /// Size of one analysis pixel in µm (a 2 x 2 cell on a colour camera).
    fn analysis_pixel_um(&self) -> (f64, f64) {
        match &self.info {
            Some(i) => {
                let (w, h) = i.effective_pixel_um();
                if i.color.is_some() { (2.0 * w, 2.0 * h) } else { (w, h) }
            }
            None => (f64::NAN, f64::NAN),
        }
    }

    fn save_dialog(&self, frame: &eframe::Frame, title: &str, filter: (&str, &str), name: String) -> Option<PathBuf> {
        let dir = self.capture_dir();
        let _ = std::fs::create_dir_all(&dir);
        rfd::FileDialog::new()
            .set_parent(frame)
            .set_title(title)
            .set_directory(&dir)
            .set_file_name(name)
            .add_filter(filter.0, &[filter.1])
            .save_file()
    }

    /// Everything the menus and buttons asked for during this frame.
    fn run_actions(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        for a in std::mem::take(&mut self.actions) {
            match a {
                Action::TogglePause => {
                    if self.running() {
                        self.controls.paused = !self.controls.paused;
                    }
                }
                Action::Snapshot => {
                    if !(self.snap_png || self.snap_raw || self.snap_csv) {
                        self.set_status("Snapshot: tick at least one of PNG, raw TIFF or CSV under Record & export".into(), true);
                    } else if self.running() {
                        let stem = export::unique_stem(&self.capture_dir(), "snapshot");
                        self.controls.exports.push(ExportCmd::Snapshot { stem, png: self.snap_png, raw: self.snap_raw, csv: self.snap_csv });
                    }
                }
                Action::ToggleRecord => {
                    if self.controls.record.is_some() {
                        self.controls.record = None;
                        self.set_status("Finishing the recording…".into(), false);
                    } else if self.running() {
                        // A recording is of live frames.
                        self.controls.paused = false;
                        self.next_record_id += 1;
                        let stem = export::unique_stem(&self.capture_dir(), "recording");
                        let path = export::with_suffix(&stem, &format!(".{}", self.video_format.ext()));
                        self.set_status(format!("Recording to {}", path.display()), false);
                        self.controls.record = Some(RecordSpec { id: self.next_record_id, stem, format: self.video_format });
                    }
                }
                Action::SavePng(view) => {
                    let what = if view == View::Full { "frame" } else { "zoom" };
                    let name = format!("{what}_{}.png", export::timestamp());
                    if let Some(path) = self.save_dialog(frame, "Save image as PNG", ("PNG image", "png"), name) {
                        self.controls.exports.push(ExportCmd::Png { path, view });
                    }
                }
                Action::SaveRaw => {
                    let name = format!("raw_{}.tif", export::timestamp());
                    if let Some(path) = self.save_dialog(frame, "Save raw frame (16-bit TIFF)", ("TIFF image", "tif"), name) {
                        self.controls.exports.push(ExportCmd::Raw { path });
                    }
                }
                Action::CopyImage(view) => self.controls.exports.push(ExportCmd::Copy { view }),
                Action::SaveScreen(rect, what) => {
                    let name = format!("{what}_{}.png", export::timestamp());
                    if let Some(path) = self.save_dialog(frame, "Save as PNG (as on screen)", ("PNG image", "png"), name) {
                        self.shot = Some((1, ShotRequest { rect, path }));
                        ctx.request_repaint();
                    }
                }
                Action::ExportCsv(which) => {
                    let Some(a) = &self.analysis else { continue };
                    let what = match which {
                        Profiles::X => "profile_x",
                        Profiles::Y => "profile_y",
                        Profiles::Both | Profiles::None => "results",
                    };
                    let table = export::results_table(a, &self.results_meta(), which, ',');
                    let name = format!("{what}_{}.csv", export::timestamp());
                    if let Some(path) = self.save_dialog(frame, "Export to CSV", ("CSV table", "csv"), name) {
                        match export::save_text(&path, &table) {
                            Ok(()) => self.set_status(format!("Saved {}", path.display()), false),
                            Err(e) => self.set_status(e, true),
                        }
                    }
                }
                Action::CopyTable(which) => {
                    if let Some(a) = &self.analysis {
                        ctx.copy_text(export::results_table(a, &self.results_meta(), which, '\t'));
                        self.set_status("Copied to the clipboard (tab-separated, pastes into Excel)".into(), false);
                    }
                }
                Action::ResetProfile(i) => self.reset_profile[i] = true,
                Action::OpenFolder => {
                    let dir = self.capture_dir();
                    if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::process::Command::new("explorer").arg(&dir).spawn()) {
                        self.set_status(format!("Cannot open {}: {e}", dir.display()), true);
                    }
                }
                Action::BrowseFolder => {
                    let picked = rfd::FileDialog::new()
                        .set_parent(frame)
                        .set_title("Folder for snapshots and recordings")
                        .set_directory(self.capture_dir())
                        .pick_folder();
                    if let Some(d) = picked {
                        self.capture_dir = d.display().to_string();
                    }
                }
            }
        }
    }

    /// Asks for a pending screenshot, and saves the crop of one that arrived.
    fn handle_screenshots(&mut self, ctx: &egui::Context) {
        if let Some((wait, _)) = &mut self.shot {
            if *wait > 0 {
                *wait -= 1;
                ctx.request_repaint();
            } else if let Some((_, req)) = self.shot.take() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(Arc::new(req))));
            }
        }
        let shots: Vec<(egui::UserData, Arc<egui::ColorImage>)> = ctx.input(|i| {
            i.raw
                .events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Screenshot { user_data, image, .. } => Some((user_data.clone(), image.clone())),
                    _ => None,
                })
                .collect()
        });
        for (data, image) in shots {
            let Some(req) = data.data.as_ref().and_then(|d| d.downcast_ref::<Arc<ShotRequest>>()) else { continue };
            let crop = image.region(&req.rect, Some(ctx.pixels_per_point()));
            match export::save_png(&req.path, &crop) {
                Ok(()) => self.set_status(format!("Saved {}", req.path.display()), false),
                Err(e) => self.set_status(e, true),
            }
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        if ctx.input_mut(|i| i.consume_shortcut(&SHORTCUT_SNAPSHOT)) {
            self.actions.push(Action::Snapshot);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&SHORTCUT_RECORD)) {
            self.actions.push(Action::ToggleRecord);
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F1)) {
            self.show_about = !self.show_about;
        }
        // Space is Play / Pause unless a text field is being edited.
        if !ctx.egui_wants_keyboard_input() && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Space)) {
            self.actions.push(Action::TogglePause);
        }
    }

    /// The right-click menu of the full-frame and the zoom image.
    fn image_menu(&mut self, ui: &mut egui::Ui, view: View, rect: Rect) {
        let has_frame = self.frame_size.is_some() && self.running();
        let has_result = self.analysis.is_some();
        let running = self.running();
        let paused = self.controls.paused;
        let what = if view == View::Full { "full frame" } else { "zoom view" };
        let shot = if view == View::Full { "full_view" } else { "zoom_view" };
        let items: [(bool, String, &str, Action); 5] = [
            (has_frame, format!("Save {what} as PNG…"), "The picture at full resolution, as displayed (palette or colour)", Action::SavePng(view)),
            (has_frame, "Save this view with overlays as PNG…".into(), "Exactly what is on screen here: axes, zoom box, fit outline", Action::SaveScreen(rect, shot)),
            (has_frame, "Save raw frame as 16-bit TIFF…".into(), "The camera's counts, untouched; calibrated in µm for ImageJ / Fiji", Action::SaveRaw),
            (has_frame, format!("Copy {what} to clipboard"), "As displayed, full resolution", Action::CopyImage(view)),
            (has_result, "Export results + profiles to CSV…".into(), "Dx, Dy, σ, centre, fit parameters and both profiles", Action::ExportCsv(Profiles::Both)),
        ];
        self.menu_items(ui, items);
        ui.separator();
        self.menu_items(
            ui,
            [
                (running, if paused { "▶ Play" } else { "⏸ Pause" }.into(), "Space", Action::TogglePause),
                (has_frame, "📷 Snapshot".into(), "Ctrl+S", Action::Snapshot),
            ],
        );
        ui.separator();
        ui.checkbox(&mut self.show_outline, "Fit outline (1/e²)");
        ui.checkbox(&mut self.show_full, "Full frame beside zoom");
        ui.menu_button("Palette", |ui| {
            ui.radio_value(&mut self.controls.palette, Palette::Gray, "Gray");
            ui.radio_value(&mut self.controls.palette, Palette::Thermal, "Thermal");
        });
        if view == View::Zoom {
            ui.menu_button("Display mode", |ui| {
                let z = &mut self.controls.zoom.mode;
                ui.radio_value(z, ZoomMode::Auto, "Auto zoom");
                ui.radio_value(z, ZoomMode::FullFrame, "Full frame");
                ui.radio_value(z, ZoomMode::Lock, "Lock");
            });
        }
    }

    /// The right-click menu of a profile plot (`axis` 0 = X, 1 = Y).
    fn profile_menu(&mut self, ui: &mut egui::Ui, axis: usize, rect: Rect) {
        let has = self.analysis.is_some();
        let (one, name) = if axis == 0 { (Profiles::X, "X") } else { (Profiles::Y, "Y") };
        let shot = if axis == 0 { "plot_x" } else { "plot_y" };
        self.menu_items(
            ui,
            [
                (has, format!("Export {name} profile to CSV…"), "Position (px and µm), data and fit, with the results", Action::ExportCsv(one)),
                (has, "Export both profiles to CSV…".into(), "With the results", Action::ExportCsv(Profiles::Both)),
                (has, format!("Copy {name} profile data"), "Tab-separated: pastes into Excel or Origin", Action::CopyTable(one)),
                (has, "Copy results only".into(), "Dx, Dy, σ, centre, fit parameters", Action::CopyTable(Profiles::None)),
                (has, "Save plot as PNG…".into(), "Exactly what is on screen here", Action::SaveScreen(rect, shot)),
            ],
        );
        ui.separator();
        let pause = if self.controls.paused { "▶ Play" } else { "⏸ Pause" };
        self.menu_items(
            ui,
            [
                (true, "Reset view".into(), "Fit the axes to the data again (also: double-click)", Action::ResetProfile(axis)),
                (self.running(), pause.into(), "Space", Action::TogglePause),
            ],
        );
    }

    /// Menu buttons: (enabled, text, hover text, action). A click queues the action and closes the menu.
    fn menu_items<const N: usize>(&mut self, ui: &mut egui::Ui, items: [(bool, String, &str, Action); N]) {
        for (enabled, text, tip, action) in items {
            let mut r = ui.add_enabled(enabled, egui::Button::new(text));
            if !tip.is_empty() {
                r = r.on_hover_text(tip);
            }
            if r.clicked() {
                self.actions.push(action);
                ui.close();
            }
        }
    }

    fn about_live(&self) -> Option<about::Live> {
        let i = self.info.as_ref()?;
        let r = self.analysis.as_ref().map(|a| &a.result);
        let both = |f: fn(&beam::AxisResult) -> f64| r.filter(|r| r.detected()).map(|r| (f(&r.x), f(&r.y)));
        Some(about::Live {
            model: format!("{} · {}", i.model, i.serial),
            sensor_um: (i.pixel_w_um, i.pixel_h_um),
            binning: (i.bin_x.max(1), i.bin_y.max(1)),
            bayer: i.color.is_some(),
            pixel_um: self.analysis_pixel_um(),
            unit: self.unit(),
            sigma: both(|a| a.sigma()),
            sigma2: both(|a| a.sigma2),
            snr: both(|a| a.snr),
            roi: r.map(|r| (r.roi.3, r.roi.2)),
            saturated: r.is_some_and(|r| r.saturated),
        })
    }

    // ----- top panel -----

    fn results_ui(&self, ui: &mut egui::Ui) {
        let u = self.unit();
        let a = self.analysis.as_ref();
        let r = a.map(|a| &a.result);
        let num = |v: Option<f64>, d: usize| v.filter(|v| v.is_finite()).map_or("—".to_owned(), |v| format!("{v:.d$}"));
        ui.horizontal(|ui| {
            for (label, v) in [("Dx 1/e²", r.map(|r| r.dx_um)), ("Dy 1/e²", r.map(|r| r.dy_um))] {
                ui.vertical(|ui| {
                    ui.label(RichText::new(label).weak());
                    ui.label(RichText::new(format!("{} µm", num(v, 1))).size(30.0).strong());
                });
                ui.add_space(24.0);
            }
            if let Some(nb) = r.and_then(|r| r.no_beam.as_ref()) {
                ui.vertical(|ui| {
                    ui.label(RichText::new("No beam").size(22.0).strong().color(Color32::from_rgb(240, 150, 40)));
                    ui.label(RichText::new(nb.to_string()).small().color(Color32::from_rgb(240, 150, 40)));
                });
                ui.add_space(12.0);
            }
            ui.separator();
            egui::Grid::new("res").num_columns(4).spacing([18.0, 2.0]).show(ui, |ui| {
                ui.label(RichText::new(format!("σx / σy ({u})")).weak());
                ui.label(format!("{} / {}", num(r.map(|r| r.x.sigma()), 2), num(r.map(|r| r.y.sigma()), 2)));
                ui.label(RichText::new(format!("Center x / y ({u})")).weak());
                ui.label(format!("{} / {}", num(r.map(|r| r.x.center()), 1), num(r.map(|r| r.y.center()), 1)));
                ui.end_row();
                ui.label(RichText::new(format!("σ₂x / σ₂y ({u})")).weak()).on_hover_text("Second-moment width; should agree with the fit within a few % on a clean beam");
                ui.label(format!("{} / {}", num(r.map(|r| r.x.sigma2), 2), num(r.map(|r| r.y.sigma2), 2)));
                ui.label(RichText::new("Frame · FPS").weak());
                ui.label(format!("{} · {}", a.map_or("—".into(), |a| a.frame_count.to_string()), num(a.map(|a| a.fps), 1)));
                ui.end_row();
                ui.label(RichText::new("Peak (counts)").weak());
                ui.horizontal(|ui| {
                    ui.label(r.map_or("—".into(), |r| r.peak.to_string()));
                    let sat = r.is_some_and(|r| r.saturated);
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                    ui.painter().circle_filled(rect.center(), 6.0, if sat { Color32::RED } else { Color32::from_gray(70) });
                    ui.label(if sat { RichText::new("Saturated").color(Color32::RED) } else { RichText::new("Saturated").weak() });
                });
                ui.label(RichText::new("Model").weak());
                ui.label(self.info.as_ref().map_or("—", |i| i.model.as_str()));
                ui.end_row();
            });
        });
    }

    // ----- centre -----

    fn plots_ui(&mut self, ui: &mut egui::Ui) {
        let total = ui.available_height();
        let image_h = (total * 0.62).max(220.0);
        let width = ui.available_width();

        ui.allocate_ui(egui::vec2(width, image_h), |ui| {
            ui.horizontal_top(|ui| {
                if self.show_full {
                    let full_w = (width * 0.36).floor();
                    let resp = ui
                        .allocate_ui(egui::vec2(full_w, image_h), |ui| {
                            ui.set_width(full_w);
                            ui.vertical(|ui| self.full_plot(ui, image_h)).inner
                        })
                        .inner;
                    let rect = resp.rect;
                    resp.context_menu(|ui| self.image_menu(ui, View::Full, rect));
                }
                let resp = ui.vertical(|ui| self.zoom_plot(ui, image_h)).inner;
                let rect = resp.rect;
                resp.context_menu(|ui| self.image_menu(ui, View::Zoom, rect));
            });
        });

        let u = self.unit();
        let reset = std::mem::take(&mut self.reset_profile);
        let resps = ui.columns(2, |cols| {
            let r = self.analysis.as_ref().map(|a| &a.result);
            [
                profile_plot(&mut cols[0], "X profile", &format!("column ({u})"), r.map(|r| &r.x), reset[0]),
                profile_plot(&mut cols[1], "Y profile", &format!("row ({u})"), r.map(|r| &r.y), reset[1]),
            ]
        });
        for (axis, resp) in resps.into_iter().enumerate() {
            let rect = resp.rect;
            resp.context_menu(|ui| self.profile_menu(ui, axis, rect));
        }
    }

    /// The whole sensor, with the zoom window drawn on it.
    fn full_plot(&self, ui: &mut egui::Ui, height: f32) -> egui::Response {
        ui.label(RichText::new("Full frame").strong());
        // Fit the graph to the sensor's shape, so it has no empty bands (about 60 px go to the axes).
        let (rows, cols) = self.frame_size.unwrap_or((1080, 1440));
        let plot_h = ((ui.available_width() - 60.0) * rows as f32 / cols as f32 + 45.0).clamp(80.0, height - 24.0);
        image_plot("full", plot_h, self.unit()).show(ui, |pui| {
            let (Some(tex), Some((rows, cols))) = (&self.full_texture, self.frame_size) else { return };
            let (h, w) = (rows as f64, cols as f64);
            pui.image(PlotImage::new("full frame", tex.id(), PlotPoint::new(w / 2.0, -h / 2.0), egui::vec2(w as f32, h as f32)));
            if let Some(win) = self.window {
                let (c0, r0) = (win.col0 as f64, win.row0 as f64);
                let (c1, r1) = (c0 + win.cols as f64, r0 + win.rows as f64);
                let rect = vec![[c0, -r0], [c1, -r0], [c1, -r1], [c0, -r1], [c0, -r0]];
                pui.line(Line::new("zoom window", rect).color(ZOOM_COLOR).width(1.5));
            }
            if self.show_outline {
                if let Some(o) = self.outline {
                    pui.line(Line::new("fit", ellipse(&o)).color(OUTLINE_COLOR).width(1.0));
                }
            }
            pui.set_plot_bounds(PlotBounds::from_min_max([0.0, -h], [w, 0.0]));
        })
        .response
    }

    /// The auto-zoomed crop, with the optional fit outline.
    fn zoom_plot(&mut self, ui: &mut egui::Ui, height: f32) -> egui::Response {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Zoom").strong());
            if self.controls.paused && self.state == SessionState::Running {
                let frame = self.analysis.as_ref().map_or(String::new(), |a| format!(" · frame {} held", a.frame_count));
                ui.label(RichText::new(format!("⏸ PAUSED{frame}")).strong().color(Color32::from_rgb(240, 150, 40)));
            }
            ui.label(RichText::new(&self.zoom_text).small().monospace());
        });
        let resp = image_plot("zoom", height - 24.0, self.unit()).show(ui, |pui| {
            let (Some(tex), Some(w)) = (&self.texture, self.window) else { return };
            let (c0, r0, cols, rows) = (w.col0 as f64, w.row0 as f64, w.cols as f64, w.rows as f64);
            pui.image(PlotImage::new("frame", tex.id(), PlotPoint::new(c0 + cols / 2.0, -(r0 + rows / 2.0)), egui::vec2(cols as f32, rows as f32)));
            if self.show_outline {
                if let Some(o) = self.outline {
                    draw_outline(pui, &o, rows, self.unit());
                }
            }
            pui.set_plot_bounds(PlotBounds::from_min_max([c0, -(r0 + rows)], [c0 + cols, -r0]));
        });
        let rect = resp.response.rect;
        if rect.height() > 10.0 {
            self.controls.plot_aspect = (rect.width() / rect.height()) as f64;
        }
        resp.response
    }
}

const ZOOM_COLOR: Color32 = Color32::from_rgb(255, 160, 40);
const OUTLINE_COLOR: Color32 = Color32::from_rgb(0, 220, 255);

/// A fixed, pixel-square image graph: rows grow downwards (plot y = -row).
fn image_plot(id: &str, height: f32, unit: &str) -> Plot<'static> {
    Plot::new(id)
        .height(height)
        .data_aspect(1.0)
        .allow_drag(false)
        .allow_zoom(false)
        .allow_scroll(false)
        .allow_boxed_zoom(false)
        .allow_double_click_reset(false)
        .show_grid(false)
        .x_axis_label(format!("column ({unit})"))
        .y_axis_label(format!("row ({unit})"))
        .y_axis_formatter(|m, _| format!("{:.0}", -m.value + 0.0))
        .label_formatter(|h| {
            let (HoverPosition::NearDataPoint { position: p, .. } | HoverPosition::Elsewhere { position: p }) = *h;
            Some(format!("col {:.0}, row {:.0}", p.x.floor(), (-p.y).floor()))
        })
}

/// The fitted 1/e² ellipse in plot coordinates.
fn ellipse(o: &Outline) -> Vec<[f64; 2]> {
    (0..=120)
        .map(|i| {
            let t = std::f64::consts::TAU * i as f64 / 120.0;
            [o.cx + o.rx * t.cos(), -(o.cy + o.ry * t.sin())]
        })
        .collect()
}

/// The 1/e² ellipse, its centre, and the width (Dx) and height (Dy) as dimension lines with labels.
fn draw_outline(pui: &mut PlotUi, o: &Outline, view_rows: f64, unit: &str) {
    let (cx, cy, rx, ry) = (o.cx, -o.cy, o.rx, o.ry);
    let dash = LineStyle::Dashed { length: 6.0 };
    pui.line(Line::new("1/e² outline", ellipse(o)).color(OUTLINE_COLOR).width(1.5));
    pui.line(Line::new("Dx", vec![[cx - rx, cy], [cx + rx, cy]]).color(OUTLINE_COLOR).width(1.0).style(dash));
    pui.line(Line::new("Dy", vec![[cx, cy - ry], [cx, cy + ry]]).color(OUTLINE_COLOR).width(1.0).style(dash));
    let ends = vec![[cx - rx, cy], [cx + rx, cy], [cx, cy - ry], [cx, cy + ry]];
    pui.points(Points::new("ends", ends).radius(3.0).color(OUTLINE_COLOR));
    let gap = 0.02 * view_rows;
    let label = |s: String| RichText::new(s).color(OUTLINE_COLOR).size(15.0).strong();
    pui.text(
        Text::new("Dx label", PlotPoint::new(cx, cy + ry + gap), label(format!("Dx {:.1} µm ({:.1} {unit})", o.dx_um, 2.0 * rx)))
            .anchor(egui::Align2::CENTER_BOTTOM),
    );
    pui.text(
        Text::new("Dy label", PlotPoint::new(cx + rx + gap, cy), label(format!("Dy {:.1} µm\n({:.1} {unit})", o.dy_um, 2.0 * ry)))
            .anchor(egui::Align2::LEFT_CENTER),
    );
}

fn profile_plot(ui: &mut egui::Ui, title: &str, xlabel: &str, axis: Option<&beam::AxisResult>, reset: bool) -> egui::Response {
    ui.label(RichText::new(title).strong());
    let mut plot = Plot::new(title).legend(Legend::default()).x_axis_label(xlabel).allow_scroll(false);
    if reset {
        plot = plot.reset();
    }
    plot.show(ui, |pui| {
            let Some(a) = axis else { return };
            let pts = |v: &[f64]| -> Vec<[f64; 2]> { v.iter().enumerate().map(|(i, &y)| [(a.x0 + i) as f64, y]).collect() };
            pui.points(Points::new("Data", pts(&a.profile)).radius(1.5).color(Color32::from_rgb(90, 160, 230)));
            if !a.fit_curve.is_empty() {
                pui.line(Line::new("Fit", pts(&a.fit_curve)).color(Color32::from_rgb(240, 130, 40)).width(2.0));
            }
        })
        .response
}

fn record_text(r: &RecStats) -> String {
    let secs = r.secs.max(0.0);
    let dropped = if r.dropped > 0 { format!(" ({} dropped)", r.dropped) } else { String::new() };
    format!("REC {:02}:{:04.1} · {} frames{dropped} · {:.1} MB", (secs / 60.0) as u64, secs % 60.0, r.frames, r.bytes as f64 / 1e6)
}

/// `Pictures\ThorCam Beam Profiler` of the user, else `captures` next to the program.
fn default_capture_dir() -> String {
    let pictures = std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join("Pictures")).filter(|p| p.is_dir());
    let dir = match pictures {
        Some(p) => p.join("ThorCam Beam Profiler"),
        None => std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("captures"))).unwrap_or_else(|| PathBuf::from("captures")),
    };
    dir.display().to_string()
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(8.0);
    ui.label(RichText::new(title).strong());
    ui.separator();
}

fn default_log_path() -> String {
    let dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())).unwrap_or_default();
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    dir.join(format!("beam_log_{secs}.csv")).display().to_string()
}

impl eframe::App for ThorCamApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.sync_from_workers(&ctx);
        self.handle_screenshots(&ctx);
        self.handle_keys(&ctx);

        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar_ui(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_ui(ui));
        egui::Panel::left("controls").resizable(false).exact_size(300.0).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| self.controls_ui(ui));
        });
        let results = egui::Panel::top("results")
            .show(ui, |ui| {
                // Registered before the labels, so they keep their hover texts; right-click anywhere else.
                let bg = ui.interact(ui.max_rect(), ui.id().with("results_menu"), egui::Sense::click());
                ui.add_space(4.0);
                self.results_ui(ui);
                ui.add_space(4.0);
                bg
            })
            .inner;
        results.context_menu(|ui| {
            if ui.add_enabled(self.analysis.is_some(), egui::Button::new("Copy results")).clicked() {
                self.actions.push(Action::CopyTable(Profiles::None));
                ui.close();
            }
            if ui.add_enabled(self.analysis.is_some(), egui::Button::new("Export results + profiles to CSV…")).clicked() {
                self.actions.push(Action::ExportCsv(Profiles::Both));
                ui.close();
            }
        });
        egui::CentralPanel::default().show(ui, |ui| self.plots_ui(ui));

        let live = self.about_live();
        about::window(&ctx, &mut self.show_about, live.as_ref());

        self.run_actions(&ctx, frame);
        self.push_to_workers();
    }
}

impl Drop for ThorCamApp {
    fn drop(&mut self) {
        // Stop the threads (this disarms and closes the camera), then close the SDK.
        self.session = None;
        source::release_sdk();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs one frame of a central panel holding `plot`, with the given input events; returns whether a context
    /// menu is open on the plot afterwards.
    fn frame(ctx: &egui::Context, events: Vec<egui::Event>, plot: &dyn Fn(&mut egui::Ui) -> egui::Response) -> bool {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0))),
            events,
            ..Default::default()
        };
        let mut open = false;
        let mut out = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let r = plot(ui);
                r.context_menu(|ui| {
                    ui.label("menu");
                });
                open = r.context_menu_opened();
            });
        });
        out.textures_delta.clear();
        open
    }

    fn right_click_opens_menu(plot: &dyn Fn(&mut egui::Ui) -> egui::Response) -> bool {
        let ctx = egui::Context::default();
        let at = egui::pos2(400.0, 300.0);
        let button = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Secondary, pressed, modifiers: Modifiers::NONE };
        frame(&ctx, vec![], plot);
        frame(&ctx, vec![egui::Event::PointerMoved(at)], plot);
        frame(&ctx, vec![button(true)], plot);
        frame(&ctx, vec![button(false)], plot);
        frame(&ctx, vec![], plot)
    }

    #[test]
    fn right_click_opens_the_profile_menu() {
        let axis = beam::AxisResult::default();
        assert!(right_click_opens_menu(&|ui| profile_plot(ui, "X profile", "column (px)", Some(&axis), false)));
    }

    #[test]
    fn right_click_opens_the_image_menu() {
        assert!(right_click_opens_menu(&|ui| image_plot("zoom", 500.0, "px").show(ui, |_| {}).response));
    }
}

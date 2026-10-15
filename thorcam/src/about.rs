//! The About window: how camera pixels become a beam diameter in µm, and the Gaussian fit behind it, worked
//! through with the numbers of the beam on screen.

use eframe::egui::{self, Color32, RichText};
use egui_plot::{Line, LineStyle, Plot, PlotPoint, Text};

/// Numbers of the current measurement, for the worked example.
#[derive(Clone, Debug, Default)]
pub struct Live {
    pub model: String,
    /// Sensor pixel pitch, µm (without binning).
    pub sensor_um: (f64, f64),
    pub binning: (u32, u32),
    /// Colour sensor: one analysis pixel is a 2 x 2 Bayer cell.
    pub bayer: bool,
    /// Size of one analysis pixel, µm.
    pub pixel_um: (f64, f64),
    /// "px" or "cell".
    pub unit: &'static str,
    /// Fitted sigma, analysis pixels; `None` without a beam.
    pub sigma: Option<(f64, f64)>,
    pub sigma2: Option<(f64, f64)>,
    pub snr: Option<(f64, f64)>,
    pub roi: Option<(usize, usize)>,
    pub saturated: bool,
}

const ACCENT: Color32 = Color32::from_rgb(0, 190, 230);

pub fn window(ctx: &egui::Context, open: &mut bool, live: Option<&Live>) {
    egui::Window::new("About · how the beam size is measured")
        .open(open)
        .default_size([720.0, 760.0])
        .min_width(480.0)
        .resizable(true)
        .collapsible(false)
        .vscroll(true)
        .show(ctx, |ui| contents(ui, live));
}

fn heading(ui: &mut egui::Ui, n: usize, text: &str) {
    ui.add_space(14.0);
    ui.label(RichText::new(format!("{n}. {text}")).size(17.0).strong());
    ui.add_space(2.0);
}

/// An equation on its own line, in a shaded box.
fn eq(ui: &mut egui::Ui, text: &str) {
    egui::Frame::group(ui.style()).fill(ui.visuals().faint_bg_color).inner_margin(egui::Margin::symmetric(12, 6)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(text).monospace().size(15.0));
    });
}

fn para(ui: &mut egui::Ui, text: &str) {
    ui.label(text);
}

fn contents(ui: &mut egui::Ui, live: Option<&Live>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("ThorCam Beam Profiler").size(22.0).strong());
        ui.label(RichText::new(concat!("v", env!("CARGO_PKG_VERSION"))).weak());
    });
    para(
        ui,
        "Measures the width of a laser beam that falls directly on the camera sensor: one Gaussian fit per axis gives \
         the 1/e² diameters Dx and Dy in micrometres. This page shows every step from camera counts to µm.",
    );

    heading(ui, 1, "From a pixel to a micrometre");
    para(ui, "The camera reports its pixel pitch p (for the CS165 family 3.45 µm). Binning b joins b pixels into one, and a colour sensor is analysed per 2 × 2 Bayer cell:");
    eq(ui, "p_eff  = p × b                    (monochrome sensor)\np_cell = 2 × p × b                (colour sensor, one 2 × 2 Bayer cell)");
    para(ui, "Every position and width the program fits is in these analysis pixels (\"px\", or \"cell\" on a colour camera). Multiplying by p_eff turns them into µm.");

    heading(ui, 2, "From the image to two profiles");
    para(
        ui,
        "The brightest pixel is found on a despeckled image (a lone hot pixel cannot win). Around it a square ROI of \
         2h × 2h pixels is cut out (h = \"ROI half-size\"; 0 = whole sensor). A recorded dark frame D is subtracted. \
         Then every column and every row of the ROI is summed:",
    );
    eq(ui, "X(c) = Σ_r [ I(r, c) − D(r, c) ]      sum down each column  →  X profile\nY(r) = Σ_c [ I(r, c) − D(r, c) ]      sum along each row    →  Y profile");
    para(
        ui,
        "Why sums: a Gaussian beam I(x, y) = I₀ · exp(−2x²/wx² − 2y²/wy²) separates into an x part and a y part, so \
         summing over y leaves a Gaussian in x with exactly the same width. Summing N rows also lowers the noise by about √N.",
    );

    heading(ui, 3, "The fit");
    para(ui, "Each profile is fitted with a Gaussian on a constant background:");
    eq(ui, "f(x) = A · exp( −(x − x₀)² / (2σ²) ) + B");
    egui::Grid::new("about_params").num_columns(2).spacing([16.0, 2.0]).show(ui, |ui| {
        for (sym, what) in [
            ("A", "amplitude: height of the peak above the background (summed counts)"),
            ("x₀", "centre of the beam (px)"),
            ("σ", "standard deviation: the width parameter (px)"),
            ("B", "offset: background level of the profile"),
        ] {
            ui.label(RichText::new(sym).monospace().strong());
            ui.label(what);
            ui.end_row();
        }
    });
    para(ui, "The four parameters minimise the sum of squared residuals over the n profile points,");
    eq(ui, "χ²(A, x₀, σ, B) = Σ_i [ y_i − f(x_i) ]²");
    para(
        ui,
        "solved by Levenberg–Marquardt: a Gauss–Newton step (J^T·J + λ·diag J^T·J) Δ = J^T·r with an adaptive damping λ, \
         at most 200 iterations. Start values: A = max − min, x₀ = position of the maximum, \
         σ = (points above half maximum) / 2.355, B = min.",
    );

    heading(ui, 4, "From σ to the beam size");
    para(ui, "A laser beam is usually written I(x) = I₀ · exp(−2x² / w²). Comparing with f(x) gives w = 2σ:");
    eq(
        ui,
        "1/e² radius     w = 2σ          (the intensity is e^−2 = 13.5 % of the peak at x₀ ± w)\n\
         1/e² diameter   D = 4σ          (= the ISO 11146 \"D4σ\" width of a Gaussian beam)\n\
         FWHM              = 2√(2 ln 2)·σ ≈ 2.3548 σ,   so D ≈ 1.699 × FWHM",
    );
    eq(ui, "Dx [µm] = 4 · σx [px] · p_x [µm/px]\nDy [µm] = 4 · σy [px] · p_y [µm/px]");
    gaussian_figure(ui);

    heading(ui, 5, "Worked example: the beam on screen");
    match live {
        Some(l) => worked_example(ui, l),
        None => {
            ui.label(RichText::new("Connect a camera (or the simulator) to see the numbers of a real measurement here.").italics());
        }
    }

    heading(ui, 6, "Checks before a width is shown");
    para(ui, "Widths are only shown when the frame really holds a beam; otherwise the top bar says \"No beam\" and why:");
    for t in [
        "Contrast: the peak stands at least \"Min contrast\" counts above the background (the darkest 32 × 32 corner).",
        "Fit quality: S/N = A / √(mean residual²) of both fits reaches \"Min fit S/N\" (a fit to pure noise gives 3 to 4).",
        "Geometry: σ ≥ 0.5 px, the centre x₀ lies inside the ROI, and the ROI holds the 1/e² diameter (4σ ≤ ROI size).",
    ] {
        ui.horizontal_wrapped(|ui| {
            ui.label("•");
            ui.label(t);
        });
    }
    para(ui, "As a cross-check the second-moment width (ISO 11146) is computed over x₀ ± 6σ after removing the fitted offset B:");
    eq(ui, "σ₂² = Σ (x − x_c)² · (y(x) − B) / Σ (y(x) − B)
x_c = Σ x · (y(x) − B) / Σ (y(x) − B)          (centroid)");
    para(ui, "On a clean Gaussian beam σ₂ agrees with σ within a few %. A σ₂ much larger than σ means wings, side lobes or uneven background: the beam is not Gaussian and D4σ (= 4σ₂) is the more honest width.");

    heading(ui, 7, "Getting an accurate number");
    for t in [
        "The beam must hit the sensor directly. With a lens or a beam expander in between, divide by its magnification M.",
        "Keep the peak below saturation (red pixels, the Saturated lamp): a clipped top makes the fitted beam too wide.",
        "Sample the beam well: σ of at least 2 to 3 pixels (D of 10 px or more). Smaller beams are limited by the pixel size.",
        "Make the ROI about 3 × D wide so the wings and the background are both in the fit.",
        "Record a dark frame (beam blocked) to remove fixed-pattern offsets; a uniform background is taken by B anyway.",
        "Dx and Dy are along the sensor's columns and rows. A rotated elliptical beam gives its projections on these axes.",
    ] {
        ui.horizontal_wrapped(|ui| {
            ui.label("•");
            ui.label(t);
        });
    }

    heading(ui, 8, "Shortcuts");
    egui::Grid::new("about_keys").num_columns(2).spacing([16.0, 2.0]).show(ui, |ui| {
        for (k, what) in [
            ("Space", "Play / Pause"),
            ("Ctrl + S", "Snapshot (PNG; TIFF / CSV if ticked)"),
            ("Ctrl + R", "Start / stop recording"),
            ("F1", "This window"),
            ("Right-click", "Image or plot menu: save, copy, export CSV"),
        ] {
            ui.label(RichText::new(k).monospace().strong());
            ui.label(what);
            ui.end_row();
        }
    });
    ui.add_space(8.0);
}

fn worked_example(ui: &mut egui::Ui, l: &Live) {
    let (px, py) = l.pixel_um;
    let u = l.unit;
    egui::Grid::new("about_live").num_columns(2).spacing([16.0, 3.0]).striped(true).show(ui, |ui| {
        ui.label("Camera");
        ui.label(&l.model);
        ui.end_row();
        ui.label("Pixel pitch p");
        ui.label(format!("{:.2} × {:.2} µm", l.sensor_um.0, l.sensor_um.1));
        ui.end_row();
        ui.label("Binning b");
        ui.label(format!("{} × {}", l.binning.0, l.binning.1));
        ui.end_row();
        ui.label("Analysis pixel");
        let how = if l.bayer {
            format!("2 × {:.2} × {} = {px:.2} µm per cell (colour sensor)", l.sensor_um.0, l.binning.0)
        } else {
            format!("{:.2} × {} = {px:.2} µm per px", l.sensor_um.0, l.binning.0)
        };
        ui.label(how);
        ui.end_row();
        if let Some((w, h)) = l.roi {
            ui.label("ROI");
            ui.label(format!("{w} × {h} {u}"));
            ui.end_row();
        }
    });
    ui.add_space(4.0);
    let Some((sx, sy)) = l.sigma else {
        ui.label(RichText::new("No beam detected right now, so there is no fit to work through.").italics());
        return;
    };
    let line = |axis: &str, s: f64, p: f64| {
        format!(
            "{axis}:  σ = {s:.2} {u}  →  D = 4 × {s:.2} × {p:.3} µm = {:.1} µm
    w = 2σ·p = {:.1} µm,  FWHM = 2.3548σ·p = {:.1} µm",
            4.0 * s * p,
            2.0 * s * p,
            2.354_820_045 * s * p
        )
    };
    eq(ui, &format!("{}\n{}", line("X", sx, px), line("Y", sy, py)));
    if let Some((s2x, s2y)) = l.sigma2 {
        let pct = |a: f64, b: f64| 100.0 * (a - b) / b;
        ui.label(format!(
            "Second-moment check: σ₂x = {s2x:.2} {u} ({:+.1} %), σ₂y = {s2y:.2} {u} ({:+.1} %) against the fit.",
            pct(s2x, sx),
            pct(s2y, sy)
        ));
    }
    if let Some((nx, ny)) = l.snr {
        ui.label(format!("Fit S/N: {nx:.0} (X), {ny:.0} (Y)."));
    }
    let min_s = sx.min(sy);
    if min_s < 2.0 {
        ui.colored_label(Color32::from_rgb(240, 150, 40), format!("σ is only {min_s:.1} {u}: the beam is close to the pixel size, the width is less certain."));
    }
    if l.saturated {
        ui.colored_label(Color32::RED, "The beam is saturated: the width is overestimated. Reduce exposure or gain.");
    }
}

/// The normalised Gaussian with the FWHM and the 1/e² diameter marked.
fn gaussian_figure(ui: &mut egui::Ui) {
    ui.add_space(6.0);
    let g = |x: f64| (-0.5 * x * x).exp();
    let curve: Vec<[f64; 2]> = (0..=400).map(|i| -4.0 + 8.0 * i as f64 / 400.0).map(|x| [x, g(x)]).collect();
    let e2 = (-2.0f64).exp();
    let hw = (2.0 * 2.0f64.ln()).sqrt(); // half width at half maximum, in sigma
    let dash = LineStyle::Dashed { length: 6.0 };
    let orange = Color32::from_rgb(240, 130, 40);
    Plot::new("about_gauss")
        .height(230.0)
        .allow_drag(false)
        .allow_zoom(false)
        .allow_scroll(false)
        .allow_boxed_zoom(false)
        .show_grid(false)
        .x_axis_label("(x − x₀) / σ")
        .y_axis_label("f / A")
        .include_y(1.12)
        .include_y(-0.05)
        .show(ui, |pui| {
            pui.line(Line::new("Gaussian", curve).color(ACCENT).width(2.0));
            pui.line(Line::new("50 %", vec![[-4.0, 0.5], [4.0, 0.5]]).color(Color32::GRAY).style(dash));
            pui.line(Line::new("13.5 %", vec![[-4.0, e2], [4.0, e2]]).color(Color32::GRAY).style(dash));
            pui.line(Line::new("FWHM", vec![[-hw, 0.5], [hw, 0.5]]).color(orange).width(2.5));
            pui.line(Line::new("D = 4σ", vec![[-2.0, e2], [2.0, e2]]).color(Color32::from_rgb(80, 200, 120)).width(2.5));
            for x in [-2.0, 2.0] {
                pui.line(Line::new("w", vec![[x, 0.0], [x, e2]]).color(Color32::from_rgb(80, 200, 120)).style(dash));
            }
            pui.line(Line::new("σ", vec![[0.0, g(1.0)], [1.0, g(1.0)]]).color(Color32::from_rgb(200, 120, 220)).width(2.0));
            let label = |s: &str, c: Color32| RichText::new(s).color(c).size(14.0).strong();
            pui.text(Text::new("t1", PlotPoint::new(0.0, 0.47), label("FWHM ≈ 2.355 σ", orange)).anchor(egui::Align2::CENTER_TOP));
            pui.text(Text::new("t2", PlotPoint::new(0.0, e2 + 0.06), label("1/e² diameter D = 4σ  (2w)", Color32::from_rgb(80, 200, 120))));
            pui.text(Text::new("t3", PlotPoint::new(1.15, g(1.0)), label("σ", Color32::from_rgb(200, 120, 220))).anchor(egui::Align2::LEFT_CENTER));
            pui.text(Text::new("t4", PlotPoint::new(3.0, 0.5), label("50 %", Color32::GRAY)).anchor(egui::Align2::CENTER_BOTTOM));
            pui.text(Text::new("t5", PlotPoint::new(3.0, e2), label("13.5 % = 1/e²", Color32::GRAY)).anchor(egui::Align2::CENTER_BOTTOM));
        });
}

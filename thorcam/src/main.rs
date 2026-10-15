//! ThorCam beam profiler: a Thorlabs scientific camera, a Gaussian fit per axis and an auto-zoomed image.
//!
//! Run with `--sim` to use the built-in simulated camera instead of the Thorlabs SDK, or `--sim-color` for the
//! simulated colour (Bayer) camera.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod about;
mod app;
mod export;
mod pipeline;
mod sdk_files;
mod sim;
mod slot;
mod source;

use eframe::egui;

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().collect();
    // Hidden helper mode, started at exit when a DLL could not be deleted yet (see sdk_files).
    if let [_, flag, dir, pid] = args.as_slice() {
        if flag == "--cleanup" {
            if let Ok(pid) = pid.parse() {
                sdk_files::run_cleanup_helper(std::path::Path::new(dir), pid);
            }
            return Ok(());
        }
    }
    let start = if args.iter().any(|a| a == "--sim-color") {
        Some(source::SourceKind::SimulatorColor)
    } else if args.iter().any(|a| a == "--sim") {
        Some(source::SourceKind::Simulator)
    } else {
        None
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ThorCam Beam Profiler")
            .with_inner_size([1400.0, 920.0])
            .with_min_inner_size([960.0, 640.0]),
        ..Default::default()
    };
    let result = eframe::run_native("ThorCam Beam Profiler", options, Box::new(move |cc| Ok(Box::new(app::ThorCamApp::new(cc, start)))));
    // The window is closed and the app dropped (camera closed). Close the SDK, then remove the unpacked DLLs.
    source::release_sdk();
    sdk_files::cleanup_at_exit();
    result
}

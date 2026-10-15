//! Where frames come from: a Thorlabs camera through the native SDK, or the simulator.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tlcam::{Applied, CameraInfo, CameraSettings, Frame, Sdk};

use crate::sdk_files;
use crate::sim::SimCamera;

/// Everything the acquisition thread needs from a camera.
pub trait CameraSource: Send {
    fn info(&self) -> &CameraInfo;
    fn start(&mut self) -> Result<(), String>;
    fn apply(&mut self, s: &CameraSettings) -> Result<Applied, String>;
    /// Waits briefly (about 20 ms at most) for the next frame.
    fn next_frame(&mut self) -> Result<Option<Frame>, String>;
}

impl CameraSource for tlcam::Camera {
    fn info(&self) -> &CameraInfo {
        tlcam::Camera::info(self)
    }
    fn start(&mut self) -> Result<(), String> {
        tlcam::Camera::start(self).map_err(|e| e.0)
    }
    fn apply(&mut self, s: &CameraSettings) -> Result<Applied, String> {
        tlcam::Camera::apply(self, s).map_err(|e| e.0)
    }
    fn next_frame(&mut self) -> Result<Option<Frame>, String> {
        self.poll_frame().map_err(|e| e.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Thorlabs,
    Simulator,
    /// The simulator with a Bayer colour sensor and a red laser.
    SimulatorColor,
}

/// Keeps the SDK open between connections: `tl_camera_open_sdk` should run once per process.
static SDK: Mutex<Option<Arc<Sdk>>> = Mutex::new(None);

pub fn open(kind: SourceKind, dll_dir: Option<&Path>) -> Result<Box<dyn CameraSource>, String> {
    match kind {
        SourceKind::Simulator => Ok(Box::new(SimCamera::new(false))),
        SourceKind::SimulatorColor => Ok(Box::new(SimCamera::new(true))),
        SourceKind::Thorlabs => {
            let dir = dll_dir.ok_or_else(|| {
                format!("Thorlabs SDK DLLs not found. Put the files of Native_{}_lib in a folder named {} next to the program.", usize::BITS, dll_folder_name())
            })?;
            let sdk = {
                let mut keep = SDK.lock().unwrap_or_else(|e| e.into_inner());
                match &*keep {
                    Some(s) => s.clone(),
                    None => {
                        let s = Sdk::open(dir).map_err(|e| e.0)?;
                        *keep = Some(s.clone());
                        s
                    }
                }
            };
            Ok(Box::new(tlcam::Camera::open(&sdk, 0).map_err(|e| e.0)?))
        }
    }
}

/// Closes the SDK once no camera uses it any more. Call at program exit, after every camera is closed.
pub fn release_sdk() {
    SDK.lock().unwrap_or_else(|e| e.into_inner()).take();
}

pub fn dll_folder_name() -> &'static str {
    if cfg!(target_pointer_width = "64") {
        "Library_X64"
    } else {
        "Library_X86"
    }
}

fn has_sdk(dir: &Path) -> bool {
    dir.join("thorlabs_tsi_camera_sdk.dll").is_file()
}

/// Finds the native SDK DLLs, in this order:
/// 1. the `THORCAM_DLL_DIR` environment variable;
/// 2. `Library_X64` (or `Library_X86`) next to the program, so newer DLLs can be dropped in without a rebuild;
/// 3. the DLLs embedded in the program, unpacked for this run (see [`crate::sdk_files`]);
/// 4. the Thorlabs SDK folder in a parent directory (when running from `target/release` in this repository).
pub fn find_dll_dir() -> Result<PathBuf, String> {
    if let Some(d) = std::env::var_os("THORCAM_DLL_DIR").map(PathBuf::from) {
        if has_sdk(&d) {
            return Ok(d);
        }
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe_dir = exe.parent().ok_or("no program folder")?;
    let local = exe_dir.join(dll_folder_name());
    if has_sdk(&local) {
        return Ok(local);
    }
    let unpack_error = if !sdk_files::has_embedded() {
        None
    } else {
        match sdk_files::unpack() {
            Ok(d) => return Ok(d),
            Err(e) => Some(e),
        }
    };
    let sdk_rel = Path::new("Thorlabs_SDK/Scientific Camera Interfaces/SDK/Native Toolkit/dlls")
        .join(if cfg!(target_pointer_width = "64") { "Native_64_lib" } else { "Native_32_lib" });
    if let Some(d) = exe_dir.ancestors().map(|a| a.join(&sdk_rel)).find(|d| has_sdk(d)) {
        return Ok(d);
    }
    Err(unpack_error.unwrap_or_else(|| format!("Thorlabs SDK DLLs not found. Put them in a folder named {} next to the program.", dll_folder_name())))
}

//! The Thorlabs SDK DLLs embedded in the program: unpacked for one run, removed again at exit.
//!
//! Each run unpacks to `%LOCALAPPDATA%\ThorCamBeamProfiler\run-<process id>\` and keeps `.lock` in it open with no
//! sharing, so other copies of the program can tell the folder is in use. At exit the folder is deleted. If Windows
//! still has one of the DLLs mapped (the SDK can keep its own plugins loaded until the process ends), the program
//! starts a hidden copy of itself with `--cleanup`, which waits for this process to end and then deletes the folder.
//! At start-up, folders of runs that are no longer alive (a crash, a power cut) are removed, as are the
//! `sdk-<hash>` folders of earlier versions. Nothing else on the disk is touched.

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/embedded_sdk.rs"));
}

const APP_DIR: &str = "ThorCamBeamProfiler";
const LOCK: &str = ".lock";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;
const SYNCHRONIZE: u32 = 0x0010_0000;

#[link(name = "kernel32")]
extern "system" {
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
    fn WaitForSingleObject(handle: *mut c_void, ms: u32) -> u32;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

/// This run's folder and its lock, held open until [`cleanup_at_exit`].
static RUN: Mutex<Option<(PathBuf, File)>> = Mutex::new(None);

pub fn has_embedded() -> bool {
    !embedded::SDK_FILES.is_empty()
}

fn app_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join(APP_DIR)
}

/// Unpacks the embedded SDK for this run and returns the folder. Repeated calls return the same folder.
pub fn unpack() -> Result<PathBuf, String> {
    let mut run = RUN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((dir, _)) = &*run {
        return Ok(dir.clone());
    }
    let base = app_dir();
    remove_stale(&base);
    let dir = base.join(format!("run-{}", std::process::id()));
    let fail = |e: std::io::Error| format!("Cannot unpack the Thorlabs SDK to {}: {e}", dir.display());
    std::fs::create_dir_all(&dir).map_err(fail)?;
    let lock = OpenOptions::new().write(true).create(true).truncate(true).share_mode(0).open(dir.join(LOCK)).map_err(fail)?;
    for (name, bytes) in embedded::SDK_FILES {
        std::fs::write(dir.join(name), bytes).map_err(fail)?;
    }
    *run = Some((dir.clone(), lock));
    Ok(dir)
}

/// Removes this program's leftovers that no running copy uses: `run-*` folders whose lock can be taken, and the
/// `sdk-*` folders of earlier versions.
fn remove_stale(base: &Path) {
    let Ok(entries) = std::fs::read_dir(base) else { return };
    for e in entries.flatten() {
        let path = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if !path.is_dir() {
            continue;
        }
        if name.starts_with("run-") {
            // Opening the lock with no sharing fails while its run is alive.
            let in_use = OpenOptions::new().write(true).share_mode(0).open(path.join(LOCK)).is_err() && path.join(LOCK).exists();
            if in_use {
                continue;
            }
        } else if !name.starts_with("sdk-") {
            continue;
        }
        let _ = std::fs::remove_dir_all(&path);
    }
}

/// Call once the camera and the SDK are closed, just before the program ends.
pub fn cleanup_at_exit() {
    let Some((dir, lock)) = RUN.lock().unwrap_or_else(|e| e.into_inner()).take() else { return };
    drop(lock);
    // The SDK made the folder the current directory; Windows does not delete a folder that is in use as one.
    let _ = std::env::set_current_dir(std::env::temp_dir());
    if std::fs::remove_dir_all(&dir).is_ok() {
        remove_if_empty(&app_dir());
        return;
    }
    // A DLL is still mapped: let a hidden copy of the program finish the job after this process has ended.
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe)
            .arg("--cleanup")
            .arg(&dir)
            .arg(std::process::id().to_string())
            .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
            .spawn();
    }
}

/// The `--cleanup <folder> <pid>` mode: waits until process `pid` has ended, then deletes `folder`.
/// Refuses any folder that is not a `run-*` folder of this program.
pub fn run_cleanup_helper(dir: &Path, pid: u32) {
    let ours = dir.parent() == Some(app_dir().as_path()) && dir.file_name().is_some_and(|n| n.to_string_lossy().starts_with("run-"));
    if !ours {
        return;
    }
    unsafe {
        let h = OpenProcess(SYNCHRONIZE, 0, pid);
        if !h.is_null() {
            WaitForSingleObject(h, 30_000);
            CloseHandle(h);
        }
    }
    let t = Instant::now();
    while dir.exists() && t.elapsed() < Duration::from_secs(10) {
        if std::fs::remove_dir_all(dir).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    remove_if_empty(&app_dir());
}

fn remove_if_empty(dir: &Path) {
    // remove_dir only succeeds on an empty folder, so a running copy's files are never touched.
    let _ = std::fs::remove_dir(dir);
}

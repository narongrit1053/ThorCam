//! Embeds the Thorlabs camera SDK DLLs in the executable, so `thorcam.exe` runs as a single file.
//!
//! Source folder: `THORCAM_SDK_DIR`, or the SDK's `Native_64_lib` / `Native_32_lib` in this repository.
//! The colour and polarization processing DLLs (172 of the 175 MB) are left out: this program never loads them,
//! and the camera SDK does not depend on them. Without an SDK folder the build still succeeds, with nothing embedded.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Big optional processing modules of the SDK, not needed to run a camera.
const SKIP_PREFIXES: [&str; 2] = ["thorlabs_tsi_color_processing", "thorlabs_tsi_polarization_processor"];

fn main() {
    println!("cargo:rerun-if-env-changed=THORCAM_SDK_DIR");
    let bits = std::env::var("CARGO_CFG_TARGET_POINTER_WIDTH").unwrap_or_else(|_| "64".into());
    let dir = std::env::var_os("THORCAM_SDK_DIR").map(PathBuf::from).unwrap_or_else(|| {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../Thorlabs_SDK/Scientific Camera Interfaces/SDK/Native Toolkit/dlls")
            .join(if bits == "32" { "Native_32_lib" } else { "Native_64_lib" })
    });
    println!("cargo:rerun-if-changed={}", dir.display());

    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    files.retain(|p| {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_ascii_lowercase();
        p.is_file() && (name.ends_with(".dll") || name.ends_with(".cfg")) && !SKIP_PREFIXES.iter().any(|s| name.starts_with(s))
    });
    files.sort();
    if !files.iter().any(|p| p.file_name().is_some_and(|n| n == "thorlabs_tsi_camera_sdk.dll")) {
        println!("cargo:warning=Thorlabs SDK not found in {}; the executable will need a Library_X64 folder.", dir.display());
        files.clear();
    }

    let mut code = String::from("/// (file name, contents) of the embedded SDK files.\npub const SDK_FILES: &[(&str, &[u8])] = &[\n");
    for p in &files {
        println!("cargo:rerun-if-changed={}", p.display());
        let name = p.file_name().unwrap().to_str().unwrap();
        let abs = std::fs::canonicalize(p).unwrap();
        let _ = writeln!(code, "    ({name:?}, include_bytes!({:?})),", abs.display().to_string().trim_start_matches(r"\\?\"));
    }
    code.push_str("];\n");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("embedded_sdk.rs");
    std::fs::write(out, code).expect("write embedded_sdk.rs");
}

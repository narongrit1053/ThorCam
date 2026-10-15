//! Raw access to `thorlabs_tsi_camera_sdk.dll`.
//!
//! This follows Thorlabs' own `load_dll_helpers/tl_camera_sdk_load.c`: load the DLL, look up the exported
//! `get_function_from_shared_library__or_nullptr`, and use it to fetch every `tl_camera_*` function by name.
//! The signatures are those of `Native Toolkit/include/tl_camera_sdk.h`.

use std::ffi::{c_char, c_int, c_uint, c_void, CStr};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

pub type Handle = *mut c_void;

/// `TL_CAMERA_OPERATION_MODE_SOFTWARE_TRIGGERED`
pub const OPERATION_MODE_SOFTWARE_TRIGGERED: c_int = 0;
/// `TL_CAMERA_SENSOR_TYPE_MONOCHROME`
pub const SENSOR_TYPE_MONOCHROME: c_int = 0;
/// `TL_CAMERA_SENSOR_TYPE_BAYER`
pub const SENSOR_TYPE_BAYER: c_int = 1;

const DLL_NAME: &str = "thorlabs_tsi_camera_sdk.dll";
const LOAD_WITH_ALTERED_SEARCH_PATH: u32 = 0x0000_0008;

#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryExW(name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
    fn FreeLibrary(module: *mut c_void) -> c_int;
    fn SetDllDirectoryW(path: *const u16) -> c_int;
}

type GetFunction = unsafe extern "C" fn(*mut c_void, *mut c_char) -> *mut c_void;

macro_rules! api {
    ($($name:ident : fn($($arg:ty),*) -> $ret:ty;)*) => {
        /// Function pointers into the SDK. Field names are the C names without the `tl_camera_` prefix.
        #[allow(dead_code)]
        pub struct Api {
            $(pub $name: unsafe extern "C" fn($($arg),*) -> $ret,)*
        }

        impl Api {
            unsafe fn resolve(get: GetFunction, module: *mut c_void) -> Result<Self, String> {
                Ok(Self {
                    $($name: {
                        let cname = concat!("tl_camera_", stringify!($name), "\0");
                        let p = get(module, cname.as_ptr() as *mut c_char);
                        if p.is_null() {
                            return Err(format!("{} has no function {}", DLL_NAME, &cname[..cname.len() - 1]));
                        }
                        std::mem::transmute::<*mut c_void, unsafe extern "C" fn($($arg),*) -> $ret>(p)
                    },)*
                })
            }
        }
    };
}

api! {
    open_sdk: fn() -> c_int;
    close_sdk: fn() -> c_int;
    get_last_error: fn() -> *const c_char;
    discover_available_cameras: fn(*mut c_char, c_int) -> c_int;
    open_camera: fn(*mut c_char, *mut Handle) -> c_int;
    close_camera: fn(Handle) -> c_int;

    get_model: fn(Handle, *mut c_char, c_int) -> c_int;
    get_serial_number: fn(Handle, *mut c_char, c_int) -> c_int;
    get_firmware_version: fn(Handle, *mut c_char, c_int) -> c_int;
    get_camera_sensor_type: fn(Handle, *mut c_int) -> c_int;
    get_color_filter_array_phase: fn(Handle, *mut c_int) -> c_int;
    get_color_correction_matrix: fn(Handle, *mut f32) -> c_int;
    get_default_white_balance_matrix: fn(Handle, *mut f32) -> c_int;
    get_image_width: fn(Handle, *mut c_int) -> c_int;
    get_image_height: fn(Handle, *mut c_int) -> c_int;
    get_sensor_pixel_width: fn(Handle, *mut f64) -> c_int;
    get_sensor_pixel_height: fn(Handle, *mut f64) -> c_int;
    get_bit_depth: fn(Handle, *mut c_int) -> c_int;
    get_binx: fn(Handle, *mut c_int) -> c_int;
    get_biny: fn(Handle, *mut c_int) -> c_int;

    get_exposure_time_range: fn(Handle, *mut i64, *mut i64) -> c_int;
    get_exposure_time: fn(Handle, *mut i64) -> c_int;
    set_exposure_time: fn(Handle, i64) -> c_int;
    get_gain_range: fn(Handle, *mut c_int, *mut c_int) -> c_int;
    get_gain: fn(Handle, *mut c_int) -> c_int;
    set_gain: fn(Handle, c_int) -> c_int;
    convert_gain_to_decibels: fn(Handle, c_int, *mut f64) -> c_int;
    get_black_level_range: fn(Handle, *mut c_int, *mut c_int) -> c_int;
    get_black_level: fn(Handle, *mut c_int) -> c_int;
    set_black_level: fn(Handle, c_int) -> c_int;
    get_is_led_supported: fn(Handle, *mut c_int) -> c_int;
    get_is_led_on: fn(Handle, *mut c_int) -> c_int;
    set_is_led_on: fn(Handle, c_int) -> c_int;
    set_is_hot_pixel_correction_enabled: fn(Handle, c_int) -> c_int;

    set_operation_mode: fn(Handle, c_int) -> c_int;
    set_frames_per_trigger_zero_for_unlimited: fn(Handle, c_uint) -> c_int;
    set_image_poll_timeout: fn(Handle, c_int) -> c_int;
    arm: fn(Handle, c_int) -> c_int;
    disarm: fn(Handle) -> c_int;
    issue_software_trigger: fn(Handle) -> c_int;
    get_pending_frame_or_null: fn(Handle, *mut *mut u16, *mut c_int, *mut *mut u8, *mut c_int) -> c_int;
}

/// The loaded DLL and its function table. Unloaded on drop.
pub struct Library {
    module: *mut c_void,
    pub api: Api,
}

// The SDK functions may be called from any thread; each camera handle is used by one thread at a time.
unsafe impl Send for Library {}
unsafe impl Sync for Library {}

impl Library {
    /// Loads `thorlabs_tsi_camera_sdk.dll` from `dir`.
    ///
    /// The SDK loads its device DLLs (`thorlabs_tsi_zelux_camera_device.dll`, `thorlabs_ccd_*.dll`, ...) at run
    /// time from the current directory, so `dir` also becomes the DLL search directory and the current directory.
    /// This is the same thing `Set_DLL_Folder.vi` does in the LabVIEW version.
    pub fn load(dir: &Path) -> Result<Self, String> {
        let path = dir.join(DLL_NAME);
        if !path.is_file() {
            return Err(format!("{} not found in {}", DLL_NAME, dir.display()));
        }
        let wide = |p: &Path| p.as_os_str().encode_wide().chain(Some(0)).collect::<Vec<u16>>();
        unsafe {
            // Every Thorlabs DLL needs the Microsoft Visual C++ runtime; say so plainly when it is missing.
            let rt = LoadLibraryExW(wide(Path::new("msvcp140.dll")).as_ptr(), std::ptr::null_mut(), 0);
            if rt.is_null() {
                return Err(format!(
                    "The Microsoft Visual C++ 2015-2022 runtime ({}) is not installed; the Thorlabs SDK needs it.                      Install ThorCam, or the runtime from https://aka.ms/vs/17/release/vc_redist.{}.exe",
                    if cfg!(target_pointer_width = "64") { "x64" } else { "x86" },
                    if cfg!(target_pointer_width = "64") { "x64" } else { "x86" },
                ));
            }
            FreeLibrary(rt);
            SetDllDirectoryW(wide(dir).as_ptr());
            let _ = std::env::set_current_dir(dir);
            let module = LoadLibraryExW(wide(&path).as_ptr(), std::ptr::null_mut(), LOAD_WITH_ALTERED_SEARCH_PATH);
            if module.is_null() {
                return Err(format!(
                    "could not load {} ({}). Check that the DLL matches this program's bitness ({}-bit).",
                    path.display(),
                    std::io::Error::last_os_error(),
                    usize::BITS
                ));
            }
            let get = GetProcAddress(module, c"get_function_from_shared_library__or_nullptr".as_ptr());
            if get.is_null() {
                FreeLibrary(module);
                return Err(format!("{DLL_NAME} has no get_function_from_shared_library__or_nullptr export"));
            }
            let get: GetFunction = std::mem::transmute::<*mut c_void, GetFunction>(get);
            match Api::resolve(get, module) {
                Ok(api) => Ok(Self { module, api }),
                Err(e) => {
                    FreeLibrary(module);
                    Err(e)
                }
            }
        }
    }

    /// The SDK's description of the last error.
    pub fn last_error(&self) -> String {
        unsafe {
            let p = (self.api.get_last_error)();
            if p.is_null() {
                String::new()
            } else {
                CStr::from_ptr(p).to_string_lossy().trim().to_owned()
            }
        }
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        unsafe {
            FreeLibrary(self.module);
        }
    }
}

//! USB protocol trace without a USB sniffer: runs the real SDK and logs every transfer it hands to the Cypress
//! `CYUSB3` driver.
//!
//! The Thorlabs DLLs talk to the camera with `DeviceIoControl` on the driver's device handle. This example patches
//! the import tables of the loaded Thorlabs DLLs so those calls (and the calls that report their completion) go
//! through logging wrappers first, then drives the camera through a fixed script with a marker before each step.
//!
//! cargo run -p tlcam --example usbtrace -- "<folder with thorlabs_tsi_camera_sdk.dll>" <log file>

use std::ffi::{c_char, c_void, CStr};
use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

type Bool = i32;

#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: *mut c_void,
}

#[repr(C)]
struct OverlappedEntry {
    completion_key: usize,
    overlapped: *mut Overlapped,
    internal: usize,
    bytes: u32,
}

#[link(name = "kernel32")]
extern "system" {
    fn GetModuleHandleA(name: *const c_char) -> *mut c_void;
    fn VirtualProtect(addr: *mut c_void, size: usize, prot: u32, old: *mut u32) -> Bool;
    fn GetCurrentThreadId() -> u32;
}

const PAGE_READWRITE: u32 = 0x04;
const STATUS_PENDING: usize = 0x103;

// ---- log -------------------------------------------------------------------------------------------------------

static LOG: Mutex<Option<std::fs::File>> = Mutex::new(None);
/// Raw image-endpoint packets (`<log>.raw`), to check the frame decoding offline.
static RAW: Mutex<Option<std::fs::File>> = Mutex::new(None);
static T0: Mutex<Option<Instant>> = Mutex::new(None);

fn log(line: &str) {
    let t = T0.lock().unwrap().map(|t| t.elapsed().as_secs_f64() * 1000.0).unwrap_or(0.0);
    let s = format!("{t:10.3} ms [t{:5}] {line}\n", unsafe { GetCurrentThreadId() });
    if let Some(f) = LOG.lock().unwrap().as_mut() {
        let _ = f.write_all(s.as_bytes());
    }
}

fn mark(step: &str) {
    println!("== {step}");
    log(&format!("==================== {step}"));
}

/// Hex + ASCII of up to `max` bytes.
fn dump(bytes: &[u8], max: usize) -> String {
    let n = bytes.len().min(max);
    let mut s = String::new();
    for b in &bytes[..n] {
        let _ = write!(s, "{b:02x} ");
    }
    if bytes.len() > n {
        let _ = write!(s, "... (+{} bytes)", bytes.len() - n);
    }
    let text: String = bytes[..n].iter().map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' }).collect();
    format!("{s} |{text}|")
}

// ---- CyUSB3 transfer decoding ----------------------------------------------------------------------------------
//
// `SINGLE_TRANSFER` (CyAPI, packed): SETUP_PACKET{bmRequest u8, bRequest u8, wValue u16, wIndex u16, wLength u16,
// ulTimeOut u32} = 12 bytes, reserved u8 @12, ucEndpointAddress u8 @13, NtStatus u32 @14, UsbdStatus u32 @18,
// IsoPacketOffset u32 @22, IsoPacketLength u32 @26, BufferOffset u32 @30, BufferLength u32 @34; header = 38 bytes.

const IOCTL_EP0: u32 = 0x22_0020; // IOCTL_ADAPT_SEND_EP0_CONTROL_TRANSFER
const IOCTL_NON_EP0: u32 = 0x22_0024; // IOCTL_ADAPT_SEND_NON_EP0_TRANSFER

fn rd_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// IN transfers waiting for completion: (OVERLAPPED*, buffer, buffer size, description).
static PENDING: Mutex<Vec<(usize, usize, usize, String)>> = Mutex::new(Vec::new());
/// Number of large (image) IN packets logged per endpoint, so the log stays readable.
static BIG_LOGGED: AtomicUsize = AtomicUsize::new(0);
const BIG_LOG_LIMIT: usize = 40;
static BIG_COUNT: AtomicUsize = AtomicUsize::new(0);
static BIG_BYTES: AtomicUsize = AtomicUsize::new(0);

fn describe(code: u32, inbuf: &[u8]) -> (String, Option<(u8, usize, usize)>) {
    if (code == IOCTL_EP0 || code == IOCTL_NON_EP0) && inbuf.len() >= 38 {
        let ep = inbuf[13];
        let off = rd_u32(inbuf, 30) as usize;
        let len = rd_u32(inbuf, 34) as usize;
        if code == IOCTL_EP0 {
            let d = format!(
                "EP0 bmReq=0x{:02x} bReq=0x{:02x} wValue=0x{:04x} wIndex=0x{:04x} wLength={} timeout={}",
                inbuf[0],
                inbuf[1],
                rd_u16(inbuf, 2),
                rd_u16(inbuf, 4),
                rd_u16(inbuf, 6),
                rd_u32(inbuf, 8)
            );
            return (d, Some((if inbuf[0] & 0x80 != 0 { 0x80 } else { 0 }, off, len)));
        }
        let dir = if ep & 0x80 != 0 { "IN " } else { "OUT" };
        return (format!("BULK {dir} ep=0x{ep:02x} len={len}"), Some((ep, off, len)));
    }
    (format!("IOCTL 0x{code:08x} in={}", inbuf.len()), None)
}

fn log_in_data(desc: &str, buf: usize, size: usize, got: usize) {
    let data = unsafe { std::slice::from_raw_parts(buf as *const u8, size) };
    if size < 38 {
        log(&format!("  <- {desc} done, {got} bytes: {}", dump(&data[..got.min(size)], 64)));
        return;
    }
    let off = rd_u32(data, 30) as usize;
    let len = rd_u32(data, 34) as usize;
    let nt = rd_u32(data, 14);
    let usbd = rd_u32(data, 18);
    let n = got.saturating_sub(off).min(len).min(size.saturating_sub(off));
    let payload = &data[off..off + n];
    if data[13] == 0x83 && n > 0 {
        if let Some(f) = RAW.lock().unwrap().as_mut() {
            // Each record: u32 length, then the packet exactly as received.
            let _ = f.write_all(&(n as u32).to_le_bytes());
            let _ = f.write_all(payload);
        }
    }
    if n > 1024 {
        BIG_COUNT.fetch_add(1, Ordering::Relaxed);
        BIG_BYTES.fetch_add(n, Ordering::Relaxed);
        if BIG_LOGGED.fetch_add(1, Ordering::Relaxed) >= BIG_LOG_LIMIT {
            return;
        }
    }
    log(&format!("  <- {desc} done nt=0x{nt:08x} usbd=0x{usbd:08x} got={n}: {}", dump(payload, if n > 1024 { 48 } else { 512 })));
}

/// Logs IN transfers that have completed since the last check.
fn sweep() {
    let mut p = PENDING.lock().unwrap();
    // Once the camera closes, the SDK frees the transfer buffers; reading them then crashes.
    if CLOSING.load(Ordering::Relaxed) {
        p.clear();
        return;
    }
    p.retain(|(ov, buf, size, desc)| {
        let o = unsafe { &*(*ov as *const Overlapped) };
        if o.internal == STATUS_PENDING {
            return true;
        }
        log_in_data(desc, *buf, *size, o.internal_high);
        false
    });
}

fn complete(ov: *mut Overlapped, bytes: usize) {
    let mut p = PENDING.lock().unwrap();
    if let Some(i) = p.iter().position(|e| e.0 == ov as usize) {
        let (_, buf, size, desc) = p.swap_remove(i);
        drop(p);
        log_in_data(&desc, buf, size, bytes);
    }
}

// ---- hooks -----------------------------------------------------------------------------------------------------

type DeviceIoControlFn =
    unsafe extern "system" fn(*mut c_void, u32, *mut c_void, u32, *mut c_void, u32, *mut u32, *mut Overlapped) -> Bool;
type CreateFileAFn = unsafe extern "system" fn(*const c_char, u32, u32, *mut c_void, u32, u32, *mut c_void) -> *mut c_void;
type GetOverlappedResultFn = unsafe extern "system" fn(*mut c_void, *mut Overlapped, *mut u32, Bool) -> Bool;
type GqcsExFn = unsafe extern "system" fn(*mut c_void, *mut OverlappedEntry, u32, *mut u32, u32, Bool) -> Bool;

static REAL_DIOC: AtomicUsize = AtomicUsize::new(0);
static REAL_CREATEFILE: AtomicUsize = AtomicUsize::new(0);
static REAL_GOR: AtomicUsize = AtomicUsize::new(0);
static REAL_GQCS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "system" fn hook_dioc(
    h: *mut c_void,
    code: u32,
    inb: *mut c_void,
    inlen: u32,
    outb: *mut c_void,
    outlen: u32,
    ret: *mut u32,
    ov: *mut Overlapped,
) -> Bool {
    let real: DeviceIoControlFn = std::mem::transmute(REAL_DIOC.load(Ordering::Relaxed));
    let inbuf = if inb.is_null() { &[][..] } else { std::slice::from_raw_parts(inb as *const u8, inlen as usize) };
    let (desc, xfer) = describe(code, inbuf);
    let is_in = matches!(xfer, Some((ep, _, _)) if ep & 0x80 != 0);
    if let Some((_, off, len)) = xfer {
        if !is_in {
            let payload = &inbuf[off.min(inbuf.len())..(off + len).min(inbuf.len())];
            log(&format!("-> {desc}: {}", dump(payload, 512)));
        } else if len <= 1024 {
            log(&format!("-> {desc} (submit)"));
        }
    } else {
        log(&format!("-> {desc} h={h:?}: {}", dump(inbuf, 64)));
    }
    let r = real(h, code, inb, inlen, outb, outlen, ret, ov);
    let completed_now = r != 0;
    if is_in && !outb.is_null() {
        let d = format!("{desc} h={h:?}");
        if completed_now {
            let got = if !ret.is_null() { *ret as usize } else if !ov.is_null() { (*ov).internal_high } else { outlen as usize };
            log_in_data(&d, outb as usize, outlen as usize, got);
        } else if !ov.is_null() {
            PENDING.lock().unwrap().push((ov as usize, outb as usize, outlen as usize, d));
        }
    } else if xfer.is_none() && completed_now && !outb.is_null() && outlen > 0 {
        let n = if ret.is_null() { 0 } else { *ret as usize };
        log(&format!("  <- {desc} returned {n}: {}", dump(std::slice::from_raw_parts(outb as *const u8, n.min(outlen as usize)), 128)));
    }
    sweep();
    r
}

unsafe extern "system" fn hook_createfile(
    name: *const c_char,
    access: u32,
    share: u32,
    sa: *mut c_void,
    disp: u32,
    flags: u32,
    tmpl: *mut c_void,
) -> *mut c_void {
    let real: CreateFileAFn = std::mem::transmute(REAL_CREATEFILE.load(Ordering::Relaxed));
    let h = real(name, access, share, sa, disp, flags, tmpl);
    let n = if name.is_null() { String::new() } else { CStr::from_ptr(name).to_string_lossy().into_owned() };
    log(&format!("CreateFileA(\"{n}\") -> {h:?}"));
    h
}

unsafe extern "system" fn hook_gor(h: *mut c_void, ov: *mut Overlapped, bytes: *mut u32, wait: Bool) -> Bool {
    let real: GetOverlappedResultFn = std::mem::transmute(REAL_GOR.load(Ordering::Relaxed));
    let r = real(h, ov, bytes, wait);
    if r != 0 && !bytes.is_null() {
        complete(ov, *bytes as usize);
    }
    sweep();
    r
}

unsafe extern "system" fn hook_gqcs(port: *mut c_void, entries: *mut OverlappedEntry, count: u32, removed: *mut u32, ms: u32, alertable: Bool) -> Bool {
    let real: GqcsExFn = std::mem::transmute(REAL_GQCS.load(Ordering::Relaxed));
    let r = real(port, entries, count, removed, ms, alertable);
    if r != 0 && !removed.is_null() {
        for e in std::slice::from_raw_parts(entries, *removed as usize) {
            complete(e.overlapped, e.bytes as usize);
        }
    }
    sweep();
    r
}

/// Points the module's import of `func` (from KERNEL32.dll) at `hook`. Returns the original address, if patched.
unsafe fn patch_iat(module: *mut c_void, func: &str, hook: usize) -> Option<usize> {
    let base = module as usize;
    let rd32 = |a: usize| (a as *const u32).read_unaligned();
    let nt = base + rd32(base + 0x3c) as usize;
    // PE32+ optional header starts at nt+24; the import directory is data directory 1 at +112+8.
    let imp_rva = rd32(nt + 24 + 120) as usize;
    if imp_rva == 0 {
        return None;
    }
    let mut desc = base + imp_rva;
    loop {
        let name_rva = rd32(desc + 12) as usize;
        if name_rva == 0 {
            return None;
        }
        let dll = CStr::from_ptr((base + name_rva) as *const c_char).to_string_lossy().to_ascii_lowercase();
        if dll == "kernel32.dll" {
            let oft = rd32(desc) as usize;
            let ft = rd32(desc + 16) as usize;
            let mut i = 0;
            loop {
                let thunk = ((base + if oft != 0 { oft } else { ft }) as *const u64).add(i).read();
                if thunk == 0 {
                    break;
                }
                if thunk & (1 << 63) == 0 {
                    let n = CStr::from_ptr((base + thunk as usize + 2) as *const c_char);
                    if n.to_bytes() == func.as_bytes() {
                        let slot = ((base + ft) as *mut usize).add(i);
                        let mut old = 0;
                        VirtualProtect(slot as *mut c_void, 8, PAGE_READWRITE, &mut old);
                        let orig = slot.read();
                        slot.write(hook);
                        VirtualProtect(slot as *mut c_void, 8, old, &mut old);
                        return Some(orig);
                    }
                }
                i += 1;
            }
        }
        desc += 20;
    }
}

static HOOKED: AtomicBool = AtomicBool::new(false);
static CLOSING: AtomicBool = AtomicBool::new(false);

fn install_hooks() {
    let hooks: [(&str, usize, &AtomicUsize); 4] = [
        ("DeviceIoControl", hook_dioc as *const () as usize, &REAL_DIOC),
        ("CreateFileA", hook_createfile as *const () as usize, &REAL_CREATEFILE),
        ("GetOverlappedResult", hook_gor as *const () as usize, &REAL_GOR),
        ("GetQueuedCompletionStatusEx", hook_gqcs as *const () as usize, &REAL_GQCS),
    ];
    for dll in [c"thorlabs_tsi_camera_sdk.dll", c"thorlabs_tsi_zelux_camera_device.dll", c"thorlabs_tsi_usb_hotplug_monitor.dll"] {
        let m = unsafe { GetModuleHandleA(dll.as_ptr()) };
        if m.is_null() {
            log(&format!("module {} not loaded", dll.to_string_lossy()));
            continue;
        }
        for (f, hook, real) in hooks {
            if let Some(orig) = unsafe { patch_iat(m, f, hook) } {
                // Every module imports the same kernel32 function, so one original address serves all.
                real.store(orig, Ordering::Relaxed);
                log(&format!("hooked {}!{f}", dll.to_string_lossy()));
            }
        }
    }
    HOOKED.store(true, Ordering::Relaxed);
}

// ---- script ----------------------------------------------------------------------------------------------------

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let dir = std::path::absolute(args.next().ok_or("usage: usbtrace <dll folder> <log file>")?)?;
    let log_path = std::path::absolute(args.next().ok_or("usage: usbtrace <dll folder> <log file>")?)?;
    *LOG.lock().unwrap() = Some(std::fs::File::create(&log_path)?);
    *RAW.lock().unwrap() = Some(std::fs::File::create(log_path.with_extension("raw"))?);
    *T0.lock().unwrap() = Some(Instant::now());

    // Nothing is hooked yet during open_sdk; the device DLLs are loaded by it.
    mark("open_sdk");
    let sdk = tlcam::Sdk::open(&dir)?;
    install_hooks();

    mark("discover");
    let serials = sdk.discover()?;
    println!("Cameras found: {serials:?}");
    log(&format!("discover -> {serials:?}"));
    if serials.is_empty() {
        return Ok(());
    }

    mark("open_camera + read_info");
    let mut cam = tlcam::Camera::open_serial(&sdk, &serials[0])?;
    log(&format!("info: {:?}", cam.info()));
    let info = cam.info().clone();
    let gain_mid = (info.gain_range.0 + info.gain_range.1) / 2;
    let bl_mid = (info.black_level_range.0 + info.black_level_range.1) / 2;

    for (name, s) in [
        ("apply exposure 5 ms, gain 0", tlcam::CameraSettings { exposure_ms: 5.0, gain: 0, black_level: None, led_on: false }),
        ("apply exposure 12.345 ms", tlcam::CameraSettings { exposure_ms: 12.345, gain: 0, black_level: None, led_on: false }),
        ("apply gain mid", tlcam::CameraSettings { exposure_ms: 12.345, gain: gain_mid, black_level: None, led_on: false }),
        ("apply black level mid", tlcam::CameraSettings { exposure_ms: 12.345, gain: gain_mid, black_level: Some(bl_mid), led_on: false }),
        ("apply led on", tlcam::CameraSettings { exposure_ms: 12.345, gain: gain_mid, black_level: Some(bl_mid), led_on: true }),
        ("apply defaults", tlcam::CameraSettings { exposure_ms: 5.0, gain: 0, black_level: Some(info.initial.black_level), led_on: false }),
        // Bright frames make the pixel packing unambiguous when compared with the SDK's decoded frames.
        ("apply exposure 200 ms, gain max", tlcam::CameraSettings { exposure_ms: 200.0, gain: info.gain_range.1, black_level: Some(info.initial.black_level), led_on: false }),
    ] {
        mark(name);
        let a = cam.apply(&s)?;
        log(&format!("applied: {a:?}"));
    }

    mark("start (hot pixel, op mode, frames, poll timeout, arm, trigger)");
    cam.start()?;
    mark("frames");
    let t = Instant::now();
    let mut n = 0;
    while n < 5 && t.elapsed() < Duration::from_secs(5) {
        if let Some(f) = cam.poll_frame()? {
            let peak = f.data.iter().max().copied().unwrap_or(0);
            let mean = f.data.iter().map(|&v| v as f64).sum::<f64>() / f.data.len() as f64;
            log(&format!(
                "FRAME count={} {}x{} peak={peak} mean={mean:.2} first16={:?}",
                f.frame_count,
                f.width,
                f.height,
                &f.data[..16]
            ));
            println!("frame {} peak {peak} mean {mean:.1}", f.frame_count);
            let bytes: Vec<u8> = f.data.iter().flat_map(|v| v.to_le_bytes()).collect();
            std::fs::write(log_path.with_extension(format!("frame{}.u16", f.frame_count)), bytes)?;
            n += 1;
        }
    }
    log(&format!(
        "image IN packets so far: {} totalling {} bytes",
        BIG_COUNT.load(Ordering::Relaxed),
        BIG_BYTES.load(Ordering::Relaxed)
    ));
    mark("stop (disarm)");
    cam.stop()?;
    mark("close camera");
    CLOSING.store(true, Ordering::Relaxed);
    drop(cam);
    mark("close sdk");
    drop(sdk);
    log(&format!(
        "image IN packets total: {} totalling {} bytes",
        BIG_COUNT.load(Ordering::Relaxed),
        BIG_BYTES.load(Ordering::Relaxed)
    ));
    println!("Trace written to {} (hooks installed: {})", log_path.display(), HOOKED.load(Ordering::Relaxed));
    Ok(())
}

# ThorCam Beam Profiler (Rust)

A rebuild of the LabVIEW `ThorCam.vi` beam profiler in Rust, following `ThorCam_Blueprint.html` and
`AutoZoom_Design.md` in the parent folder. It controls a Thorlabs scientific camera through the **native C SDK**
(`thorlabs_tsi_camera_sdk.dll`), fits a Gaussian to the X and Y profiles and reports the 1/e² beam diameter.
The image display auto-zooms on the beam.

## Run it

The Rust toolchain is installed locally in `..\.rust\`. Activate it in each new terminal first.

```powershell
. ..\.rust\env.ps1
cargo run --release -p thorcam            # Thorlabs camera
cargo run --release -p thorcam -- --sim   # built-in simulated camera, no hardware needed
cargo run --release -p thorcam -- --sim-color   # simulated colour (Bayer) camera with a red laser
```

When run from this folder, the program finds the DLLs in
`..\Thorlabs_SDK\Scientific Camera Interfaces\SDK\Native Toolkit\dlls\Native_64_lib` by itself.

**Single-file program** for the lab PC: `powershell -ExecutionPolicy Bypass -File .\package.ps1` builds
`dist\ThorCamBeamProfiler.exe` (about 23 MB). Copy that one file anywhere and run it; no installation, no admin
rights. The Thorlabs SDK DLLs are inside it. Each run unpacks them (14 files, 2 MB) to
`%LOCALAPPDATA%\ThorCamBeamProfiler\run-<process id>\` and **deletes them again when the program closes**. If Windows
still holds a DLL at that moment, a hidden copy of the program waits for it to end and deletes the folder then.
Leftovers of a crash are removed at the next start; a second running copy keeps its own folder. So the program leaves
nothing behind: to remove it, delete the exe (and any CSV logs you saved). The colour and polarization
*processing* DLLs of the SDK (172 MB) are left out: this program does its own Bayer handling and the camera SDK
does not load them. The SDK's logger is configured for `C:\SDK_log_files\`, but it has not written there in any
test run.

The PC still needs what every Thorlabs SDK program needs:

* the Thorlabs camera USB driver, installed with ThorCam;
* the Microsoft Visual C++ 2015–2022 runtime (x64). ThorCam installs it too; if it is missing, the program says so
  and gives the download link.

To use other SDK DLLs without rebuilding, put them in a folder `Library_X64` next to the exe; that folder wins.
Search order: the `THORCAM_DLL_DIR` environment variable, `Library_X64` next to the exe, the embedded DLLs, then the
SDK folder in a parent directory.

**Hardware check without the GUI:**

```powershell
cargo run -p tlcam --example probe -- "..\Thorlabs_SDK\Scientific Camera Interfaces\SDK\Native Toolkit\dlls\Native_64_lib"
```

It opens the SDK, lists cameras, prints the camera info and grabs 10 frames.

## Toolbar, export and the right-click menus

* **⏸ Pause / ▶ Play** (Space): freezes the image and the results on the frame on screen. The camera keeps running.
  While paused the held frame is analysed again as controls change (ROI, palette, channel, zoom), and it is what
  snapshots and exports save. The CSV log, recordings and dark-frame recording only take live frames.
* **📷 Snapshot** (Ctrl+S): writes `snapshot_<date>_<time>` into the capture folder (default
  `%USERPROFILE%\Pictures\ThorCam Beam Profiler`, set under *Record & export*): `.png` (full frame as displayed),
  `_raw.tif` (the camera's 16-bit counts, the Bayer mosaic on a colour camera; ImageJ / Fiji opens it calibrated in
  µm and with the 0…full-scale display range) and `.csv` (camera settings, Dx/Dy/w/FWHM, σ, σ₂, centre, fit
  parameters, S/N, R², and both profiles with position in px and µm, data and fit).
* **⏺ Record** (Ctrl+R): `recording_<date>_<time>.avi` (MJPEG of the full frame as displayed; VLC, Windows Media
  Player, ffmpeg; frame rate = the measured rate, pauses excluded; continues in `_002.avi`, … before 2 GB) or `.tif`
  (raw 16-bit BigTIFF stack). Each recording also writes `_frames.csv` with the results of every frame it holds. The
  writer runs on its own thread; if it falls behind, frames are dropped and counted, never queued without bound.
* **ℹ About** (F1): how a pixel becomes µm and how the fit works (model, Levenberg–Marquardt, w = 2σ, D = 4σ, FWHM,
  second moment, detection checks), with the numbers of the beam on screen worked through.
* **Right-click** the full frame or the zoom image: save as PNG (full resolution), save the view with overlays as on
  screen, save the raw frame as TIFF, copy the image, export CSV, pause, snapshot, outline / palette / display mode.
  Right-click a profile plot: export that profile (or both) to CSV, copy it tab-separated for Excel, copy the
  results, save the plot as PNG, reset the view. Right-click the results bar: copy or export the results.

## Camera USB protocol (reverse engineered)

How the SDK talks to a Zelux CS165 over USB, without the DLL in between, is written up in
[`docs/cs165-usb-protocol.md`](docs/cs165-usb-protocol.md): plain-text commands and replies on two bulk endpoints,
and frames as 16 KiB packets of MIPI RAW10 on a third. It was recovered with `tlcam/examples/usbtrace.rs`, which runs
the real SDK and logs every transfer it hands to the Cypress `CYUSB3` driver:

    cargo run --release -p tlcam --example usbtrace -- "<folder with thorlabs_tsi_camera_sdk.dll>" trace.log

It writes `trace.log` (every command, response and image packet header), `trace.raw` (the raw image packets) and
`trace.frameN.u16` (the SDK's decoded frames, to check a decoder against).

## Layout

| Crate | What | Replaces (LabVIEW) |
|---|---|---|
| `tlcam` | Loads the DLL (the way Thorlabs' `load_dll_helpers` do), safe `Sdk` and `Camera` types | `Camera_Open/Start/ApplySettings/GetFrame/Close.vi`, `Set_DLL_Folder.vi` |
| `beam` | Gaussian fit, beam analysis, auto-zoom, synthetic frames; no camera or GUI code | `Profile_Fit`, `Beam_Analyze`, `Beam_FindRegion`, `Zoom_Window`, `Display_Image.vi` |
| `thorcam` | The window (egui) and the three loops; a simulated CS165MU | `Main.vi` |

Three threads, as in the blueprint:

* **UI**: controls send settings only when they change.
* **Acquisition**: the only code that touches the camera. It applies settings and puts each frame in a one-element
  lossy queue, so processing always gets the newest frame.
* **Processing**: `analyze` on every frame, CSV log, recording, exports, and the auto-zoomed image at most 10 times
  a second. Recordings are encoded and written on a fourth thread (`export::Recorder`), file saves on short-lived
  threads, so neither slows the analysis.

## What matches the blueprint

* Dx, Dy = 1/e² diameter = 4σ × pixel size (read from the camera, × binning). σ from the fit and σ₂ (second
  moment) are both shown.
* Fit over an ROI of ±h px around the peak (h = 150 by default, 0 = whole sensor), with an offset term.
* Saturated when peak ≥ 2^bit depth − 1. Saturated pixels show red in the image.
* **One change from the blueprint:** the peak (which centres the ROI and sets "Saturated") is found on the
  despeckled image `min(I[r, c], I[r+1, c+1])`, the same step `AutoZoom_Design.md` uses. With the raw maximum, as
  `Beam_Analyze.vi` is drawn, a single hot pixel moved the ROI off the beam and gave σ ≈ 0.2 px and a false
  "Saturated". The simulator showed this; `hot_pixels_do_not_capture_the_roi` tests it.
* Exposure, gain and black level are clamped to the camera's range, then read back. If the camera rejects a change
  while armed, it is disarmed, written and re-armed.
* Hot-pixel correction is switched on where the camera supports it.
* Optional dark frame (average of 20 frames with the beam blocked). A dark frame that saw light (despeckled
  maximum more than 25 counts above its mean) is refused, because subtracting it would put a negative copy of the
  beam into every profile. The simulator has a "Block beam" switch for recording one.
* CSV log: `t, frame, beam, σx, σy, σ₂x, σ₂y, Dx, Dy, centre x, centre y, peak, saturated` (`beam` = 0/1).
* **Beam detection** (not in the blueprint). A frame is a beam only when
  1. the despeckled peak is at least *Min contrast* (30) counts above the background (darkest 32 × 32 px corner);
     2/3 of that keeps a beam detected, so a beam near the limit does not flicker;
  2. each axis' Gaussian fit has S/N = amplitude / RMS residual ≥ *Min fit S/N* (8; pure noise gives 3 to 4) and
     σ ≥ 0.5 px;
  3. the fitted centre lies inside the ROI and the ROI holds the whole 1/e² diameter (else: "increase ROI half-size").

  Without a beam the panel shows "No beam" and the reason, widths are "—", no fit is drawn, and Saturated stays off.
* **Colour cameras** (e.g. CS165CU, CS895CU; Bayer sensor, read from the SDK with its filter phase and colour
  matrices). The analysis uses one value per 2 × 2 Bayer cell, so positions are in *cells* and the pixel size is
  doubled (6.9 µm on a CS165CU); the 1/e² diameters in µm come out the same as on a mono camera. The raw mosaic is
  never fitted: the R, G and B filters pass a laser line very differently, so it is a checkerboard, and
  demosaicing would add interpolated values. *Colour channel*: Sum (R+G+G+B, default), Red, Green (G+G) or Blue;
  pick the one of your laser colour for the best S/N. A cell is clipped when any raw pixel it uses is at full
  scale; Saturated needs two diagonally touching clipped cells, so a hot pixel does not count. The display shows
  white-balanced, colour-corrected RGB (clipped cells magenta), or with *Colour image* off the analysed channel.
  Polarization cameras are still analysed as raw monochrome data.
* σ₂ (second moment) is computed within ±6σ of the fitted centre, the ISO 11146 window of 3 beam diameters, so
  hot pixels and background far from the beam do not inflate it.
* Display: the full frame (with the zoom window as an orange box) beside the auto-zoomed view. "Fit outline" draws
  the fitted 1/e² ellipse with Dx and Dy as dimension lines and labels.
* **The camera is always released**: `Camera` disarms and closes itself when dropped, on Disconnect, on window
  close, on an error, and even on a panic. `tl_camera_close_sdk` runs at exit.
* Auto-zoom exactly as `AutoZoom_Design.md` (despeckle, darkest-corner background, 13.5 % level, 30/20-count
  hysteresis, margin, ease 0.3 / hold < 0.1 / jump, square pixels). Modes: Auto zoom, Full frame, Lock.

## Tests

```powershell
cargo test --workspace
```

The `beam` tests include the blueprint's `Test_Beam_Analyze` (σx = 40, σy = 25 px recovered within 0.2 px,
Dx ≈ 552 µm, the A = 1100 saturation case) and the `Test_AutoZoom` presets and sequences from
`AutoZoom_Design.md` (small, corner, edge, large, two close / far, three spots, dark, hot pixels, saturated,
elliptical, moving, jump, grow/shrink with exactly 2 toggles, hysteresis band, dim beam). Timing on this PC:
`find_region` 2.5 ms and a full-frame `analyze` 4 ms per 1440 × 1080 frame.

Real hardware, 2026-10-09: the single exe, started from an empty folder, unpacked the SDK, opened a CS165CU colour
camera (serial 28764), recognised it as a Bayer sensor, streamed frames at about 35 fps, read back exposure and
black level, reported "No beam" on a dark sensor, and released the camera on window close. A beam measurement on
real hardware has not been made yet.

## Toolchain note

Rust 1.99, `x86_64-pc-windows-gnu` toolchain (no Visual Studio needed), 64-bit build. For a 32-bit build, add the
`i686-pc-windows-gnu` target and use the `Native_32_lib` DLLs in `Library_X86`.

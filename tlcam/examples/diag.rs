//! Camera diagnostics: grabs a frame at several exposures, prints per-channel statistics and the beam analysis,
//! and saves each raw frame (`frame_<ms>ms.u16`, little-endian u16, row-major) for offline inspection.
//!
//! cargo run --release -p tlcam --example diag -- "<dll folder>" "<output folder>" [exposures in ms...]

use std::time::{Duration, Instant};

use beam::bayer::{channel_image, Channel, Pattern};
use beam::{analyze, BeamParams, ImageRef};

fn stats(v: &[u16]) -> String {
    let mut s = v.to_vec();
    s.sort_unstable();
    let mean = v.iter().map(|&x| x as f64).sum::<f64>() / v.len() as f64;
    let q = |f: f64| s[((s.len() - 1) as f64 * f) as usize];
    format!("min {:>4}  p1 {:>4}  median {:>4}  mean {:>7.1}  p99 {:>4}  p99.9 {:>4}  max {:>4}", s[0], q(0.01), q(0.5), mean, q(0.99), q(0.999), s[s.len() - 1])
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let dir = std::path::absolute(args.get(1).ok_or("usage: diag <dll folder> <out folder> [ms ...]")?)?;
    let out = std::path::absolute(args.get(2).ok_or("missing output folder")?)?;
    let exposures: Vec<f64> = args[3..].iter().filter_map(|a| a.parse().ok()).collect();
    let exposures = if exposures.is_empty() { vec![5.0, 50.0] } else { exposures };
    std::fs::create_dir_all(&out)?;

    let sdk = tlcam::Sdk::open(&dir)?;
    let mut cam = tlcam::Camera::open(&sdk, 0)?;
    let info = cam.info().clone();
    println!("{} {}  {} x {}  {}-bit  color: {:?}", info.model, info.serial, info.width, info.height, info.bit_depth, info.color);
    println!("black level range {:?}, now {}", info.black_level_range, info.initial.black_level);
    cam.start()?;
    for &ms in &exposures {
        let a = cam.apply(&tlcam::CameraSettings { exposure_ms: ms, gain: 0, black_level: None, led_on: false })?;
        // Skip frames exposed before the change.
        let t = Instant::now();
        let mut frame = None;
        let mut n = 0;
        while t.elapsed() < Duration::from_secs(5) && n < 4 {
            if let Some(f) = cam.poll_frame()? {
                frame = Some(f);
                n += 1;
            }
        }
        let Some(f) = frame else {
            println!("{ms} ms: no frame");
            continue;
        };
        println!("\n=== exposure {:.3} ms (applied), frame {} ===", a.exposure_ms, f.frame_count);
        println!("raw          {}", stats(&f.data));
        let path = out.join(format!("frame_{ms}ms.u16"));
        std::fs::write(&path, f.data.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>())?;
        let raw = ImageRef::new(&f.data, f.width, f.height);
        let fs = info.full_scale();
        let pat = match info.color.map(|c| c.phase) {
            Some(tlcam::CfaPhase::Red) => Some(Pattern::RGGB),
            Some(tlcam::CfaPhase::Blue) => Some(Pattern::BGGR),
            Some(tlcam::CfaPhase::GreenLeftOfRed) => Some(Pattern::GRBG),
            Some(tlcam::CfaPhase::GreenLeftOfBlue) => Some(Pattern::GBRG),
            None => None,
        };
        let (pw, _) = info.effective_pixel_um();
        match pat {
            Some(pat) => {
                for ch in Channel::ALL {
                    let c = channel_image(raw, pat, ch, fs);
                    let p = BeamParams { full_scale: c.full_scale, pixel_w_um: 2.0 * pw, pixel_h_um: 2.0 * pw, ..Default::default() };
                    let r = analyze(c.as_ref(), &p, None);
                    println!("{:<14} {}", ch.name(), stats(&c.data));
                    println!(
                        "               background {:.1}  peak {}  contrast {:.1}  clipped cells {}  -> {}",
                        r.background,
                        r.peak,
                        r.contrast,
                        c.clipped.iter().filter(|&&b| b).count(),
                        r.no_beam.as_ref().map_or(format!("beam: Dx {:.1} um, Dy {:.1} um", r.dx_um, r.dy_um), |e| format!("no beam: {e}"))
                    );
                }
            }
            None => {
                let p = BeamParams { full_scale: fs, pixel_w_um: pw, pixel_h_um: pw, ..Default::default() };
                let r = analyze(raw, &p, None);
                println!("analysis: background {:.1} peak {} -> {:?}", r.background, r.peak, r.no_beam);
            }
        }
    }
    Ok(())
}

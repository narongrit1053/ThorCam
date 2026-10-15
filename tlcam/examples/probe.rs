//! Hardware check without the GUI: loads the SDK, lists cameras and grabs a few frames from the first one.
//!
//! cargo run -p tlcam --example probe -- "<folder with thorlabs_tsi_camera_sdk.dll>"

use std::time::{Duration, Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::args().nth(1).ok_or("usage: probe <dll folder>")?;
    let dir = std::path::absolute(dir)?;
    let sdk = tlcam::Sdk::open(&dir)?;
    println!("SDK open ({}).", dir.display());
    let serials = sdk.discover()?;
    println!("Cameras found: {serials:?}");
    if serials.is_empty() {
        return Ok(());
    }
    let mut cam = tlcam::Camera::open(&sdk, 0)?;
    println!("{:#?}", cam.info());
    let applied = cam.apply(&tlcam::CameraSettings { exposure_ms: 5.0, gain: 0, black_level: None, led_on: false })?;
    println!("Applied: {applied:?}");
    cam.start()?;
    let t = Instant::now();
    let mut n = 0;
    while n < 10 && t.elapsed() < Duration::from_secs(5) {
        if let Some(f) = cam.poll_frame()? {
            let peak = f.data.iter().max().copied().unwrap_or(0);
            println!("frame {:>4}: {} x {} px, peak {peak}", f.frame_count, f.width, f.height);
            n += 1;
        }
    }
    println!("{n} frames in {:.2} s. Closing the camera.", t.elapsed().as_secs_f64());
    Ok(())
}

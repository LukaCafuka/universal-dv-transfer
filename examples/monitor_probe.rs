//! Read-only live preview probe: no capture files, play/record commands or UI.
use anyhow::{Context, Result};
use std::time::{Duration, Instant};
use universal_dv_transfer::engine::{Control, Controller, Phase};
fn main() -> Result<()> {
    let _diagnostics =
        universal_dv_transfer::diagnostics::init(std::path::Path::new("test-output/live-monitor"))?;
    let mut args = std::env::args().skip(1);
    let id = args
        .next()
        .context("Pass the exact device ID from the devices command")?;
    let seconds: u64 = args.next().unwrap_or("15".into()).parse()?;
    let cycles: u32 = args.next().unwrap_or("1".into()).parse()?;
    for cycle in 0..cycles {
        println!("Receiver cycle {} of {cycles}", cycle + 1);
        let mut controller = Controller::default();
        controller.monitor(id.clone(), 100, String::new())?;
        let start = Instant::now();
        let mut interval = Instant::now();
        let mut frames = 0;
        while start.elapsed() < Duration::from_secs(seconds) && controller.busy() {
            if controller.preview.lock().unwrap().take().is_some() {
                frames += 1;
            }
            if interval.elapsed() >= Duration::from_secs(1) {
                let s = controller.snapshot();
                println!(
                    "preview={:.1} fps source={} dropped={} {:?} {}",
                    frames as f64 / interval.elapsed().as_secs_f64(),
                    s.received,
                    s.dropped,
                    s.phase,
                    s.message
                );
                frames = 0;
                interval = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        controller.send(Control::Stop);
        controller.wait();
        let s = controller.snapshot();
        anyhow::ensure!(s.phase != Phase::Error, "{}", s.message);
        anyhow::ensure!(s.received > 0, "No DV/HDV samples arrived during the probe");
    }
    Ok(())
}

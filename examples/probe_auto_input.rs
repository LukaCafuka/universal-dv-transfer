//! Brief input-only native probe. No transport or tape-write commands are sent.
use anyhow::{Result, ensure};
use std::{
    collections::BTreeMap,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};
use universal_dv_transfer::backend::{self, Session};

fn main() -> Result<()> {
    let _diagnostics =
        universal_dv_transfer::diagnostics::init(std::path::Path::new("test-output/native-probe"))?;
    let devices = backend::devices()?;
    let requested = std::env::args().nth(1);
    let mode = std::env::args().nth(2).unwrap_or_else(|| "auto".into());
    ensure!(
        requested.is_some() || devices.len() == 1,
        "Connect one deck, or pass the exact ID from the devices command"
    );
    let id = requested.unwrap_or_else(|| devices[0].id.clone());
    println!("Opening input only ({mode}): {id}");
    let session = match mode.as_str() {
        "auto" => Session::open_auto(&id, 100)?,
        "hdv" => Session::open_hdv(&id, false, 0, 100)?,
        "dv" => Session::open(&id, None, 100)?,
        _ => anyhow::bail!("Mode must be auto, dv or hdv"),
    };
    let started = Instant::now();
    let mut samples = BTreeMap::<u8, usize>::new();
    while started.elapsed() < Duration::from_millis(500) {
        session.poll()?;
        for sample in session.chunks.try_iter() {
            if let Some(kind) = sample.first() {
                *samples.entry(*kind).or_default() += 1;
            }
        }
    }
    println!(
        "Auto input opened; sample tags/counts: {samples:?}; receive loss: {}",
        session.dropped.load(Ordering::Relaxed)
    );
    Ok(())
}

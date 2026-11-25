//! Bounded asynchronous diagnostics: native callbacks never wait for disk I/O.
use anyhow::Result;
use crossbeam_channel::{Sender, bounded};
use std::{
    fs::{self, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::OnceLock,
    time::Duration,
};

enum Event {
    Line(String),
    Flush(Sender<()>),
}
struct Logger {
    tx: Sender<Event>,
    path: PathBuf,
}
static LOGGER: OnceLock<Logger> = OnceLock::new();

pub struct Guard;
impl Drop for Guard {
    fn drop(&mut self) {
        flush();
    }
}
pub fn init(capture_folder: &Path) -> Result<Guard> {
    if LOGGER.get().is_some() {
        return Ok(Guard);
    }
    let folder = capture_folder.join("logs");
    fs::create_dir_all(&folder)?;
    let path = folder.join(format!(
        "udvt-{}-{}.log",
        chrono::Utc::now().format("%Y%m%dT%H%M%S%.9fZ"),
        std::process::id()
    ));
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)?;
    let (tx, rx) = bounded(2048);
    std::thread::spawn(move || {
        let mut file = BufWriter::new(file);
        while let Ok(event) = rx.recv() {
            match event {
                Event::Line(line) => {
                    let _ = writeln!(file, "{line}");
                    let _ = file.flush();
                }
                Event::Flush(done) => {
                    let _ = file.flush();
                    let _ = done.send(());
                }
            }
        }
        let _ = file.flush();
    });
    let _ = LOGGER.set(Logger { tx, path });
    log(
        "app",
        format!(
            "Universal DV Transfer {} on {}/{}",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
    );
    Ok(Guard)
}
pub fn path() -> Option<&'static Path> {
    LOGGER.get().map(|l| l.path.as_path())
}
pub fn log(category: &str, message: impl AsRef<str>) {
    if let Some(logger) = LOGGER.get() {
        let line = format!(
            "{} [{category}] {}",
            chrono::Utc::now().to_rfc3339(),
            message.as_ref()
        );
        let _ = logger.tx.try_send(Event::Line(line));
    }
}
pub fn flush() {
    if let Some(logger) = LOGGER.get() {
        let (done, rx) = bounded(1);
        if logger
            .tx
            .send_timeout(Event::Flush(done), Duration::from_millis(250))
            .is_ok()
        {
            let _ = rx.recv_timeout(Duration::from_millis(500));
        }
    }
}

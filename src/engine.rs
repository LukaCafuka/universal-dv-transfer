use crate::{
    backend::{Session, Transport},
    config::{AviType, CaptureOptions, TransferFormat},
    dv::{Frame, Standard},
    media::{self, Input, LivePreview, Writer},
    naming,
};
use anyhow::{Context, Result, bail, ensure};
use crossbeam_channel::{Receiver, Sender};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Preparing,
    Monitoring,
    Finalizing,
    Capturing,
    CapturePaused,
    Recording,
    RecordPaused,
    Finished,
    Error,
}
#[derive(Debug, Clone)]
pub struct Status {
    pub report: Option<PathBuf>,
    pub switches: u64,
    pub issues: u64,
    pub hdv: Option<crate::hdv::VideoInfo>,
    pub bytes: u64,
    pub phase: Phase,
    pub received: u64,
    pub written: u64,
    pub dropped: u64,
    pub buffered: usize,
    pub seconds: f64,
    pub files: Vec<PathBuf>,
    pub message: String,
    pub timecode: String,
    pub recorded: String,
    pub standard: Option<Standard>,
}
impl Default for Status {
    fn default() -> Self {
        Self {
            report: None,
            switches: 0,
            issues: 0,
            hdv: None,
            bytes: 0,
            phase: Phase::Idle,
            received: 0,
            written: 0,
            dropped: 0,
            buffered: 0,
            seconds: 0.,
            files: Vec::new(),
            message: "Ready".into(),
            timecode: String::new(),
            recorded: String::new(),
            standard: None,
        }
    }
}
#[derive(Debug, Clone)]
pub enum Source {
    Device(String),
    File(PathBuf),
}
#[derive(Debug, Clone)]
pub enum Control {
    Stop,
    Pause,
    Resume,
    Split,
    Deck(Transport),
}
pub struct Controller {
    pub status: Arc<Mutex<Status>>,
    pub preview: Arc<Mutex<Option<Vec<u8>>>>,
    tx: Sender<Control>,
    worker: Option<JoinHandle<()>>,
}
impl Default for Controller {
    fn default() -> Self {
        let (tx, _) = crossbeam_channel::unbounded();
        Self {
            status: Arc::new(Mutex::new(Status::default())),
            preview: Arc::new(Mutex::new(None)),
            tx,
            worker: None,
        }
    }
}
impl Controller {
    pub fn monitor(&mut self, id: String, capacity: usize, ffmpeg: String) -> Result<()> {
        self.launch(move |controls, status, image| {
            crate::auto_capture::monitor(id, capacity, &ffmpeg, controls, status, image)
        })
    }
    pub fn snapshot(&self) -> Status {
        self.status.lock().unwrap().clone()
    }
    pub fn busy(&self) -> bool {
        self.worker.as_ref().is_some_and(|h| !h.is_finished())
    }
    pub fn send(&self, c: Control) {
        let _ = self.tx.send(c);
    }
    fn launch(
        &mut self,
        job: impl FnOnce(
            Receiver<Control>,
            Arc<Mutex<Status>>,
            Arc<Mutex<Option<Vec<u8>>>>,
        ) -> Result<()>
        + Send
        + 'static,
    ) -> Result<()> {
        ensure!(!self.busy(), "A transfer is already running");
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
        *self.status.lock().unwrap() = Status {
            phase: Phase::Preparing,
            message: "Opening source…".into(),
            ..Status::default()
        };
        let (tx, rx) = crossbeam_channel::unbounded();
        self.tx = tx;
        let status = self.status.clone();
        let preview = self.preview.clone();
        self.worker = Some(thread::spawn(move || {
            let result = job(rx, status.clone(), preview);
            let mut s = status.lock().unwrap();
            match result {
                Ok(()) => {
                    s.phase = Phase::Finished;
                    if s.message.is_empty() {
                        s.message = "Finished".into();
                    }
                }
                Err(e) => {
                    s.phase = Phase::Error;
                    s.message = format!("{e:#}");
                    crate::diagnostics::log("transfer.error", &s.message);
                }
            }
            crate::diagnostics::log(
                "transfer",
                format!(
                    "phase={:?} received={} written={} dropped={} files={} message={}",
                    s.phase,
                    s.received,
                    s.written,
                    s.dropped,
                    s.files.len(),
                    s.message
                ),
            );
        }));
        Ok(())
    }
    pub fn capture(
        &mut self,
        source: Source,
        mut options: CaptureOptions,
        ffmpeg: String,
        paused: bool,
    ) -> Result<()> {
        crate::diagnostics::log(
            "capture",
            format!("source={source:?} paused={paused} ffmpeg={ffmpeg:?} options={options:?}"),
        );
        if let Source::File(path) = &source {
            options.format = if crate::hdv::is_hdv_file(path) {
                TransferFormat::Hdv
            } else if options.format == TransferFormat::Auto {
                TransferFormat::Dv
            } else {
                options.format
            };
        }
        options.validate()?;
        if options.format == TransferFormat::Dv && options.avi_type == AviType::Type2 {
            media::check_ffmpeg(&ffmpeg)?;
        }
        self.launch(move |rx, status, preview| {
            if options.format == TransferFormat::Auto {
                crate::auto_capture::capture(source, options, &ffmpeg, paused, rx, status, preview)
            } else if options.format == TransferFormat::Hdv {
                crate::hdv_engine::capture(source, options, &ffmpeg, paused, rx, status, preview)
            } else {
                capture(source, options, &ffmpeg, paused, rx, status, preview)
            }
        })
    }
    pub fn record(
        &mut self,
        playlist: Vec<PathBuf>,
        target: Source,
        ffmpeg: String,
        control: bool,
        preview: bool,
    ) -> Result<()> {
        crate::diagnostics::log(
            "record",
            format!(
                "target={target:?} control_deck={control} preview={preview} playlist={playlist:?}"
            ),
        );
        self.launch(move |rx, status, image| {
            if playlist.iter().any(|p| crate::hdv::is_hdv_file(p)) {
                return crate::hdv_engine::record(
                    playlist, target, &ffmpeg, control, preview, rx, status, image,
                );
            }
            record(
                playlist, target, &ffmpeg, control, preview, rx, status, image,
            )
        })
    }
    pub fn wait(&mut self) {
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}
impl Drop for Controller {
    fn drop(&mut self) {
        self.send(Control::Stop);
        self.wait();
    }
}

struct Feed {
    frames: Receiver<Result<Frame>>,
    deck: Sender<Transport>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    drops: Arc<std::sync::atomic::AtomicU64>,
}
impl Feed {
    fn new(source: Source, capacity: usize, ffmpeg: String) -> Self {
        let (tx, frames) = crossbeam_channel::bounded(capacity);
        let (deck, commands) = crossbeam_channel::unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let cancelled = stop.clone();
        let drops = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counts = drops.clone();
        let worker = thread::spawn(move || {
            let result = (|| -> Result<()> {
                match source {
                    Source::File(p) => {
                        let mut input = Input::open(&p, &ffmpeg)?;
                        while !cancelled.load(Ordering::Relaxed) {
                            let Some(f) = input.next_frame()? else {
                                break;
                            };
                            let mut item = Ok(f);
                            loop {
                                match tx.send_timeout(item, Duration::from_millis(25)) {
                                    Ok(()) => break,
                                    Err(crossbeam_channel::SendTimeoutError::Timeout(v)) => {
                                        if cancelled.load(Ordering::Relaxed) {
                                            return Ok(());
                                        }
                                        item = v;
                                    }
                                    Err(_) => return Ok(()),
                                }
                            }
                        }
                    }
                    Source::Device(id) => {
                        let s = Session::open(&id, None, capacity)?;
                        let mut forwarding_drops = 0;
                        while !cancelled.load(Ordering::Relaxed) {
                            for cmd in commands.try_iter() {
                                s.transport(cmd)?;
                            }
                            s.poll()?;
                            while let Ok(f) = s.frames.try_recv() {
                                if tx.try_send(Ok(f)).is_err() {
                                    forwarding_drops += 1;
                                }
                            }
                            counts.store(
                                s.dropped.load(Ordering::Relaxed) + forwarding_drops,
                                Ordering::Relaxed,
                            );
                        }
                        for cmd in commands.try_iter() {
                            s.transport(cmd)?;
                        }
                    }
                }
                Ok(())
            })();
            if let Err(e) = result {
                let mut pending = Err(e);
                while !cancelled.load(Ordering::Relaxed) {
                    match tx.send_timeout(pending, Duration::from_millis(25)) {
                        Ok(()) => break,
                        Err(crossbeam_channel::SendTimeoutError::Timeout(item)) => pending = item,
                        Err(_) => break,
                    }
                }
            }
        });
        Self {
            frames,
            deck,
            stop,
            thread: Some(worker),
            drops,
        }
    }
}
impl Drop for Feed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}
fn update_frame(status: &mut Status, f: &Frame) {
    status.standard = Some(f.standard);
    status.timecode = f.timecode.clone().unwrap_or_default();
    status.recorded = f.recorded.map(|d| d.to_string()).unwrap_or_default();
}
fn image(preview: &mut Option<LivePreview>, f: &Frame, out: &Arc<Mutex<Option<Vec<u8>>>>) {
    if let Some(p) = preview {
        p.output_to(out);
        p.offer(1, &f.data);
    }
}

fn capture(
    source: Source,
    o: CaptureOptions,
    ffmpeg: &str,
    mut paused: bool,
    rx: Receiver<Control>,
    status: Arc<Mutex<Status>>,
    out: Arc<Mutex<Option<Vec<u8>>>>,
) -> Result<()> {
    let feed = Feed::new(source, o.buffer_frames, ffmpeg.into());
    let mut preview = if o.preview {
        Some(LivePreview::new(ffmpeg))
    } else {
        None
    };
    if o.control_deck && !paused {
        let _ = feed.deck.send(Transport::Play);
    }
    let mut writer: Option<Writer> = None;
    let mut standard = None;
    let mut field_order = None;
    let mut segment_frames = 0u64;
    let mut segment_seen = 0u64;
    let mut last_date: Option<chrono::NaiveDateTime> = None;
    let mut split = false;
    let mut last_signal = Instant::now();
    let mut elapsed = 0.;
    let mut signal_received = false;
    {
        let mut s = status.lock().unwrap();
        s.phase = if paused {
            Phase::CapturePaused
        } else {
            Phase::Capturing
        };
        s.message = if preview.is_none() && o.preview {
            "Preview unavailable; check FFmpeg".into()
        } else {
            "Waiting for DV signal".into()
        };
    }
    let result = (|| -> Result<()> {
        loop {
            for c in rx.try_iter() {
                match c {
                    Control::Stop => {
                        status.lock().unwrap().message = "Stopped".into();
                        return Ok(());
                    }
                    Control::Pause => {
                        paused = true;
                        if let Some(w) = writer.take() {
                            w.finish()?;
                        }
                        if o.control_deck {
                            let _ = feed.deck.send(Transport::Pause);
                        }
                        status.lock().unwrap().phase = Phase::CapturePaused;
                    }
                    Control::Resume => {
                        paused = false;
                        if o.control_deck {
                            let _ = feed.deck.send(Transport::Play);
                        }
                        status.lock().unwrap().phase = Phase::Capturing;
                    }
                    Control::Split => split = true,
                    Control::Deck(t) => {
                        let _ = feed.deck.send(t);
                    }
                }
            }
            let f = match feed.frames.recv_timeout(Duration::from_millis(25)) {
                Ok(f) => f?,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    status.lock().unwrap().message = "End of source".into();
                    break;
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    if !paused && last_signal.elapsed() > Duration::from_secs(15) {
                        bail!(
                            "No DV signal for 15 seconds. Capture was finalized; check the tape and FireWire connection."
                        );
                    }
                    continue;
                }
            };
            last_signal = Instant::now();
            signal_received = true;
            image(&mut preview, &f, &out);
            {
                let mut s = status.lock().unwrap();
                update_frame(&mut s, &f);
                s.buffered = feed.frames.len();
                s.dropped = feed.drops.load(Ordering::Relaxed);
            }
            if paused {
                continue;
            }
            let gap = match (last_date, f.recorded) {
                (Some(a), Some(b)) => {
                    o.discontinuity_seconds > 0
                        && (b - a).num_seconds().unsigned_abs() > o.discontinuity_seconds as u64
                }
                _ => false,
            };
            if f.recorded.is_some() {
                last_date = f.recorded;
            }
            let limit = o.max_frames > 0 && segment_frames >= o.max_frames;
            // A Type 2 AVI header describes the whole file. Start another file
            // when known scan metadata changes; an unreadable pack alone does
            // not split a capture. Type 1 keeps the flags in each DV frame.
            let scan_changed = o.avi_type == crate::config::AviType::Type2
                && f.field_order.is_some()
                && f.field_order != field_order;
            if split || gap || limit || standard.is_some_and(|s| s != f.standard) || scan_changed {
                if let Some(w) = writer.take() {
                    w.finish()?;
                }
                split = false;
            }
            if writer.is_none() {
                let (path, file) = naming::reserve(&o, f.recorded)?;
                writer = Some(Writer::new(file, &path, o.avi_type, &f, ffmpeg)?);
                status.lock().unwrap().files.push(path);
                standard = Some(f.standard);
                field_order = f.field_order;
                segment_frames = 0;
                segment_seen = 0;
            }
            if segment_seen.is_multiple_of(o.every_nth) {
                writer.as_mut().unwrap().write(&f)?;
                segment_frames += 1;
                status.lock().unwrap().written += 1;
            }
            segment_seen += 1;
            elapsed += f.standard.seconds(1);
            {
                let mut s = status.lock().unwrap();
                s.received += 1;
                s.seconds = elapsed;
                s.message = "Capturing".into();
            }
            if o.duration_seconds > 0 && elapsed + 0.000001 >= o.duration_seconds as f64 {
                status.lock().unwrap().message = "Capture duration reached".into();
                break;
            }
        }
        ensure!(signal_received, "Source contained no complete DV frames");
        Ok(())
    })();
    if o.control_deck {
        let _ = feed.deck.send(Transport::Pause);
    }
    // Finalize even on lost signal or a source error, so complete captured frames remain usable.
    let finish = writer.take().map(Writer::finish).transpose();
    drop(feed);
    if let Err(finalize_error) = finish {
        return Err(finalize_error).context(match result {
            Ok(()) => "Could not finalize the capture".to_owned(),
            Err(source_error) => format!("{source_error:#}; capture finalization also failed"),
        });
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn record(
    playlist: Vec<PathBuf>,
    target: Source,
    ffmpeg: &str,
    control: bool,
    show_preview: bool,
    rx: Receiver<Control>,
    status: Arc<Mutex<Status>>,
    out: Arc<Mutex<Option<Vec<u8>>>>,
) -> Result<()> {
    ensure!(!playlist.is_empty(), "Playlist is empty");
    // Preflight every input before any command can put a deck into record mode.
    let mut standard = None;
    for path in &playlist {
        let mut input = Input::open(path, ffmpeg)?;
        let mut count = 0;
        while let Some(f) = input.next_frame()? {
            if rx.try_iter().any(|c| matches!(c, Control::Stop)) {
                status.lock().unwrap().message = "Cancelled before tape recording".into();
                return Ok(());
            }
            if let Some(s) = standard {
                ensure!(
                    s == f.standard,
                    "Playlist mixes PAL and NTSC: {}",
                    path.display()
                );
            } else {
                standard = Some(f.standard);
            }
            count += 1;
        }
        ensure!(count > 0, "No DV frames in {}", path.display());
    }
    let standard = standard.context("No DV frames")?;
    let mut file: Option<File> = None;
    let mut session = None;
    match target {
        Source::Device(id) => {
            session = Some(Session::open(&id, Some(standard), 100)?);
        }
        Source::File(p) => {
            file = Some(
                OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&p)
                    .with_context(|| format!("Will not overwrite {}", p.display()))?,
            );
            status.lock().unwrap().files.push(p);
        }
    }
    let mut preview = if show_preview {
        Some(LivePreview::new(ffmpeg))
    } else {
        None
    };
    let mut paused = false;
    let mut last: Option<Frame> = None;
    if control && let Some(s) = &session {
        s.transport(Transport::Record)?;
    }
    status.lock().unwrap().phase = Phase::Recording;
    let result = (|| -> Result<()> {
        for path in playlist {
            let mut input = Input::open(&path, ffmpeg)?;
            while let Some(f) = input.next_frame()? {
                loop {
                    let mut stopping = false;
                    for cmd in rx.try_iter() {
                        match cmd {
                            Control::Stop => stopping = true,
                            Control::Pause => {
                                paused = true;
                                if control && let Some(s) = &session {
                                    s.transport(Transport::RecordPause)?;
                                }
                                status.lock().unwrap().phase = Phase::RecordPaused;
                            }
                            Control::Resume => {
                                paused = false;
                                if control && let Some(s) = &session {
                                    s.transport(Transport::Record)?;
                                }
                                status.lock().unwrap().phase = Phase::Recording;
                            }
                            Control::Deck(t) => {
                                if let Some(s) = &session {
                                    s.transport(t)?;
                                }
                            }
                            Control::Split => {}
                        }
                    }
                    if stopping {
                        status.lock().unwrap().message = "Stopped".into();
                        return Ok(());
                    }
                    if !paused {
                        break;
                    }
                    if let (Some(s), Some(f)) = (&session, &last) {
                        s.write(f)?;
                        s.poll()?;
                    } else {
                        thread::sleep(Duration::from_millis(20));
                    }
                }
                ensure!(f.standard == standard, "Source changed after preflight");
                if let Some(s) = &session {
                    s.write(&f)?;
                    s.poll()?;
                }
                if let Some(w) = &mut file {
                    w.write_all(&f.data)?;
                }
                image(&mut preview, &f, &out);
                {
                    let mut s = status.lock().unwrap();
                    update_frame(&mut s, &f);
                    s.received += 1;
                    s.written += 1;
                    s.seconds = standard.seconds(s.written);
                    s.message = format!("Recording {}", path.display());
                }
                last = Some(f);
            }
        }
        status.lock().unwrap().message = "Playlist complete".into();
        Ok(())
    })();
    let drain = if result.is_ok() {
        session.as_ref().map(Session::finish).transpose()
    } else {
        Ok(None)
    };
    // Leave the tape stopped/record-paused rather than recording a repeated final frame indefinitely.
    if control && let Some(s) = &session {
        let _ = s.transport(Transport::RecordPause);
    }
    if let Some(w) = file {
        w.sync_all()?;
    }
    result?;
    drain?;
    Ok(())
}

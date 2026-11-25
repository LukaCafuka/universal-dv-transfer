use crate::{
    backend::{Session, Transport},
    config::CaptureOptions,
    engine::{Control, Phase, Source, Status},
    hdv::{self, Inspector, PACKET, Packetizer, Reader},
    media::LivePreview,
};
use anyhow::{Context, Result, bail, ensure};
use crossbeam_channel::{Receiver, Sender};
use std::{
    fs::{File, OpenOptions},
    io::{BufWriter, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

struct Feed {
    chunks: Receiver<Result<Vec<u8>>>,
    commands: Sender<Transport>,
    cancelled: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    dropped: Arc<AtomicU64>,
}
impl Feed {
    fn new(source: Source, capacity: usize) -> Self {
        let (tx, chunks) = crossbeam_channel::bounded(capacity);
        let (commands, rx) = crossbeam_channel::unbounded();
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = cancelled.clone();
        let dropped = Arc::new(AtomicU64::new(0));
        let lost = dropped.clone();
        let worker = thread::spawn(move || {
            let result = (|| -> Result<()> {
                match source {
                    Source::File(path) => {
                        let mut input = Reader::open(&path)?;
                        while !stop.load(Ordering::Relaxed) {
                            let Some(packets) = input.next_packets()? else {
                                break;
                            };
                            let mut item = Ok(packets.into_iter().flatten().collect());
                            loop {
                                match tx.send_timeout(item, Duration::from_millis(25)) {
                                    Ok(()) => break,
                                    Err(crossbeam_channel::SendTimeoutError::Timeout(v)) => {
                                        if stop.load(Ordering::Relaxed) {
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
                        let session = Session::open_hdv(&id, false, 0, capacity)?;
                        let mut forwarding_drops = 0;
                        while !stop.load(Ordering::Relaxed) {
                            for command in rx.try_iter() {
                                session.transport(command)?;
                            }
                            session.poll()?;
                            for bytes in session.chunks.try_iter() {
                                if tx.try_send(Ok(bytes)).is_err() {
                                    forwarding_drops += 1;
                                }
                            }
                            lost.store(
                                session.dropped.load(Ordering::Relaxed) + forwarding_drops,
                                Ordering::Relaxed,
                            );
                        }
                        for command in rx.try_iter() {
                            session.transport(command)?;
                        }
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                let mut item = Err(error);
                while !stop.load(Ordering::Relaxed) {
                    match tx.send_timeout(item, Duration::from_millis(25)) {
                        Ok(()) => break,
                        Err(crossbeam_channel::SendTimeoutError::Timeout(v)) => item = v,
                        Err(_) => break,
                    }
                }
            }
        });
        Self {
            chunks,
            commands,
            cancelled,
            worker: Some(worker),
            dropped,
        }
    }
}
impl Drop for Feed {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn preview(preview: &mut Option<LivePreview>, bytes: &[u8], image: &Arc<Mutex<Option<Vec<u8>>>>) {
    if let Some(p) = preview {
        p.output_to(image);
        p.offer(2, bytes);
    }
}
#[derive(Default)]
struct CaptureFiles {
    count: usize,
    report: Option<PathBuf>,
}
impl CaptureFiles {
    fn update(&mut self, paths: &[PathBuf], report: &Option<PathBuf>, status: &Arc<Mutex<Status>>) {
        if self.count == paths.len() && self.report.as_ref() == report.as_ref() {
            return;
        }
        let mut s = status.lock().unwrap();
        s.files.extend_from_slice(&paths[self.count..]);
        if report.is_some() {
            s.report = report.clone();
        }
        self.count = paths.len();
        self.report = report.clone();
    }
}
fn sync(writer: &mut Option<BufWriter<File>>) -> Result<()> {
    if let Some(mut file) = writer.take() {
        file.flush()?;
        file.get_ref().sync_all()?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn capture(
    source: Source,
    options: CaptureOptions,
    ffmpeg: &str,
    mut paused: bool,
    controls: Receiver<Control>,
    status: Arc<Mutex<Status>>,
    image: Arc<Mutex<Option<Vec<u8>>>>,
) -> Result<()> {
    let feed = Feed::new(source, options.buffer_frames);
    let mut display = if options.preview {
        Some(LivePreview::new(ffmpeg))
    } else {
        None
    };
    let mut packets = Packetizer::default();
    let mut inspector = Inspector::default();
    let mut writer = crate::hdv_capture::Writer::new(options.clone());
    let mut files = CaptureFiles::default();
    let mut max_buffered = 0;
    let mut next_stats = Instant::now() + Duration::from_secs(5);
    let mut resume_at_gop = false;
    let mut active_seconds = 0.;
    let mut last_signal = Instant::now();
    if options.control_deck && !paused {
        let _ = feed.commands.send(Transport::Play);
    }
    status.lock().unwrap().phase = if paused {
        Phase::CapturePaused
    } else {
        Phase::Capturing
    };
    let result = (|| -> Result<()> {
        loop {
            for control in controls.try_iter() {
                match control {
                    Control::Stop => {
                        status.lock().unwrap().message = "Stopped".into();
                        return Ok(());
                    }
                    Control::Pause => {
                        paused = true;
                        writer.finish()?;
                        files.update(&writer.files, &writer.report, &status);
                        writer = crate::hdv_capture::Writer::new(options.clone());
                        files = CaptureFiles::default();
                        if options.control_deck {
                            let _ = feed.commands.send(Transport::Pause);
                        }
                        status.lock().unwrap().phase = Phase::CapturePaused;
                    }
                    Control::Resume => {
                        paused = false;
                        resume_at_gop = true;
                        if options.control_deck {
                            let _ = feed.commands.send(Transport::Play);
                        }
                        status.lock().unwrap().phase = Phase::Capturing;
                    }
                    Control::Split => writer.request_split(),
                    Control::Deck(command) => {
                        let _ = feed.commands.send(command);
                    }
                }
            }
            let (bytes, eof) = match feed.chunks.recv_timeout(Duration::from_millis(25)) {
                Ok(bytes) => (bytes?, false),
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => (Vec::new(), true),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    if !paused && last_signal.elapsed() > Duration::from_secs(15) {
                        bail!(
                            "No HDV signal for 15 seconds; check HDV output mode and the FireWire connection"
                        );
                    }
                    continue;
                }
            };
            if !bytes.is_empty() {
                last_signal = Instant::now();
            }
            let batch = if eof {
                packets.finish()?
            } else {
                packets.push(&bytes)
            };
            max_buffered = max_buffered.max(feed.chunks.len() + usize::from(!batch.is_empty()));
            let mut display_bytes = Vec::with_capacity(batch.len() * PACKET);
            let mut written = 0;
            let mut written_bytes = 0;
            let mut duration_reached = false;
            for packet in batch {
                let before = inspector.seconds;
                inspector.push(&packet);
                display_bytes.extend_from_slice(&packet);
                if paused {
                    continue;
                }
                let access = Some(hdv::pid(&packet)) == inspector.video_pid
                    && hdv::random_access(&packet)
                    && !inspector.tables.is_empty();
                if resume_at_gop && !access {
                    continue;
                }
                resume_at_gop = false;
                let written_before = writer.inspector.pictures;
                writer.push(&packet)?;
                written += writer.inspector.pictures - written_before;
                written_bytes += PACKET as u64;
                active_seconds += inspector.seconds - before;
                if options.duration_seconds > 0 && active_seconds >= options.duration_seconds as f64
                {
                    duration_reached = true;
                    break;
                }
            }
            // Publish once per received batch. Per-packet file-list scans and
            // metadata formatting can otherwise become slower than the deck as
            // scene files accumulate, filling the receive queues.
            {
                let mut s = status.lock().unwrap();
                s.hdv = inspector.video.clone();
                s.received = inspector.pictures;
                s.dropped =
                    inspector.errors + packets.discarded + feed.dropped.load(Ordering::Relaxed);
                s.buffered = feed.chunks.len();
                if !paused {
                    s.written += written;
                    s.bytes += written_bytes;
                    s.seconds = active_seconds;
                    if let Some(metadata) = &writer.inspector.metadata {
                        s.timecode = metadata.timecode.clone().unwrap_or_default();
                        s.recorded = metadata.recorded.map(|d| d.to_string()).unwrap_or_default();
                    }
                    s.message = if writer.waiting() {
                        "Waiting for an HDV GOP with safe preroll to split".into()
                    } else {
                        "Capturing HDV transport stream".into()
                    };
                }
            }
            files.update(&writer.files, &writer.report, &status);
            preview(&mut display, &display_bytes, &image);
            if Instant::now() >= next_stats {
                crate::diagnostics::log(
                    "hdv.capture",
                    format!(
                        "packets={} pictures={} transport_errors={} unsynchronized_bytes={} receive_or_queue_drops={} queued_chunks={} peak_queued_chunks={}",
                        inspector.packets,
                        inspector.pictures,
                        inspector.errors,
                        packets.discarded,
                        feed.dropped.load(Ordering::Relaxed),
                        feed.chunks.len(),
                        max_buffered,
                    ),
                );
                next_stats = Instant::now() + Duration::from_secs(5);
            }
            if duration_reached {
                status.lock().unwrap().message = "Capture duration reached".into();
                return Ok(());
            }
            if eof {
                inspector.validate()?;
                status.lock().unwrap().message = "End of HDV source".into();
                break;
            }
        }
        Ok(())
    })();
    if options.control_deck {
        let _ = feed.commands.send(Transport::Pause);
    }
    let finish = writer.finish();
    files.update(&writer.files, &writer.report, &status);
    crate::diagnostics::log(
        "hdv.capture",
        format!(
            "final packets={} pictures={} transport_errors={} unsynchronized_bytes={} receive_or_queue_drops={} peak_queued_chunks={}",
            inspector.packets,
            inspector.pictures,
            inspector.errors,
            packets.discarded,
            feed.dropped.load(Ordering::Relaxed),
            max_buffered,
        ),
    );
    drop(feed);
    result?;
    finish
}

#[allow(clippy::too_many_arguments)]
pub fn record(
    playlist: Vec<PathBuf>,
    target: Source,
    ffmpeg: &str,
    control: bool,
    show_preview: bool,
    controls: Receiver<Control>,
    status: Arc<Mutex<Status>>,
    image: Arc<Mutex<Option<Vec<u8>>>>,
) -> Result<()> {
    ensure!(!playlist.is_empty(), "Playlist is empty");
    let mut format = None;
    let mut bitrate = 0;
    let mut boundaries = Vec::new();
    // Read every packet before opening a deck or creating the destination.
    for path in &playlist {
        ensure!(
            hdv::is_hdv_file(path),
            "Playlist mixes DV and HDV or contains a non-TS file: {}",
            path.display()
        );
        let mut input = Reader::open(path)?;
        let mut inspector = Inspector::default();
        while let Some(packets) = input.next_packets()? {
            if controls.try_iter().any(|c| matches!(c, Control::Stop)) {
                status.lock().unwrap().message = "Cancelled before tape recording".into();
                return Ok(());
            }
            for packet in packets {
                inspector.push(&packet);
            }
        }
        let video = inspector.validate()?.clone();
        if matches!(&target, Source::Device(_)) {
            inspector.validate_tape_source().with_context(|| {
                format!("{} is not prepared for HDV tape recording", path.display())
            })?;
        }
        ensure!(
            inspector.errors == 0,
            "HDV transport errors in {}; repair the source before recording",
            path.display()
        );
        ensure!(
            inspector.bitrate > 0 && inspector.seconds > 0.,
            "Missing PCR timing in {}",
            path.display()
        );
        ensure!(
            inspector.format_changes == 0,
            "HDV format changes within {}; split matching formats before tape output",
            path.display()
        );
        ensure!(
            inspector.bitrate <= 45_000_000,
            "Transport bitrate exceeds HDV FireWire capacity"
        );
        boundaries.push(inspector.discontinuity_packets());
        if let Some(expected) = &format {
            ensure!(
                *expected == video,
                "HDV playlist changes resolution, scan mode or frame rate; record matching clips together"
            );
        } else {
            format = Some(video);
            bitrate = inspector.bitrate;
        }
    }
    let mut file = None;
    let mut device = None;
    let device_id = if let Source::Device(id) = &target {
        Some(id.clone())
    } else {
        None
    };
    match target {
        Source::File(path) => {
            file = Some(BufWriter::new(
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .with_context(|| format!("Will not overwrite {}", path.display()))?,
            ));
            status.lock().unwrap().files.push(path);
        }
        Source::Device(id) => device = Some(Session::open_hdv(&id, true, bitrate, 100)?),
    }
    let mut display = if show_preview {
        Some(LivePreview::new(ffmpeg))
    } else {
        None
    };
    let mut paused = false;
    if control && let Some(s) = &device {
        s.transport(Transport::Record)?;
    }
    status.lock().unwrap().phase = Phase::Recording;
    let result = (|| -> Result<()> {
        let mut elapsed = 0.;
        let mut pictures = 0;
        for (index, path) in playlist.into_iter().enumerate() {
            if index > 0
                && let Some(s) = &device
            {
                let boundary: Vec<u8> = boundaries[index].iter().flatten().copied().collect();
                s.write_bytes(&boundary)?;
            }
            let mut input = Reader::open(&path)?;
            let mut inspector = Inspector::default();
            while let Some(packets) = input.next_packets()? {
                loop {
                    for command in controls.try_iter() {
                        match command {
                            Control::Stop => {
                                status.lock().unwrap().message = "Stopped".into();
                                return Ok(());
                            }
                            Control::Pause => {
                                paused = true;
                                if let Some(s) = device.take() {
                                    s.finish()?;
                                    if control {
                                        s.transport(Transport::RecordPause)?;
                                    }
                                }
                                status.lock().unwrap().phase = Phase::RecordPaused;
                            }
                            Control::Resume => {
                                paused = false;
                                if let Some(id) = &device_id {
                                    device = Some(Session::open_hdv(id, true, bitrate, 100)?);
                                }
                                if control && let Some(s) = &device {
                                    s.transport(Transport::Record)?;
                                }
                                status.lock().unwrap().phase = Phase::Recording;
                            }
                            Control::Deck(command) => {
                                if let Some(s) = &device {
                                    s.transport(command)?;
                                }
                            }
                            Control::Split => {}
                        }
                    }
                    if !paused {
                        break;
                    }
                    if let Some(s) = &device {
                        s.poll()?;
                    } else {
                        thread::sleep(Duration::from_millis(20));
                    }
                }
                for packet in &packets {
                    inspector.push(packet);
                }
                if let Some(video) = &inspector.video {
                    ensure!(
                        Some(video) == format.as_ref(),
                        "HDV source changed after preflight"
                    );
                }
                let bytes: Vec<u8> = packets.into_iter().flatten().collect();
                if let Some(s) = &device {
                    s.write_bytes(&bytes)?;
                    s.poll()?;
                }
                if let Some(w) = &mut file {
                    w.write_all(&bytes)?;
                }
                preview(&mut display, &bytes, &image);
                let mut s = status.lock().unwrap();
                s.hdv = inspector.video.clone();
                s.received = pictures + inspector.pictures;
                s.written = s.received;
                s.bytes += bytes.len() as u64;
                s.seconds = elapsed + inspector.seconds;
                s.message = format!("Recording HDV {}", path.display());
            }
            elapsed += inspector.seconds;
            pictures += inspector.pictures;
        }
        status.lock().unwrap().message = "HDV playlist complete".into();
        Ok(())
    })();
    let drain = if result.is_ok() {
        device.as_ref().map(Session::finish).transpose()
    } else {
        Ok(None)
    };
    if control && let Some(s) = &device {
        let _ = s.transport(Transport::RecordPause);
    }
    let finish = sync(&mut file);
    result?;
    drain?;
    finish
}

#[cfg(test)]
mod capture_status_tests {
    use super::*;

    #[test]
    fn files_are_added_once_and_survive_a_new_writer_after_pause() {
        let status = Arc::new(Mutex::new(Status::default()));
        let mut published = CaptureFiles::default();
        let mut paths: Vec<PathBuf> = (0..30)
            .map(|i| PathBuf::from(format!("clip-{i}.m2t")))
            .collect();
        let report = Some(PathBuf::from("first.hdv.jsonl"));
        published.update(&paths[..1], &None, &status);
        published.update(&paths, &report, &status);
        for _ in 0..1000 {
            published.update(&paths, &report, &status);
        }
        paths.push(PathBuf::from("clip-30.m2t"));
        published.update(&paths, &report, &status);
        assert_eq!(status.lock().unwrap().files, paths);
        assert_eq!(status.lock().unwrap().report, report);

        let mut next_writer = CaptureFiles::default();
        let next = vec![PathBuf::from("after-pause.m2t")];
        next_writer.update(&next, &None, &status);
        paths.extend(next);
        assert_eq!(status.lock().unwrap().files, paths);
        // A new writer does not hide the previous report while opening its own.
        assert_eq!(status.lock().unwrap().report, report);
        let next_report = Some(PathBuf::from("next.hdv.jsonl"));
        next_writer.update(&[PathBuf::from("after-pause.m2t")], &next_report, &status);
        assert_eq!(status.lock().unwrap().report, next_report);
    }

    #[test]
    fn unchanged_file_list_does_not_wait_for_the_ui_status_lock() {
        let status = Arc::new(Mutex::new(Status::default()));
        let mut published = CaptureFiles::default();
        let paths = vec![PathBuf::from("clip.m2t")];
        let report = Some(PathBuf::from("clip.hdv.jsonl"));
        published.update(&paths, &report, &status);
        let lock = status.lock().unwrap();
        let (started_tx, started_rx) = crossbeam_channel::bounded(1);
        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        let shared = status.clone();
        let worker = thread::spawn(move || {
            started_tx.send(()).unwrap();
            published.update(&paths, &report, &shared);
            done_tx.send(()).unwrap();
        });
        started_rx.recv().unwrap();
        let completed = done_rx.recv_timeout(Duration::from_secs(1)).is_ok();
        drop(lock);
        worker.join().unwrap();
        assert!(
            completed,
            "Unchanged file lists must not block capture on UI access"
        );
    }
}

//! Mixed-tape capture. Save received bytes before doing any AVI conversion.
//! A receiver can follow a format change; it cannot recover data a deck did not send.
use crate::{
    backend::{Session, Transport},
    config::CaptureOptions,
    dv::Frame,
    engine::{Control, Phase, Source, Status},
    hdv::{self, Packetizer},
    media::{Input, LivePreview, Writer},
    naming,
};
use anyhow::{Context, Result, bail, ensure};
use crossbeam_channel::Receiver;
use serde_json::json;
use std::{
    fs::File,
    io::{BufWriter, Write},
    path::PathBuf,
    sync::{Arc, Mutex, atomic::Ordering},
    thread,
    time::{Duration, Instant},
};

enum Item {
    Dv(Frame),
    Hdv(Vec<u8>),
    Partial(Vec<u8>, &'static str, String),
}

/// The native Auto protocol is explicitly tagged; compressed pictures are never
/// guessed from an arbitrary byte pattern. CIP FMT distinguishes DV from MPEG TS.
#[derive(Default)]
struct Decoder {
    format: u8,
    dv: Vec<u8>,
    ts: Packetizer,
    ts_source: Vec<u8>,
    dbc: Option<u8>,
}
impl Decoder {
    fn flush(&mut self, out: &mut Vec<Item>) {
        if !self.dv.is_empty() {
            out.push(Item::Partial(
                std::mem::take(&mut self.dv),
                "partial.dv",
                "Incomplete DV frame at a boundary; received DIF bytes retained".into(),
            ));
        }
        if !self.ts_source.is_empty() {
            match self.ts.finish() {
                Ok(packets) if self.ts.discarded == 0 => {
                    if !packets.is_empty() {
                        out.push(Item::Hdv(packets.into_iter().flatten().collect()));
                    }
                }
                _ => out.push(Item::Partial(
                    std::mem::take(&mut self.ts_source),
                    "partial.ts",
                    "Incomplete/unsynchronized HDV sample; original sample bytes retained".into(),
                )),
            }
        }
        self.ts = Packetizer::default();
        self.ts_source.clear();
        self.dbc = None;
    }
    fn change(&mut self, format: u8, out: &mut Vec<Item>) {
        if self.format != format {
            self.flush(out);
            self.format = format;
        }
    }
    fn push(&mut self, tagged: &[u8]) -> Vec<Item> {
        let mut out = Vec::new();
        let Some((&kind, bytes)) = tagged.split_first() else {
            return out;
        };
        match kind {
            1 => {
                self.change(1, &mut out);
                match Frame::parse(bytes.to_vec()) {
                    Ok(frame) => out.push(Item::Dv(frame)),
                    Err(e) => out.push(Item::Partial(bytes.to_vec(), "partial.dv", e.to_string())),
                }
            }
            2 => {
                self.change(2, &mut out);
                self.ts_source.extend_from_slice(bytes);
                let packets = self.ts.push(bytes);
                if self.ts.discarded != 0 {
                    out.push(Item::Partial(
                        std::mem::take(&mut self.ts_source),
                        "partial.ts",
                        "HDV synchronization error; original sample bytes retained".into(),
                    ));
                    self.ts = Packetizer::default();
                } else if !packets.is_empty() {
                    out.push(Item::Hdv(packets.into_iter().flatten().collect()));
                    // Keep a bounded copy of the samples spanning the tail. On an
                    // invalid tail the recovery file may overlap the saved .m2t.
                    self.ts_source = self.ts.pending_bytes().to_vec();
                }
            }
            3 if bytes.len() >= 8 => {
                let format = bytes[4] & 0x3f;
                let payload = &bytes[8..];
                if payload.is_empty() {
                    return out;
                } // CIP-only cycles carry no picture.
                match format {
                    0x00 if bytes[1] == 120 && payload.len().is_multiple_of(480) => {
                        self.change(1, &mut out);
                        // DBC counts DV data blocks, not DIF blocks. A gap makes
                        // the current frame incomplete; never fill holes with zeros.
                        if self.dbc.is_some_and(|expected| expected != bytes[3]) {
                            self.flush(&mut out);
                            out.push(Item::Partial(
                                Vec::new(),
                                "bin",
                                "DV FireWire continuity gap; missing data cannot be reconstructed"
                                    .into(),
                            ));
                        }
                        self.dbc = Some(bytes[3].wrapping_add((payload.len() / 480) as u8));
                        for block in payload.chunks_exact(480) {
                            let first =
                                block[0] & 0xe0 == 0 && block[1] & 0xf0 == 0 && block[2] == 0;
                            if first && !self.dv.is_empty() {
                                out.push(Item::Partial(
                                    std::mem::take(&mut self.dv),
                                    "partial.dv",
                                    "DV frame ended before all DIF blocks arrived".into(),
                                ));
                            }
                            self.dv.extend_from_slice(block);
                            let expected = if self.dv[3] & 0x80 != 0 {
                                144000
                            } else {
                                120000
                            };
                            if self.dv.len() >= expected {
                                let data = std::mem::take(&mut self.dv);
                                match Frame::parse(data.clone()) {
                                    Ok(frame) => out.push(Item::Dv(frame)),
                                    Err(e) => {
                                        out.push(Item::Partial(data, "partial.dv", e.to_string()))
                                    }
                                }
                            }
                        }
                    }
                    0x20 if bytes[1] == 6
                        && bytes[2] & 0xc7 == 0xc4
                        && payload.len().is_multiple_of(192) =>
                    {
                        self.change(2, &mut out);
                        if self.dbc.is_some_and(|expected| expected != bytes[3]) {
                            out.push(Item::Partial(Vec::new(), "bin", "HDV FireWire continuity gap; missing source packets cannot be reconstructed".into()));
                        }
                        self.dbc = Some(bytes[3].wrapping_add((payload.len() / 192 * 8) as u8));
                        // Each IEC 61883-4 source packet is a 4-byte SPH + 188 TS bytes.
                        // TS continuity is checked by the inspector, including per-PID gaps.
                        let mut ts = Vec::with_capacity(payload.len() / 192 * 188);
                        for packet in payload.chunks_exact(192) {
                            if packet[4] != 0x47 {
                                self.flush(&mut out);
                                out.push(Item::Partial(
                                    bytes.to_vec(),
                                    "bin",
                                    "Invalid MPEG source packet; CIP payload retained".into(),
                                ));
                                return out;
                            }
                            ts.extend_from_slice(&packet[4..]);
                        }
                        out.push(Item::Hdv(ts));
                    }
                    _ => {
                        self.flush(&mut out);
                        out.push(Item::Partial(
                            bytes.to_vec(),
                            "bin",
                            format!(
                                "Unsupported IEC 61883 packet (FMT {format:#04x}); payload retained"
                            ),
                        ));
                    }
                }
            }
            _ => {
                self.flush(&mut out);
                out.push(Item::Partial(
                    bytes.to_vec(),
                    "bin",
                    "Unknown native sample; bytes retained for recovery".into(),
                ));
            }
        }
        out
    }
}

struct Segment {
    format: u8,
    file: BufWriter<File>,
    path: PathBuf,
    first: Option<Frame>,
    frames: u64,
    bytes: u64,
    last_date: Option<chrono::NaiveDateTime>,
}

/// Hardware-independent mixed-stream writer, also used by native Auto capture.
/// Raw DV files are retained even if AVI packaging fails or the user stops the app.
pub struct CaptureArchive {
    options: CaptureOptions,
    decoder: Decoder,
    segment: Option<Segment>,
    hdv_writer: Option<crate::hdv_capture::Writer>,
    journal: File,
    started: Instant,
    avis: Vec<PathBuf>,
    status: Status,
    split: bool,
    finished: bool,
    last_format: u8,
    preview: Option<(u8, Vec<u8>)>,
}
impl CaptureArchive {
    pub fn new(options: CaptureOptions) -> Result<Self> {
        options.validate()?;
        let (path, journal) = naming::reserve_extension(&options, None, "capture.jsonl")?;
        let mut archive = Self {
            options,
            decoder: Decoder::default(),
            segment: None,
            hdv_writer: None,
            journal,
            started: Instant::now(),
            avis: Vec::new(),
            status: Status {
                report: Some(path),
                phase: Phase::Capturing,
                ..Default::default()
            },
            split: false,
            finished: false,
            last_format: 0,
            preview: None,
        };
        archive.log(json!({"event":"capture_start", "version":1, "zero_loss_verified":false,
            "note":"The report covers received data; the deck/driver may omit data at format boundaries."}))?;
        Ok(archive)
    }
    fn log(&mut self, mut event: serde_json::Value) -> Result<()> {
        event["elapsed_ms"] = json!(self.started.elapsed().as_millis() as u64);
        event["utc"] = json!(chrono::Utc::now().to_rfc3339());
        crate::diagnostics::log("capture.event", event.to_string());
        serde_json::to_writer(&mut self.journal, &event)?;
        self.journal.write_all(b"\n")?;
        self.journal.sync_data()?;
        Ok(())
    }
    pub fn status(&self) -> &Status {
        &self.status
    }
    fn take_preview(&mut self) -> Option<(u8, Vec<u8>)> {
        if self
            .preview
            .as_ref()
            .is_some_and(|(kind, bytes)| *kind == 1 || bytes.len() >= hdv::BLOCK)
        {
            self.preview.take()
        } else {
            None
        }
    }
    pub fn split(&mut self) {
        self.split = true;
    }
    pub fn interruption(&mut self, reason: &str) -> Result<()> {
        self.boundary()?;
        self.status.issues += 1;
        self.log(json!({"event":"interruption", "reason":reason, "missing_frames":null, "recapture_recommended":true}))
    }
    /// Close a boundary without treating an intentional pause as a driver fault.
    pub fn boundary(&mut self) -> Result<()> {
        let mut tail = Vec::new();
        self.decoder.flush(&mut tail);
        self.decoder = Decoder::default();
        for item in tail {
            self.item(item)?;
        }
        self.close_segment()?;
        self.close_hdv()?;
        Ok(())
    }
    pub fn push_tagged(&mut self, bytes: &[u8]) -> Result<()> {
        ensure!(!self.finished, "Capture archive has already been finalized");
        for item in self.decoder.push(bytes) {
            self.item(item)?;
        }
        Ok(())
    }
    fn close_segment(&mut self) -> Result<()> {
        if let Some(mut segment) = self.segment.take() {
            segment.file.flush()?;
            segment.file.get_ref().sync_all()?;
            if segment.format == 1 {
                self.avis.push(segment.path.clone());
            }
            self.log(json!({"event":"segment_end", "path":segment.path, "bytes":segment.bytes}))?;
        }
        Ok(())
    }
    fn update_hdv_files(&mut self) -> Result<()> {
        let writer = self.hdv_writer.as_ref().unwrap();
        let files: Vec<_> = writer
            .files
            .iter()
            .filter(|p| !self.status.files.contains(p))
            .cloned()
            .collect();
        let report = writer.report.clone();
        for path in files {
            self.log(json!({"event":"segment_start", "format":"HDV", "path":path,
                "splitting_report":report}))?;
            self.status.files.push(path);
        }
        Ok(())
    }
    fn close_hdv(&mut self) -> Result<()> {
        if let Some(writer) = &mut self.hdv_writer {
            writer.finish()?;
        }
        if self.hdv_writer.is_some() {
            self.update_hdv_files()?;
        }
        if let Some(writer) = self.hdv_writer.take() {
            let problem = writer.inspector.validate().err().map(|e| e.to_string());
            if problem.is_some() || writer.inspector.errors > 0 {
                self.status.issues += 1;
            }
            self.log(json!({"event":"hdv_end", "files":writer.files, "splitting_report":writer.report,
                "bytes":writer.bytes, "validation_issue":problem, "transport_errors":writer.inspector.errors}))?;
        }
        Ok(())
    }
    fn open_segment(&mut self, format: u8, frame: Option<&Frame>) -> Result<()> {
        self.close_segment()?;
        let (path, file) = naming::reserve_extension(
            &self.options,
            frame.and_then(|f| f.recorded),
            if format == 1 { "dv" } else { "m2t" },
        )?;
        self.log(json!({"event":"segment_start", "format":if format==1 {"DV/DVCAM"} else {"HDV"}, "path":path}))?;
        self.status.files.push(path.clone());
        self.segment = Some(Segment {
            format,
            file: BufWriter::with_capacity(262144, file),
            path,
            first: frame.cloned(),
            frames: 0,
            bytes: 0,
            last_date: None,
        });
        self.split = false;
        Ok(())
    }
    fn switch(&mut self, format: u8) -> Result<()> {
        let old = self.last_format;
        if old != 0 && old != format {
            self.status.switches += 1;
            self.log(json!({"event":"format_switch", "from":if old==1 {"DV/DVCAM"} else {"HDV"},
                "to":if format==1 {"DV/DVCAM"} else {"HDV"}, "boundary_completeness":"unverified", "recapture_recommended":true}))?;
        }
        if self.segment.as_ref().is_some_and(|s| s.format != format) {
            self.close_segment()?;
        }
        if format == 1 {
            self.close_hdv()?;
            self.status.hdv = None;
        } else if old != 2 {
            self.status.standard = None;
            self.status.timecode.clear();
            self.status.recorded.clear();
            self.hdv_writer = None;
        }
        self.last_format = format;
        Ok(())
    }
    fn item(&mut self, item: Item) -> Result<()> {
        match item {
            Item::Partial(bytes, extension, reason) => {
                self.status.issues += 1;
                let recovery = if bytes.is_empty() {
                    None
                } else {
                    let (path, mut file) =
                        naming::reserve_extension(&self.options, None, extension)?;
                    file.write_all(&bytes)?;
                    file.sync_all()?;
                    self.status.files.push(path.clone());
                    Some(path)
                };
                self.log(json!({"event":"incomplete_data", "reason":reason, "recovery_file":recovery, "bytes":bytes.len(), "recapture_recommended":true}))?;
            }
            Item::Dv(frame) => {
                if self.options.preview {
                    self.preview = Some((1, frame.data.clone()));
                }
                self.switch(1)?;
                self.status.standard = Some(frame.standard);
                self.status.timecode = frame.timecode.clone().unwrap_or_default();
                self.status.recorded = frame.recorded.map(|d| d.to_string()).unwrap_or_default();
                let split = self.split
                    || self.segment.as_ref().is_some_and(|s| {
                        let first = s.first.as_ref().unwrap();
                        first.standard != frame.standard
                            || frame.field_order.is_some() && first.field_order != frame.field_order
                            || self.options.max_frames > 0 && s.frames >= self.options.max_frames
                            || self.options.discontinuity_seconds > 0
                                && s.last_date.zip(frame.recorded).is_some_and(|(a, b)| {
                                    (b - a).num_seconds().unsigned_abs()
                                        > self.options.discontinuity_seconds as u64
                                })
                    });
                if split || self.segment.is_none() {
                    self.open_segment(1, Some(&frame))?;
                }
                let s = self.segment.as_mut().unwrap();
                s.file.write_all(&frame.data)?;
                s.frames += 1;
                s.bytes += frame.data.len() as u64;
                if frame.recorded.is_some() {
                    s.last_date = frame.recorded;
                }
                self.status.received += 1;
                self.status.written += 1;
                self.status.bytes += frame.data.len() as u64;
                self.status.seconds += frame.standard.seconds(1);
                self.status.message =
                    "Capturing original DV/DVCAM; AVI prepared after capture".into();
            }
            Item::Hdv(bytes) => {
                if self.options.preview {
                    if self.preview.as_ref().is_some_and(|(kind, _)| *kind != 2) {
                        self.preview = None;
                    }
                    self.preview
                        .get_or_insert_with(|| (2, Vec::new()))
                        .1
                        .extend_from_slice(&bytes);
                }
                self.switch(2)?;
                let writer = self
                    .hdv_writer
                    .get_or_insert_with(|| crate::hdv_capture::Writer::new(self.options.clone()));
                if self.split {
                    writer.request_split();
                    self.split = false;
                }
                for packet in bytes.chunks_exact(hdv::PACKET) {
                    let packet: &[u8; 188] = packet.try_into().unwrap();
                    let before = writer.inspector.pictures;
                    let seconds = writer.inspector.seconds;
                    let errors = writer.inspector.errors;
                    writer.push(packet)?;
                    let info = &writer.inspector;
                    self.status.hdv = info.video.clone();
                    self.status.dropped += info.errors - errors;
                    self.status.seconds += info.seconds - seconds;
                    self.status.received += info.pictures - before;
                    self.status.written += info.pictures - before;
                    self.status.bytes += 188;
                    if let Some(metadata) = &info.metadata {
                        self.status.timecode = metadata.timecode.clone().unwrap_or_default();
                        self.status.recorded =
                            metadata.recorded.map(|d| d.to_string()).unwrap_or_default();
                    }
                }
                self.update_hdv_files()?;
                self.status.message = "Capturing original HDV transport stream".into();
            }
        }
        Ok(())
    }
    pub fn finish(&mut self) -> Result<()> {
        if !self.finished {
            self.boundary()?;
            self.finished = true;
            self.log(json!({"event":"capture_end", "received_units":self.status.received,
                "saved_bytes":self.status.bytes, "reported_lost_or_invalid_units":self.status.dropped,
                "issues":self.status.issues, "switches":self.status.switches, "zero_loss_verified":false}))?;
        }
        Ok(())
    }
    pub fn prepare_avis(&mut self, ffmpeg: &str) -> Result<()> {
        ensure!(self.finished, "Stop receiving before preparing AVI files");
        for path in std::mem::take(&mut self.avis) {
            let result = (|| -> Result<PathBuf> {
                let mut input = Input::open(&path, ffmpeg)?;
                let first = input.next_frame()?.context("Empty DV capture")?;
                let (avi, file) = naming::reserve(&self.options, first.recorded)?;
                let mut writer = Writer::new(file, &avi, self.options.avi_type, &first, ffmpeg)?;
                writer.write(&first)?;
                while let Some(frame) = input.next_frame()? {
                    writer.write(&frame)?;
                }
                writer.finish()?;
                Ok(avi)
            })();
            match result {
                Ok(avi) => {
                    self.log(json!({"event":"avi_prepared", "source":path, "path":avi}))?;
                    self.status.files.push(avi);
                }
                Err(e) => {
                    self.status.issues += 1;
                    self.log(json!({"event":"avi_failed", "source":path, "error":format!("{e:#}"), "raw_dv_retained":true}))?;
                    return Err(e.context("Original DV retained; AVI preparation failed"));
                }
            }
        }
        Ok(())
    }
}

fn publish(archive: &CaptureArchive, status: &Arc<Mutex<Status>>, paused: bool, buffered: usize) {
    let mut s = archive.status.clone();
    s.phase = if paused {
        Phase::CapturePaused
    } else {
        Phase::Capturing
    };
    s.buffered = buffered;
    *status.lock().unwrap() = s;
}

enum StreamEvent {
    Bytes(Vec<u8>),
    Interrupted(String),
    Reopened,
    Recovering(String),
}
fn retry_open<T>(
    stop: &std::sync::atomic::AtomicBool,
    mut open: impl FnMut() -> Result<T>,
    mut notify: impl FnMut(&str),
) -> Result<Option<T>> {
    let started = Instant::now();
    let mut last_notice = None;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(None);
        }
        match open() {
            Ok(session) => return Ok(Some(session)),
            Err(e) if started.elapsed()>=Duration::from_secs(30) =>
                return Err(e.context("Could not open the selected deck within 30 seconds; reconnect it or close another app using it")),
            Err(e) => {
                if last_notice.is_none_or(|last:Instant|last.elapsed()>=Duration::from_secs(2)) {
                    let message=format!("Waiting for the selected deck to reconnect: {e:#}");
                    crate::diagnostics::log("receiver.retry",&message);
                    notify(&message); last_notice=Some(Instant::now());
                }
            }
        }
        for _ in 0..20 {
            if stop.load(Ordering::Relaxed) {
                return Ok(None);
            }
            thread::sleep(Duration::from_millis(25));
        }
    }
}
struct Feed {
    events: Receiver<Result<StreamEvent>>,
    commands: crossbeam_channel::Sender<Transport>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    paused: Arc<std::sync::atomic::AtomicBool>,
    dropped: Arc<std::sync::atomic::AtomicU64>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Feed {
    fn new(id: String, capacity: usize, control: bool, paused: bool) -> Self {
        let buffered = if cfg!(windows) {
            capacity
        } else {
            capacity.saturating_mul(256)
        };
        let (tx, events) = crossbeam_channel::bounded(buffered);
        let (commands, deck) = crossbeam_channel::unbounded();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancelled = stop.clone();
        let paused = Arc::new(std::sync::atomic::AtomicBool::new(paused));
        let is_paused = paused.clone();
        let dropped = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let lost = dropped.clone();
        let worker = thread::spawn(move || {
            let mut session = None;
            // Blocking sends are used only after native reception has stopped.
            // The writer drains these queued samples before joining this worker.
            let send = |event| {
                tx.send(event)
                    .map_err(|_| anyhow::anyhow!("Capture writer closed"))
            };
            let forward = |bytes| {
                if tx.try_send(Ok(StreamEvent::Bytes(bytes))).is_err() {
                    lost.fetch_add(1, Ordering::Relaxed);
                }
            };
            let mut native_drops = 0;
            let result = (|| -> Result<()> {
                session = retry_open(
                    &cancelled,
                    || Session::open_auto(&id, capacity),
                    |message| {
                        let _ = tx.try_send(Ok(StreamEvent::Recovering(message.into())));
                    },
                )?;
                if session.is_none() {
                    return Ok(());
                }
                if control && !is_paused.load(Ordering::Relaxed) {
                    session.as_ref().unwrap().transport(Transport::Play)?;
                }
                while !cancelled.load(Ordering::Relaxed) {
                    let s = session.as_ref().unwrap();
                    for command in deck.try_iter() {
                        s.transport(command)?;
                    }
                    let poll = s.poll();
                    for bytes in s.chunks.try_iter() {
                        forward(bytes);
                    }
                    let count = s.dropped.load(Ordering::Relaxed);
                    lost.fetch_add(count.saturating_sub(native_drops), Ordering::Relaxed);
                    native_drops = count;
                    let reason = poll
                        .err()
                        .map(|e| format!("Driver interrupted reception: {e:#}"));
                    if let Some(reason) = reason {
                        let s = session.take().unwrap();
                        let pending = s.chunks.clone();
                        let counts = s.dropped.clone();
                        drop(s);
                        for bytes in pending.try_iter() {
                            send(Ok(StreamEvent::Bytes(bytes)))?;
                        }
                        lost.fetch_add(
                            counts.load(Ordering::Relaxed).saturating_sub(native_drops),
                            Ordering::Relaxed,
                        );
                        native_drops = 0;
                        send(Ok(StreamEvent::Interrupted(reason)))?;
                        session = retry_open(
                            &cancelled,
                            || Session::open_auto(&id, capacity),
                            |message| {
                                let _ = tx.try_send(Ok(StreamEvent::Recovering(message.into())));
                            },
                        )?;
                        if session.is_none() {
                            return Ok(());
                        }
                        if control && !is_paused.load(Ordering::Relaxed) {
                            session.as_ref().unwrap().transport(Transport::Play)?;
                        }
                        // Do not block a newly started receiver on a report event.
                        let _ = tx.try_send(Ok(StreamEvent::Reopened));
                    }
                }
                Ok(())
            })();
            if let Some(s) = session.take() {
                if control {
                    let _ = s.transport(Transport::Pause);
                }
                let pending = s.chunks.clone();
                let counts = s.dropped.clone();
                drop(s);
                for bytes in pending.try_iter() {
                    if send(Ok(StreamEvent::Bytes(bytes))).is_err() {
                        break;
                    }
                }
                lost.fetch_add(
                    counts.load(Ordering::Relaxed).saturating_sub(native_drops),
                    Ordering::Relaxed,
                );
            }
            if let Err(e) = result {
                let _ = send(Err(e));
            }
        });
        Self {
            events,
            commands,
            stop,
            paused,
            dropped,
            worker: Some(worker),
        }
    }
}
impl Drop for Feed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Unblock a final send even when the writer failed or was abandoned.
        let old = std::mem::replace(&mut self.events, crossbeam_channel::never());
        drop(old);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn preview_item(item: Item, pending: &mut Option<(u8, Vec<u8>)>) {
    match item {
        Item::Dv(frame) => *pending = Some((1, frame.data)),
        Item::Hdv(bytes) => {
            if pending.as_ref().is_some_and(|(kind, _)| *kind != 2) {
                *pending = None;
            }
            pending
                .get_or_insert_with(|| (2, Vec::new()))
                .1
                .extend_from_slice(&bytes);
        }
        _ => {}
    }
}
fn received(
    event: StreamEvent,
    archive: &mut CaptureArchive,
    paused: bool,
    monitor: &mut Decoder,
    display: &mut Option<LivePreview>,
    image: &Arc<Mutex<Option<Vec<u8>>>>,
) -> Result<bool> {
    match event {
        StreamEvent::Bytes(bytes) => {
            let media = bytes.first() != Some(&3) || bytes.len() > 9;
            if paused {
                if archive.options.preview {
                    for item in monitor.push(&bytes) {
                        preview_item(item, &mut archive.preview);
                    }
                }
            } else {
                archive.push_tagged(&bytes)?;
            }
            if let Some((kind, data)) = archive.take_preview()
                && let Some(display) = display
            {
                display.offer(kind, &data);
            }
            Ok(media)
        }
        StreamEvent::Interrupted(reason) => {
            archive.interruption(&reason)?;
            *monitor = Decoder::default();
            archive.preview = None;
            if let Some(display) = display {
                display.reset();
            }
            *image.lock().unwrap() = None;
            Ok(false)
        }
        StreamEvent::Recovering(message) => {
            archive.status.message = message;
            Ok(false)
        }
        StreamEvent::Reopened => {
            archive.log(json!({"event":"receiver_reopened", "recapture_recommended":true}))?;
            Ok(false)
        }
    }
}
fn receive_losses(feed: &Feed, archive: &mut CaptureArchive, previous: &mut u64) -> Result<()> {
    let total = feed.dropped.load(Ordering::Relaxed);
    if total > *previous {
        let count = total - *previous;
        *previous = total;
        archive.status.dropped += count;
        archive.status.issues += 1;
        archive.log(json!({"event":"receive_loss", "reported_lost_units":count,
            "unit":"native frames/packets/blocks, depending on backend", "missing_frames":null,
            "recapture_recommended":true}))?;
    }
    Ok(())
}

/// Idle live view owns one receiver and creates no capture files or journals.
/// Starting a transfer closes this receiver before the transfer opens the deck.
pub fn monitor(
    id: String,
    capacity: usize,
    ffmpeg: &str,
    controls: Receiver<Control>,
    status: Arc<Mutex<Status>>,
    image: Arc<Mutex<Option<Vec<u8>>>>,
) -> Result<()> {
    let feed = Feed::new(id, capacity, false, true);
    let mut decoder = Decoder::default();
    let mut display = LivePreview::new(ffmpeg);
    display.output_to(&image);
    let mut info = hdv::Inspector::default();
    let mut format = 0;
    let mut pending = None;
    {
        let mut s = status.lock().unwrap();
        s.phase = Phase::Monitoring;
        s.message = "Live preview — waiting for the deck".into();
    }
    loop {
        for command in controls.try_iter() {
            match command {
                Control::Stop => return Ok(()),
                Control::Deck(command) => {
                    let _ = feed.commands.send(command);
                }
                _ => {}
            }
        }
        match feed.events.recv_timeout(Duration::from_millis(5)) {
            Ok(event) => match event? {
                StreamEvent::Bytes(bytes) => {
                    for item in decoder.push(&bytes) {
                        let kind = match &item {
                            Item::Dv(_) => 1,
                            Item::Hdv(_) => 2,
                            _ => continue,
                        };
                        if kind != format {
                            info = hdv::Inspector::default();
                            format = kind;
                            let mut s = status.lock().unwrap();
                            s.hdv = None;
                            s.standard = None;
                            s.timecode.clear();
                            s.recorded.clear();
                        }
                        {
                            let mut s = status.lock().unwrap();
                            match &item {
                                Item::Dv(f) => {
                                    s.received += 1;
                                    s.standard = Some(f.standard);
                                    s.timecode = f.timecode.clone().unwrap_or_default();
                                    s.recorded =
                                        f.recorded.map(|d| d.to_string()).unwrap_or_default();
                                }
                                Item::Hdv(bytes) => {
                                    let before = info.pictures;
                                    for p in bytes.chunks_exact(188) {
                                        info.push(p.try_into().unwrap());
                                    }
                                    s.received += info.pictures - before;
                                    s.hdv = info.video.clone();
                                    if let Some(metadata) = &info.metadata {
                                        s.timecode = metadata.timecode.clone().unwrap_or_default();
                                        s.recorded = metadata
                                            .recorded
                                            .map(|d| d.to_string())
                                            .unwrap_or_default();
                                    }
                                }
                                _ => {}
                            }
                            s.message = if kind == 2 {
                                "Live HDV preview"
                            } else {
                                "Live DV/DVCAM preview"
                            }
                            .into();
                            s.dropped = feed.dropped.load(Ordering::Relaxed) + info.errors;
                        }
                        preview_item(item, &mut pending);
                        if let Some((kind, data)) = pending.take() {
                            display.offer(kind, &data);
                        }
                    }
                }
                StreamEvent::Interrupted(reason) => {
                    decoder = Decoder::default();
                    display.reset();
                    info = hdv::Inspector::default();
                    format = 0;
                    pending = None;
                    *image.lock().unwrap() = None;
                    status.lock().unwrap().message = reason;
                }
                StreamEvent::Recovering(reason) => status.lock().unwrap().message = reason,
                StreamEvent::Reopened => {
                    status.lock().unwrap().message = "Receiver reopened; waiting for video".into()
                }
            },
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return Ok(()),
        }
        if let Some(error) = display.error() {
            status.lock().unwrap().message = format!("Restarting preview: {error}");
        }
    }
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
    let Source::Device(id) = source else {
        bail!("Auto capture requires a FireWire device");
    };
    let mut archive = CaptureArchive::new(options.clone())?;
    let mut display = options.preview.then(|| {
        let mut p = LivePreview::new(ffmpeg);
        p.output_to(&image);
        p
    });
    let feed = Feed::new(id, options.buffer_frames, options.control_deck, paused);
    let mut monitor = Decoder::default();
    let mut last_signal = Instant::now();
    let mut gap = false;
    let mut lost = 0;
    publish(&archive, &status, paused, 0);
    let result = (|| -> Result<()> {
        'capture: loop {
            for command in controls.try_iter() {
                match command {
                    Control::Stop => break 'capture,
                    Control::Split => archive.split(),
                    Control::Pause => {
                        archive.boundary()?;
                        paused = true;
                        feed.paused.store(true, Ordering::Relaxed);
                        archive.log(json!({"event":"user_pause", "data_while_paused":"intentionally_not_saved"}))?;
                        if options.control_deck {
                            let _ = feed.commands.send(Transport::Pause);
                        }
                    }
                    Control::Resume => {
                        paused = false;
                        feed.paused.store(false, Ordering::Relaxed);
                        monitor = Decoder::default();
                        last_signal = Instant::now();
                        gap = false;
                        archive.log(json!({"event":"user_resume"}))?;
                        if options.control_deck {
                            let _ = feed.commands.send(Transport::Play);
                        }
                    }
                    Control::Deck(t) => {
                        let _ = feed.commands.send(t);
                    }
                }
            }
            match feed.events.recv_timeout(Duration::from_millis(25)) {
                Ok(event) => {
                    if received(
                        event?,
                        &mut archive,
                        paused,
                        &mut monitor,
                        &mut display,
                        &image,
                    )? {
                        last_signal = Instant::now();
                        gap = false;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            }
            if !paused && last_signal.elapsed() > Duration::from_secs(1) && !gap {
                archive.interruption("No samples for one second; blank tape, a format transition or a driver gap may be responsible")?;
                gap = true;
            }
            if !paused && last_signal.elapsed() > Duration::from_secs(120) {
                bail!("No DV/HDV data for 120 seconds; captured files retained");
            }
            receive_losses(&feed, &mut archive, &mut lost)?;
            publish(&archive, &status, paused, feed.events.len());
            if options.duration_seconds > 0
                && archive.status.seconds >= options.duration_seconds as f64
            {
                break;
            }
        }
        Ok(())
    })();
    // Stop the source first, then drain its FIFO (including callbacks completed
    // during native shutdown) before closing the last segment or preparing AVIs.
    feed.stop.store(true, Ordering::Relaxed);
    let drain = (|| -> Result<()> {
        while let Ok(event) = feed.events.recv() {
            received(
                event?,
                &mut archive,
                paused,
                &mut monitor,
                &mut display,
                &image,
            )?;
        }
        receive_losses(&feed, &mut archive, &mut lost)
    })();
    drop(feed);
    if let Err(e) = &result {
        let _ = archive.log(json!({"event":"capture_error", "error":format!("{e:#}")}));
    }
    if let Err(e) = &drain {
        let _ = archive.log(json!({"event":"shutdown_error", "error":format!("{e:#}")}));
    }
    let finish = archive.finish();
    publish(&archive, &status, paused, 0);
    {
        let mut s = status.lock().unwrap();
        s.phase = Phase::Finalizing;
        s.message = "Capture saved; preparing DV AVI files".into();
    }
    let avis = if finish.is_ok() {
        archive.prepare_avis(ffmpeg)
    } else {
        Ok(())
    };
    publish(&archive, &status, paused, 0);
    status.lock().unwrap().message = format!(
        "Capture saved: {} format switches, {} issues. Review the capture report; boundary completeness is unverified.",
        archive.status.switches, archive.status.issues
    );
    result?;
    drain?;
    finish?;
    avis
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn opening_retries_transient_errors_and_stop_cancels_recovery() {
        let stop = std::sync::atomic::AtomicBool::new(false);
        let mut attempts = 0;
        let result = retry_open(
            &stop,
            || {
                attempts += 1;
                if attempts < 3 {
                    anyhow::bail!("device is changing endpoints");
                }
                Ok(42)
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(result, Some(42));
        assert_eq!(attempts, 3);
        let result: Option<()> = retry_open(
            &stop,
            || {
                stop.store(true, Ordering::Relaxed);
                anyhow::bail!("cancel while opening");
            },
            |_| {},
        )
        .unwrap();
        assert!(result.is_none());
    }
    #[test]
    fn closing_the_writer_unblocks_a_final_source_send() {
        let (tx, events) = crossbeam_channel::bounded(1);
        let (commands, _) = crossbeam_channel::unbounded();
        let ready = Arc::new(std::sync::Barrier::new(2));
        let producer_ready = ready.clone();
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let producer_completed = completed.clone();
        let worker = thread::spawn(move || {
            tx.send(Ok(StreamEvent::Reopened)).unwrap();
            producer_ready.wait();
            assert!(tx.send(Ok(StreamEvent::Reopened)).is_err());
            producer_completed.store(true, Ordering::Relaxed);
        });
        let feed = Feed {
            events,
            commands,
            stop: Arc::new(false.into()),
            paused: Arc::new(false.into()),
            dropped: Arc::new(0.into()),
            worker: Some(worker),
        };
        ready.wait();
        drop(feed);
        assert!(completed.load(Ordering::Relaxed));
    }
}

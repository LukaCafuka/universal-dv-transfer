use crate::{
    avi::Type1Writer,
    config::AviType,
    dv::{self, FieldOrder, Frame},
};
use anyhow::{Context, Result, ensure};
use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::JoinHandle,
};

pub fn command(program: &str) -> Command {
    #[allow(unused_mut)] // Windows creation flags require a mutable Command.
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x08000000);
    }
    c
}
pub fn ffmpeg_name(value: &str) -> &str {
    if value.is_empty() { "ffmpeg" } else { value }
}
pub fn check_ffmpeg(program: &str) -> Result<()> {
    let result = command(ffmpeg_name(program)).arg("-version").output();
    match &result {
        Ok(output) => crate::diagnostics::log(
            "ffmpeg.version",
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .unwrap_or("FFmpeg returned no version"),
        ),
        Err(e) => crate::diagnostics::log(
            "ffmpeg.error",
            format!("Cannot run {}: {e}", ffmpeg_name(program)),
        ),
    }
    let output = result.context(
        "FFmpeg is required for Type 2 AVI, AVI input and preview; install FFmpeg or set its path",
    )?;
    ensure!(
        output.status.success(),
        "FFmpeg failed to run: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

/// Child stderr is drained continuously, retaining a bounded tail for useful errors.
pub struct Process {
    pub child: Child,
    tail: Arc<Mutex<Vec<u8>>>,
    drain: Option<JoinHandle<()>>,
}
impl Process {
    pub fn spawn(mut c: Command) -> Result<Self> {
        crate::diagnostics::log("ffmpeg.start", format!("{c:?}"));
        c.stderr(Stdio::piped());
        let mut child = c
            .spawn()
            .inspect_err(|e| crate::diagnostics::log("ffmpeg.error", e.to_string()))
            .context("Could not launch FFmpeg")?;
        let mut stderr = child.stderr.take().unwrap();
        let tail = Arc::new(Mutex::new(Vec::new()));
        let out = tail.clone();
        let drain = std::thread::spawn(move || {
            let mut b = [0; 2048];
            while let Ok(n) = stderr.read(&mut b) {
                if n == 0 {
                    break;
                }
                crate::diagnostics::log(
                    "ffmpeg.stderr",
                    String::from_utf8_lossy(&b[..n]).trim_end(),
                );
                let mut t = out.lock().unwrap();
                t.extend_from_slice(&b[..n]);
                if t.len() > 8192 {
                    let excess = t.len() - 8192;
                    t.drain(..excess);
                }
            }
        });
        Ok(Self {
            child,
            tail,
            drain: Some(drain),
        })
    }
    pub fn finish(&mut self) -> Result<()> {
        self.child.stdin.take();
        let status = self.child.wait()?;
        if let Some(h) = self.drain.take() {
            let _ = h.join();
        }
        ensure!(
            status.success(),
            "FFmpeg failed: {}",
            String::from_utf8_lossy(&self.tail.lock().unwrap())
        );
        Ok(())
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(h) = self.drain.take() {
            let _ = h.join();
        }
    }
}

pub enum Input {
    Raw(File),
    Demux {
        process: Process,
        stdout: ChildStdout,
    },
}
impl Input {
    pub fn open(path: &Path, ffmpeg: &str) -> Result<Self> {
        ensure!(path.is_file(), "Input does not exist: {}", path.display());
        if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("dv"))
        {
            return Ok(Self::Raw(File::open(path)?));
        }
        let mut header = Vec::new();
        File::open(path)?.take(65536).read_to_end(&mut header)?;
        let type1 = header.starts_with(b"RIFF")
            && header
                .windows(12)
                .any(|w| &w[..4] == b"strh" && &w[8..12] == b"iavs");
        // The DV muxer rebuilds the interleaved audio from Type 2's PCM tracks.
        let mut c = command(ffmpeg_name(ffmpeg));
        c.args(["-v", "error", "-nostdin", "-i"])
            .arg(path)
            .args([
                "-map",
                "0:v:0",
                "-map",
                "0:a?",
                "-c:v",
                "copy",
                "-c:a",
                "pcm_s16le",
                "-f",
                "dv",
                "pipe:1",
            ])
            .stdout(Stdio::piped())
            .stdin(Stdio::null());
        if type1 {
            c = command(ffmpeg_name(ffmpeg));
            c.args(["-v", "error", "-nostdin", "-i"])
                .arg(path)
                .args(["-map", "0:v:0", "-c:v", "copy", "-f", "rawvideo", "pipe:1"])
                .stdout(Stdio::piped())
                .stdin(Stdio::null());
        }
        let mut process = Process::spawn(c)?;
        let stdout = process.child.stdout.take().unwrap();
        Ok(Self::Demux { process, stdout })
    }
    pub fn next_frame(&mut self) -> Result<Option<Frame>> {
        match self {
            Self::Raw(r) => dv::read_frame(r),
            Self::Demux { process, stdout } => {
                let f = dv::read_frame(stdout)?;
                if f.is_none() {
                    process.finish()?;
                }
                Ok(f)
            }
        }
    }
}

pub enum Writer {
    Type1(Type1Writer),
    Type2 {
        process: Process,
        stdin: Option<ChildStdin>,
        path: PathBuf,
    },
}
impl Writer {
    pub fn new(
        file: File,
        path: &Path,
        kind: AviType,
        frame: &Frame,
        ffmpeg: &str,
    ) -> Result<Self> {
        match kind {
            AviType::Type1 => Ok(Self::Type1(Type1Writer::new(file, frame.standard)?)),
            AviType::Type2 => {
                drop(file);
                let mut c = command(ffmpeg_name(ffmpeg));
                let (num, den) = frame.standard.rate();
                let rate = format!("{num}/{den}");
                c.args([
                    "-v",
                    "error",
                    "-nostdin",
                    "-y",
                    "-f",
                    "dv",
                    "-i",
                    "pipe:0",
                    "-map",
                    "0:v:0",
                    "-map",
                    "0:a?",
                    "-c:v",
                    "copy",
                    "-c:a",
                    "pcm_s16le",
                    "-f",
                    "avi",
                ]);
                // Stream copy does not reliably propagate DV's field flags to
                // codecpar. Set the AVI header explicitly; no video is decoded
                // or re-encoded. Keep unknown metadata unspecified.
                if let Some(order) = frame.field_order {
                    c.args([
                        "-field_order:v:0",
                        match order {
                            FieldOrder::Progressive => "progressive",
                            FieldOrder::BottomFirst => "bb",
                            FieldOrder::TopFirst => "tt",
                        },
                    ]);
                }
                c.args(["-r", &rate])
                    .arg(path)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null());
                let mut process = Process::spawn(c)?;
                let stdin = process.child.stdin.take();
                Ok(Self::Type2 {
                    process,
                    stdin,
                    path: path.to_owned(),
                })
            }
        }
    }
    pub fn write(&mut self, f: &Frame) -> Result<()> {
        match self {
            Self::Type1(w) => w.write(f),
            Self::Type2 { stdin, .. } => {
                stdin
                    .as_mut()
                    .context("AVI writer closed")?
                    .write_all(&f.data)?;
                Ok(())
            }
        }
    }
    pub fn finish(self) -> Result<()> {
        match self {
            Self::Type1(w) => w.finish(),
            Self::Type2 {
                mut process,
                stdin,
                path,
            } => {
                drop(stdin);
                process
                    .finish()
                    .with_context(|| format!("Incomplete AVI retained at {}", path.display()))
            }
        }
    }
}

pub fn expand_playlist(items: &[PathBuf], leader: &str, trailer: &str) -> Result<Vec<PathBuf>> {
    let mut result = Vec::new();
    for p in (!leader.is_empty())
        .then(|| PathBuf::from(leader))
        .into_iter()
        .chain(items.iter().cloned())
        .chain((!trailer.is_empty()).then(|| PathBuf::from(trailer)))
    {
        let mut matched = if p.to_string_lossy().contains(['*', '?', '[']) {
            glob::glob(p.to_str().context("Non-Unicode wildcard path")?)?
                .collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            vec![p.clone()]
        };
        matched.sort_by_key(|p| p.to_string_lossy().to_lowercase());
        ensure!(!matched.is_empty(), "No files match {}", p.display());
        for path in matched {
            ensure!(path.is_file(), "Not a file: {}", path.display());
            result.push(path);
        }
    }
    ensure!(!result.is_empty(), "Select at least one DV or DV AVI file");
    Ok(result)
}

pub type PreviewOutput = Arc<Mutex<Option<Vec<u8>>>>;
pub struct Preview {
    tx: crossbeam_channel::Sender<Vec<u8>>,
    pub rx: crossbeam_channel::Receiver<Vec<u8>>,
    _worker: JoinHandle<()>,
    failure: Arc<Mutex<Option<String>>>,
    cancelled: Arc<AtomicBool>,
    pub decoded: Arc<AtomicU64>,
    hdv: bool,
    period: Arc<AtomicU64>,
    output: Arc<Mutex<Option<PreviewOutput>>>,
    probe: Mutex<(crate::hdv::Packetizer, crate::hdv::Inspector, Vec<u8>)>,
}
impl Preview {
    pub fn new(ffmpeg: &str) -> Result<Self> {
        Self::new_format(ffmpeg, false)
    }
    pub fn new_hdv(ffmpeg: &str) -> Result<Self> {
        Self::new_format(ffmpeg, true)
    }
    fn new_format(ffmpeg: &str, hdv: bool) -> Result<Self> {
        crate::diagnostics::log(
            "preview",
            if hdv {
                "Starting HDV preview"
            } else {
                "Starting DV preview"
            },
        );
        let mut c = command(ffmpeg_name(ffmpeg));
        c.args([
            "-v",
            "error",
            "-nostdin",
            "-probesize",
            if hdv { "1048576" } else { "32" },
            "-analyzeduration",
            if hdv { "200000" } else { "0" },
            "-fpsprobesize",
            "0",
            "-f",
            if hdv { "mpegts" } else { "dv" },
            "-threads",
            if hdv { "2" } else { "1" },
            "-i",
            "pipe:0",
            "-an",
            "-threads",
            "1",
            "-filter_threads",
            "1",
            "-vf",
            if hdv {
                "scale=320:180:flags=fast_bilinear:interl=-1,bwdif=mode=send_field:parity=auto:deint=interlaced,pad=320:240:0:30"
            } else {
                "scale=320:240:flags=fast_bilinear:interl=-1,bwdif=mode=send_field:parity=auto:deint=interlaced"
            },
            "-fps_mode",
            "passthrough",
            "-pix_fmt",
            "rgb24",
            "-f",
            "rawvideo",
            "-flush_packets",
            "1",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
        Self::spawn(c, hdv)
    }
    fn spawn(c: Command, hdv: bool) -> Result<Self> {
        let mut p = Process::spawn(c)?;
        let mut stdin = p.child.stdin.take().unwrap();
        let mut stdout = p.child.stdout.take().unwrap();
        // HDV callbacks range from one TS packet to a full driver sample.
        // Coalesced blocks give each backend the same bounded startup allowance
        // (about four seconds / 12 MiB), including slower FFmpeg cold starts.
        let (tx, input) = crossbeam_channel::bounded::<Vec<u8>>(if hdv { 256 } else { 4 });
        let (out, rx) = crossbeam_channel::bounded(1);
        let old = rx.clone();
        let failure = Arc::new(Mutex::new(None));
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = cancelled.clone();
        let decoded = Arc::new(AtomicU64::new(0));
        let frames = decoded.clone();
        let period = Arc::new(AtomicU64::new(if hdv { 16683 } else { 20000 }));
        let interval = period.clone();
        let output: Arc<Mutex<Option<PreviewOutput>>> = Arc::new(Mutex::new(None));
        let sink = output.clone();
        let reader_stop = cancelled.clone();
        let failed = failure.clone();
        let tail = p.tail.clone();
        let reader = std::thread::spawn(move || {
            let mut first = true;
            let mut delivered = None;
            loop {
                let mut rgb = vec![0; 320 * 240 * 3];
                if stdout.read_exact(&mut rgb).is_err() {
                    break;
                }
                if let Some(last) = delivered {
                    let elapsed = std::time::Instant::now().duration_since(last);
                    if let Some(wait) =
                        std::time::Duration::from_micros(interval.load(Ordering::Relaxed))
                            .checked_sub(elapsed)
                    {
                        std::thread::sleep(wait);
                    }
                }
                if reader_stop.load(Ordering::Relaxed) {
                    break;
                }
                delivered = Some(std::time::Instant::now());
                if first {
                    crate::diagnostics::log("preview", "Decoded first RGB frame (320x240)");
                    first = false;
                }
                frames.fetch_add(1, Ordering::Relaxed);
                if let Some(target) = sink.lock().unwrap().as_ref() {
                    *target.lock().unwrap() = Some(rgb.clone());
                }
                let _ = old.try_recv();
                if out.send(rgb).is_err() {
                    break;
                }
            }
        });
        let worker = std::thread::spawn(move || {
            let mut error = None;
            while !stop.load(Ordering::Relaxed) {
                if let Ok(Some(exit)) = p.child.try_wait() {
                    error = Some(format!("Preview FFmpeg exited ({exit})"));
                    break;
                }
                match input.recv_timeout(std::time::Duration::from_millis(100)) {
                    Ok(f) => {
                        if let Err(e) = stdin.write_all(&f) {
                            error = Some(format!("Preview input failed: {e}"));
                            break;
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                }
            }
            let _ = p.child.kill();
            drop(stdin);
            let _ = reader.join();
            if let Some(error) = error.filter(|_| !stop.load(Ordering::Relaxed)) {
                let detail = format!(
                    "{error}: {}",
                    String::from_utf8_lossy(&tail.lock().unwrap())
                );
                crate::diagnostics::log("preview.error", &detail);
                *failed.lock().unwrap() = Some(detail);
            }
        });
        Ok(Self {
            tx,
            rx,
            _worker: worker,
            failure,
            cancelled,
            decoded,
            hdv,
            period,
            output,
            probe: Mutex::new((
                crate::hdv::Packetizer::default(),
                crate::hdv::Inspector::default(),
                Vec::new(),
            )),
        })
    }
    pub fn error(&self) -> Option<String> {
        self.failure.lock().unwrap().clone()
    }
    pub fn output_to(&self, target: &PreviewOutput) {
        *self.output.lock().unwrap() = Some(target.clone());
    }
    pub fn offer(&self, f: &Frame) {
        self.offer_bytes(&f.data);
    }
    pub fn offer_bytes(&self, bytes: &[u8]) {
        if self.hdv {
            let mut probe = self.probe.lock().unwrap();
            let (packets, info, pending) = &mut *probe;
            for packet in packets.push(bytes) {
                info.push(&packet);
            }
            if let Some(video) = &info.video {
                self.period.store(
                    (1_000_000. / video.rate.value() / if video.progressive { 1. } else { 2. })
                        as u64,
                    Ordering::Relaxed,
                );
            }
            pending.extend_from_slice(bytes);
            let (blocks, remainder) = pending.as_chunks::<{ crate::hdv::BLOCK }>();
            let complete = pending.len() - remainder.len();
            for block in blocks {
                if self.tx.try_send(block.to_vec()).is_err() {
                    // Never continue decoding across a missing compressed chunk.
                    *self.failure.lock().unwrap() =
                        Some("Preview decoder queue overflow; resynchronizing".into());
                    self.cancelled.store(true, Ordering::Relaxed);
                    break;
                }
            }
            pending.drain(..complete);
        } else if let Ok(frame) = Frame::parse(bytes.to_vec()) {
            self.period.store(
                (frame.standard.seconds(1) * 1_000_000.
                    / if frame.field_order == Some(FieldOrder::Progressive) {
                        1.
                    } else {
                        2.
                    }) as u64,
                Ordering::Relaxed,
            );
        }
        if !self.hdv {
            // DV is intraframe: skipping a preview frame cannot break the next.
            let _ = self.tx.try_send(bytes.to_vec());
        }
    }
}
impl Drop for Preview {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

/// One decoder for the currently received format. Reconnects and format changes
/// invalidate compressed references, so a decoder from an earlier run is retired.
pub struct LivePreview {
    ffmpeg: String,
    decoder: Option<(u8, Preview)>,
    retry: std::time::Instant,
    output: Option<PreviewOutput>,
    failure: Option<String>,
}
impl LivePreview {
    pub fn new(ffmpeg: &str) -> Self {
        Self {
            ffmpeg: ffmpeg.into(),
            decoder: None,
            retry: std::time::Instant::now(),
            output: None,
            failure: None,
        }
    }
    pub fn reset(&mut self) {
        self.decoder = None;
        self.failure = None;
        self.retry = std::time::Instant::now();
    }
    pub fn offer(&mut self, kind: u8, bytes: &[u8]) {
        let restart = self
            .decoder
            .as_ref()
            .is_some_and(|(active, p)| *active != kind || p.error().is_some());
        if restart {
            self.reset();
        }
        if self.decoder.is_none() && std::time::Instant::now() >= self.retry {
            let result = if kind == 2 {
                Preview::new_hdv(&self.ffmpeg)
            } else {
                Preview::new(&self.ffmpeg)
            };
            match result {
                Ok(p) => {
                    if let Some(output) = &self.output {
                        p.output_to(output);
                    }
                    self.failure = None;
                    self.decoder = Some((kind, p));
                }
                Err(e) => {
                    crate::diagnostics::log("preview.error", format!("{e:#}"));
                    self.failure = Some(format!("Preview unavailable: {e:#}"));
                    self.retry = std::time::Instant::now() + std::time::Duration::from_secs(2);
                }
            }
        }
        if let Some((_, p)) = &self.decoder {
            p.offer_bytes(bytes);
        }
    }
    pub fn latest(&self) -> Option<Vec<u8>> {
        self.decoder
            .as_ref()
            .and_then(|(_, p)| p.rx.try_iter().last())
    }
    pub fn output_to(&mut self, target: &PreviewOutput) {
        if self
            .output
            .as_ref()
            .is_some_and(|old| Arc::ptr_eq(old, target))
        {
            return;
        }
        if let Some((_, p)) = &self.decoder {
            p.output_to(target);
        }
        self.output = Some(target.clone());
    }
    pub fn error(&self) -> Option<String> {
        self.decoder
            .as_ref()
            .and_then(|(_, p)| p.error())
            .or(self.failure.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires FFmpeg installed"]
    fn preview_reports_early_decoder_exit() {
        let mut c = command("ffmpeg");
        c.arg("-udvt_invalid_preview_option")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        let preview = Preview::spawn(c, false).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while preview.error().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            preview.error().is_some(),
            "Early decoder exit must be available for diagnostics"
        );
    }
}

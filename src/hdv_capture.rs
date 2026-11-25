//! Lossless HDV scene splitting. All original TS packets are written once as
//! unique data; new files also carry a bounded, explicitly indexed preroll.
use crate::{
    config::CaptureOptions,
    hdv::{self, Inspector, PACKET},
    naming,
};
use anyhow::{Context, Result};
use chrono::NaiveDateTime;
use serde_json::json;
use std::{
    collections::VecDeque,
    fs::File,
    io::{BufWriter, Write},
    path::PathBuf,
};

type Packet = [u8; PACKET];
const CACHE_PACKETS: usize = 8 * 1024 * 1024 / PACKET;
const INITIAL_PACKETS: usize = 512 * 1024 / PACKET;

#[derive(Default)]
struct Planner {
    cache: VecDeque<Packet>,
    base: u64,
    gops: VecDeque<u64>,
    header: Vec<u8>,
    pes_start: u64,
    identified: bool,
    serial: u64,
    last_date: Option<NaiveDateTime>,
    last_marker: Option<bool>,
    request: Option<&'static str>,
    date: Option<NaiveDateTime>,
    first_picture: u64,
    errors: u64,
    video: Option<hdv::VideoInfo>,
    video_pid: Option<u16>,
}
struct Cut {
    prefix: Vec<Packet>,
    overlap_start: u64,
    reason: &'static str,
    date: Option<NaiveDateTime>,
    at: u64,
    audio_tail: Vec<AudioTail>,
}
struct AudioTail {
    id: u16,
    remaining: Option<usize>,
    cc: Option<u8>,
}
struct Tail {
    file: BufWriter<File>,
    path: PathBuf,
    remaining: Vec<AudioTail>,
    packets: u64,
}
fn payload(packet: &Packet) -> Option<&[u8]> {
    if packet[0] != 0x47 || packet[1] & 0x80 != 0 || packet[3] & 0xd0 != 0x10 {
        return None;
    }
    let offset = if packet[3] & 0x20 != 0 {
        5 + packet[4] as usize
    } else {
        4
    };
    packet.get(offset..)
}
impl Planner {
    fn holding(&self) -> bool {
        !self.identified
            && !self.header.is_empty()
            && self.header.len() < 2048
            && self.pes_start >= self.base
    }
    fn push(
        &mut self,
        packet: &Packet,
        info: &Inspector,
        options: &CaptureOptions,
        allow_cut: bool,
    ) -> Option<Cut> {
        let index = info.packets - 1;
        if info.errors != self.errors || self.video_pid != info.video_pid {
            self.gops.clear();
            self.header.clear();
            self.errors = info.errors;
            self.video_pid = info.video_pid;
        }
        if self.video != info.video {
            if self.video.is_some() {
                self.gops.clear();
                self.request.get_or_insert("video_format_change");
            }
            self.video = info.video.clone();
        }
        self.cache.push_back(*packet);
        while self.cache.len() > CACHE_PACKETS {
            self.cache.pop_front();
            self.base += 1;
        }
        if info.metadata_serial != self.serial {
            self.serial = info.metadata_serial;
            if let Some(metadata) = &info.metadata {
                if options.discontinuity_seconds > 0 {
                    let gap = self.last_date.zip(metadata.recorded).is_some_and(|(a, b)| {
                        (b - a).num_seconds().unsigned_abs()
                            > u64::from(options.discontinuity_seconds)
                    });
                    let marker = self.last_marker == Some(false) && metadata.scene_start;
                    if gap || marker {
                        self.request = Some(if gap {
                            "recording_date_gap"
                        } else {
                            "recording_start"
                        });
                        self.date = metadata.recorded;
                    }
                }
                self.last_marker = Some(metadata.scene_start);
                if metadata.recorded.is_some() {
                    self.last_date = metadata.recorded;
                }
            }
        }
        if options.max_frames > 0
            && info.pictures.saturating_sub(self.first_picture) >= options.max_frames
        {
            self.request.get_or_insert("frame_limit");
        }
        if Some(hdv::pid(packet)) != info.video_pid {
            return None;
        }
        let data = payload(packet)?;
        if packet[1] & 0x40 != 0 {
            self.header.clear();
            self.pes_start = index;
            self.identified = false;
        }
        if self.identified || self.header.len() >= 2048 {
            return None;
        }
        self.header
            .extend_from_slice(&data[..data.len().min(2048 - self.header.len())]);
        if self.header.len() < 9 || self.header[..3] != [0, 0, 1] || self.header[3] & 0xf0 != 0xe0 {
            return None;
        }
        let skip = 9 + self.header[8] as usize;
        let es = self.header.get(skip..)?;
        // RAI alone does not prove a GOP boundary. Inspect the actual MPEG
        // sequence/GOP/I-picture headers, even when they span TS packets.
        let sequence = es.windows(4).position(|p| p == [0, 0, 1, 0xb3])?;
        let group = es.windows(8).position(|p| p[..4] == [0, 0, 1, 0xb8])?;
        let picture = es.windows(6).position(|p| p[..4] == [0, 0, 1, 0])?;
        if sequence > group || group > picture || (es[picture + 5] >> 3) & 7 != 1 {
            return None;
        }
        self.identified = true;
        let anchor = self.gops.back().copied();
        self.gops.push_back(self.pes_start);
        while self.gops.len() > 2 {
            self.gops.pop_front();
        }
        if let Some(&keep) = self.gops.front() {
            while self.base < keep && !self.cache.is_empty() {
                self.cache.pop_front();
                self.base += 1;
            }
        }
        let reason = self.request?;
        if !allow_cut {
            return None;
        }
        let anchor = anchor?;
        if anchor < self.base || info.tables.is_empty() {
            return None;
        }
        let overlap: Vec<_> = self
            .cache
            .iter()
            .take((self.pes_start - self.base) as usize)
            .copied()
            .collect();
        // A prior GOP gives the new GOP all backward video references. Keep
        // audio only when its PES start is also present in that preroll. If a
        // camera uses unusually long PES packets, defer rather than guess.
        if info.audio_pids.iter().any(|id| {
            !overlap
                .iter()
                .any(|p| hdv::pid(p) == *id && p[1] & 0x40 != 0 && payload(p).is_some())
        }) {
            return None;
        }
        let mut audio_tail = Vec::new();
        for &id in &info.audio_pids {
            let first = overlap
                .iter()
                .rposition(|p| hdv::pid(p) == id && p[1] & 64 != 0)?;
            let mut header = Vec::new();
            let mut size = 0;
            let mut cc = None;
            for p in overlap[first..].iter().filter(|p| hdv::pid(p) == id) {
                if let Some(data) = payload(p) {
                    if cc == Some(p[3] & 15) {
                        continue;
                    }
                    cc = Some(p[3] & 15);
                    size += data.len();
                    if header.len() < 6 {
                        header.extend_from_slice(&data[..data.len().min(6 - header.len())]);
                    }
                }
            }
            if header.len() < 6 || header[..3] != [0, 0, 1] {
                return None;
            }
            let length = usize::from(u16::from_be_bytes([header[4], header[5]]));
            if length == 0 {
                audio_tail.push(AudioTail {
                    id,
                    remaining: None,
                    cc,
                });
            } else if size < 6 + length {
                audio_tail.push(AudioTail {
                    id,
                    remaining: Some(6 + length - size),
                    cc,
                });
            }
        }
        let mut tables = info.tables.clone();
        // Cached PSI may be newer than the overlap. Adjust only the added
        // copies' counters so that original packets retain their exact bytes.
        for id in [Some(0), info.pmt_pid].into_iter().flatten() {
            if let Some(first) = overlap
                .iter()
                .find(|p| hdv::pid(p) == id && p[3] & 0x10 != 0)
            {
                let count = tables.iter().filter(|p| hdv::pid(p) == id).count() as u8;
                let mut cc = (first[3] & 15).wrapping_sub(count) & 15;
                for p in tables.iter_mut().filter(|p| hdv::pid(p) == id) {
                    p[3] = (p[3] & 0xf0) | cc;
                    cc = (cc + 1) & 15;
                }
            }
        }
        tables.extend(overlap);
        self.request = None;
        self.first_picture = info.pictures.saturating_sub(1);
        Some(Cut {
            prefix: tables,
            overlap_start: self.base,
            reason,
            at: self.pes_start,
            audio_tail,
            date: self.date.take().or(self.last_date),
        })
    }
}

/// Shared by fixed HDV capture and mixed DV/HDV capture. Splits do not run an
/// encoder or muxer, and preserve unknown/private PIDs as well as video/audio.
pub struct Writer {
    pub inspector: Inspector,
    pub files: Vec<PathBuf>,
    pub report: Option<PathBuf>,
    pub bytes: u64,
    options: CaptureOptions,
    planner: Planner,
    file: Option<BufWriter<File>>,
    journal: Option<File>,
    initial: Vec<Packet>,
    pending: VecDeque<(u64, Packet)>,
    finished: bool,
    tail: Option<Tail>,
}
impl Writer {
    pub fn new(options: CaptureOptions) -> Self {
        Self {
            inspector: Inspector::default(),
            files: Vec::new(),
            report: None,
            bytes: 0,
            options,
            planner: Planner::default(),
            file: None,
            journal: None,
            initial: Vec::new(),
            pending: VecDeque::new(),
            finished: false,
            tail: None,
        }
    }
    pub fn request_split(&mut self) {
        self.planner.request = Some("manual");
    }
    pub fn waiting(&self) -> bool {
        self.planner.request.is_some()
    }
    fn event(&mut self, event: serde_json::Value) -> Result<()> {
        crate::diagnostics::log("hdv.segment", event.to_string());
        if let Some(file) = &mut self.journal {
            serde_json::to_writer(&mut *file, &event)?;
            file.write_all(b"\n")?;
            file.sync_data()?;
        }
        Ok(())
    }
    fn open(&mut self, date: Option<NaiveDateTime>) -> Result<()> {
        let (path, file) = naming::reserve_extension(&self.options, date, "m2t")?;
        self.files.push(path);
        self.file = Some(BufWriter::with_capacity(hdv::BLOCK * 4, file));
        Ok(())
    }
    fn sync(&mut self) -> Result<()> {
        if let Some(mut file) = self.file.take() {
            file.flush()?;
            file.get_ref().sync_all()?;
        }
        Ok(())
    }
    pub fn push(&mut self, packet: &Packet) -> Result<()> {
        anyhow::ensure!(
            !self.finished,
            "HDV capture writer has already been finalized"
        );
        self.inspector.push(packet);
        self.feed_tail(packet)?;
        let cut = self
            .planner
            .push(packet, &self.inspector, &self.options, self.tail.is_none());
        self.bytes += PACKET as u64;
        if self.file.is_none() {
            self.initial.push(*packet);
            if self.inspector.metadata.is_some() || self.initial.len() >= INITIAL_PACKETS {
                self.open(self.inspector.metadata.as_ref().and_then(|m| m.recorded))?;
                let start = self.inspector.packets - self.initial.len() as u64;
                self.pending.extend(
                    self.initial
                        .drain(..)
                        .enumerate()
                        .map(|(n, p)| (start + n as u64, p)),
                );
                self.flush_pending(if self.planner.holding() {
                    self.planner.pes_start
                } else {
                    u64::MAX
                })?;
            }
            return Ok(());
        }
        self.pending
            .push_back((self.inspector.packets - 1, *packet));
        if let Some(cut) = cut {
            if self.journal.is_none() {
                let (path, file) = naming::reserve_extension(&self.options, None, "hdv.jsonl")?;
                self.report = Some(path);
                self.journal = Some(file);
                self.event(json!({"event":"segment", "path":self.files[0], "input_start_packet":0,
                    "prefix_packets":0, "reason":"capture_start", "packet_size":188,
                    "reconstruction":"Concatenate files in this order, skipping prefix_packets and any segment_tail tail_packets."}))?;
            }
            self.flush_pending(cut.at)?;
            if cut.audio_tail.is_empty() {
                self.sync()?;
            } else {
                self.tail = Some(Tail {
                    file: self.file.take().unwrap(),
                    path: self.files.last().unwrap().clone(),
                    remaining: cut.audio_tail,
                    packets: 0,
                });
                let pending: Vec<_> = self.pending.iter().map(|(_, p)| *p).collect();
                for p in pending {
                    self.feed_tail(&p)?;
                }
            }
            self.open(cut.date)?;
            for p in &cut.prefix {
                self.file.as_mut().unwrap().write_all(p)?;
            }
            self.event(json!({"event":"segment", "path":self.files.last(),
                "input_start_packet":cut.at, "prefix_packets":cut.prefix.len(),
                "overlap_start_packet":cut.overlap_start, "recorded":cut.date,
                "reason":cut.reason, "note":"Preroll overlaps the preceding file to preserve MPEG references and audio PES."}))?;
        }
        self.flush_pending(if self.planner.holding() {
            self.planner.pes_start
        } else {
            u64::MAX
        })?;
        Ok(())
    }
    fn feed_tail(&mut self, packet: &Packet) -> Result<()> {
        if let Some(tail) = &mut self.tail {
            if let Some(at) = tail
                .remaining
                .iter()
                .position(|state| state.id == hdv::pid(packet))
                && let Some(data) = payload(packet)
            {
                if tail.remaining[at].cc == Some(packet[3] & 15) {
                    return Ok(());
                }
                tail.remaining[at].cc = Some(packet[3] & 15);
                if packet[1] & 64 != 0 {
                    tail.remaining.remove(at);
                } else {
                    tail.file.write_all(packet)?;
                    tail.packets += 1;
                    if let Some(left) = &mut tail.remaining[at].remaining {
                        *left = left.saturating_sub(data.len());
                        if *left == 0 {
                            tail.remaining.remove(at);
                        }
                    }
                }
            }
            if tail.remaining.is_empty() {
                self.close_tail()?;
            }
        }
        Ok(())
    }
    fn close_tail(&mut self) -> Result<()> {
        if let Some(mut tail) = self.tail.take() {
            tail.file.flush()?;
            tail.file.get_ref().sync_all()?;
            self.event(json!({"event":"segment_tail", "path":tail.path, "tail_packets":tail.packets,
                "audio_pes_complete":tail.remaining.is_empty(),
                "note":"These duplicated packets complete audio PES across the video cut. Skip this tail when reconstructing the original stream."}))?;
        }
        Ok(())
    }
    fn flush_pending(&mut self, until: u64) -> Result<()> {
        while self
            .pending
            .front()
            .is_some_and(|(index, _)| *index < until)
        {
            let (_, packet) = self.pending.front().unwrap();
            self.file
                .as_mut()
                .context("HDV output file could not be opened; pending packets were not written")?
                .write_all(packet)?;
            self.pending.pop_front();
        }
        Ok(())
    }
    pub fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        if !self.initial.is_empty() {
            self.open(self.inspector.metadata.as_ref().and_then(|m| m.recorded))?;
            for p in self.initial.drain(..) {
                self.file.as_mut().unwrap().write_all(&p)?;
            }
        }
        self.flush_pending(u64::MAX)?;
        self.sync()?;
        self.close_tail()?;
        if self.journal.is_some() {
            self.event(
                json!({"event":"end", "input_packets":self.inspector.packets,
                "transport_errors":self.inspector.errors, "pending_split":self.waiting()}),
            )?;
            self.journal.take();
        }
        self.finished = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn packet(id: u16, start: bool, bytes: &[u8]) -> Packet {
        let mut p = [0xff; PACKET];
        p[..4].copy_from_slice(&[
            0x47,
            (id >> 8) as u8 | if start { 64 } else { 0 },
            id as u8,
            0x30,
        ]);
        p[4] = (183 - bytes.len()) as u8;
        p[5] = 0;
        p[PACKET - bytes.len()..].copy_from_slice(bytes);
        p
    }
    fn gop() -> Vec<u8> {
        [
            vec![0, 0, 1, 0xe0, 0, 0, 0x80, 0, 0],
            vec![0, 0, 1, 0xb3, 0x50, 0x02, 0xd0, 0x34],
            vec![0, 0, 1, 0xb8, 0, 8, 0, 0],
            vec![0, 0, 1, 0, 0, 8],
        ]
        .concat()
    }
    fn feed(
        planner: &mut Planner,
        info: &mut Inspector,
        options: &CaptureOptions,
        p: Packet,
    ) -> Option<Cut> {
        info.packets += 1;
        planner.push(&p, info, options, true)
    }
    #[test]
    fn split_waits_for_fragmented_mpeg_headers_and_audio_pes_not_just_rai() {
        let options = CaptureOptions {
            max_frames: 0,
            ..Default::default()
        };
        let mut info = Inspector::default();
        info.video_pid = Some(256);
        info.audio_pids = vec![257];
        info.tables = vec![packet(0, true, &[0])];
        let mut planner = Planner::default();
        assert!(feed(&mut planner, &mut info, &options, packet(256, true, &gop())).is_none());
        planner.request = Some("manual");
        // No audio PES in preroll: preserve bytes and defer the cut.
        assert!(feed(&mut planner, &mut info, &options, packet(256, true, &gop())).is_none());
        assert!(planner.request.is_some());
        feed(
            &mut planner,
            &mut info,
            &options,
            packet(257, true, &[0, 0, 1, 0xc0, 0, 9, 0x80, 0, 0]),
        );
        let mut rai = packet(
            256,
            true,
            &[0, 0, 1, 0xe0, 0, 0, 0x80, 0, 0, 0, 0, 1, 0, 0, 16],
        );
        rai[5] |= 64;
        assert!(feed(&mut planner, &mut info, &options, rai).is_none());
        let g = gop();
        assert!(
            feed(
                &mut planner,
                &mut info,
                &options,
                packet(256, true, &g[..25])
            )
            .is_none()
        );
        assert!(planner.holding());
        let cut = feed(
            &mut planner,
            &mut info,
            &options,
            packet(256, false, &g[25..]),
        )
        .unwrap();
        assert_eq!(cut.at, 4);
        assert_eq!(cut.overlap_start, 1);
        assert_eq!(cut.reason, "manual");
        assert!(!planner.holding());
    }
    #[test]
    fn cache_exhaustion_and_receive_errors_defer_cuts_without_unbounded_memory() {
        let options = CaptureOptions {
            max_frames: 0,
            ..Default::default()
        };
        let mut info = Inspector::default();
        info.video_pid = Some(256);
        info.tables = vec![packet(0, true, &[0])];
        let mut planner = Planner::default();
        feed(&mut planner, &mut info, &options, packet(256, true, &gop()));
        planner.request = Some("manual");
        for _ in 0..CACHE_PACKETS + 10 {
            feed(&mut planner, &mut info, &options, packet(8191, false, &[0]));
        }
        assert_eq!(planner.cache.len(), CACHE_PACKETS);
        assert!(feed(&mut planner, &mut info, &options, packet(256, true, &gop())).is_none());
        info.errors += 1;
        assert!(feed(&mut planner, &mut info, &options, packet(256, true, &gop())).is_none());
        assert!(planner.request.is_some());
    }
    #[test]
    fn repeated_transport_packets_do_not_shorten_an_audio_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tail.m2t");
        let mut writer = Writer::new(CaptureOptions::default());
        writer.tail = Some(Tail {
            file: BufWriter::new(File::create(&path).unwrap()),
            path: path.clone(),
            remaining: vec![AudioTail {
                id: 257,
                remaining: Some(10),
                cc: Some(0),
            }],
            packets: 0,
        });
        let mut first = packet(257, false, &[1, 2, 3, 4]);
        // Same counter as the final pre-cut packet: this is a duplicate.
        writer.feed_tail(&first).unwrap();
        assert_eq!(
            writer.tail.as_ref().unwrap().remaining[0].remaining,
            Some(10)
        );
        first[3] = (first[3] & 0xf0) | 1;
        writer.feed_tail(&first).unwrap();
        writer.feed_tail(&first).unwrap();
        assert_eq!(
            writer.tail.as_ref().unwrap().remaining[0].remaining,
            Some(6)
        );
        let mut last = packet(257, false, &[5, 6, 7, 8, 9, 10]);
        last[3] = (last[3] & 0xf0) | 2;
        writer.feed_tail(&last).unwrap();
        assert!(writer.tail.is_none());
        assert_eq!(std::fs::read(path).unwrap(), [first, last].concat());
    }
}

//! HDV's MPEG-2 transport stream. Capture never decodes or rewrites its 188-byte packets.
use anyhow::{Context, Result, ensure};
use std::{fs::File, io::Read, path::Path};

pub const PACKET: usize = 188;
pub const BLOCK: usize = PACKET * 256;
const PCR_WRAP: u64 = (1_u64 << 33) * 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    pub numerator: u32,
    pub denominator: u32,
}
impl Rate {
    pub fn new(mut numerator: u32, mut denominator: u32) -> Self {
        let (mut a, mut b) = (numerator, denominator);
        while b != 0 {
            (a, b) = (b, a % b);
        }
        numerator /= a;
        denominator /= a;
        Self {
            numerator,
            denominator,
        }
    }
    pub fn value(self) -> f64 {
        f64::from(self.numerator) / f64::from(self.denominator)
    }
    fn code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::new(24000, 1001),
            2 => Self::new(24, 1),
            3 => Self::new(25, 1),
            4 => Self::new(30000, 1001),
            5 => Self::new(30, 1),
            6 => Self::new(50, 1),
            7 => Self::new(60000, 1001),
            8 => Self::new(60, 1),
            _ => return None,
        })
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub rate: Rate,
    pub progressive: bool,
}
impl VideoInfo {
    pub fn is_hdv(&self) -> bool {
        (self.width == 1280 && self.height == 720 && self.progressive)
            || (self.width == 1440 && self.height == 1080)
    }
    pub fn description(&self) -> String {
        format!(
            "HDV{} {}×{}{} {:.3} fps ({}/{})",
            if self.height == 720 { "1" } else { "2" },
            self.width,
            self.height,
            if self.progressive { "p" } else { "i" },
            self.rate.value(),
            self.rate.numerator,
            self.rate.denominator
        )
    }
}

/// Normalizes arbitrary callback boundaries and 192-byte source-packet headers.
/// Only the FireWire/M2TS prefix is removed; all MPEG TS bytes are preserved.
#[derive(Default)]
pub struct Packetizer {
    pending: Vec<u8>,
    layout: Option<(usize, usize)>,
    pub discarded: u64,
}
impl Packetizer {
    pub fn pending_bytes(&self) -> &[u8] {
        &self.pending
    }
    pub fn push(&mut self, bytes: &[u8]) -> Vec<[u8; PACKET]> {
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut consumed = 0;
        loop {
            let left = &self.pending[consumed..];
            if self.layout.is_none() {
                if left.len() < 192 * 3 {
                    break;
                }
                self.layout = [(188, 0), (192, 4)]
                    .into_iter()
                    .find(|&(stride, offset)| (0..3).all(|n| left[n * stride + offset] == 0x47));
                if self.layout.is_none() {
                    consumed += 1;
                    self.discarded += 1;
                    continue;
                }
            }
            let (stride, offset) = self.layout.unwrap();
            if left.len() < stride {
                break;
            }
            if left[offset] != 0x47 {
                self.layout = None;
                continue;
            }
            out.push(left[offset..offset + PACKET].try_into().unwrap());
            consumed += stride;
        }
        self.pending.drain(..consumed);
        out
    }
    pub fn finish(&mut self) -> Result<Vec<[u8; PACKET]>> {
        if self.layout.is_none() {
            self.layout = [(188, 0), (192, 4)].into_iter().find(|&(stride, offset)| {
                !self.pending.is_empty()
                    && self.pending.len().is_multiple_of(stride)
                    && self.pending.chunks_exact(stride).all(|p| p[offset] == 0x47)
            });
        }
        let packets = self.push(&[]);
        ensure!(
            self.pending.is_empty(),
            "Truncated or unsynchronized HDV transport packet ({} trailing bytes)",
            self.pending.len()
        );
        Ok(packets)
    }
}
pub struct Reader {
    file: File,
    packetizer: Packetizer,
    eof: bool,
}
impl Reader {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            file: File::open(path).with_context(|| format!("Cannot open {}", path.display()))?,
            packetizer: Packetizer::default(),
            eof: false,
        })
    }
    pub fn next_packets(&mut self) -> Result<Option<Vec<[u8; PACKET]>>> {
        if self.eof {
            return Ok(None);
        }
        loop {
            let mut bytes = [0; BLOCK];
            let n = self.file.read(&mut bytes)?;
            let packets = if n == 0 {
                self.eof = true;
                self.packetizer.finish()?
            } else {
                self.packetizer.push(&bytes[..n])
            };
            ensure!(
                self.packetizer.discarded == 0,
                "HDV file contains unsynchronized transport bytes"
            );
            if !packets.is_empty() {
                return Ok(Some(packets));
            }
            if self.eof {
                return Ok(None);
            }
        }
    }
}
pub fn is_hdv_file(path: &Path) -> bool {
    let Ok(mut f) = File::open(path) else {
        return false;
    };
    let mut bytes = [0; 192 * 3];
    let Ok(n) = f.read(&mut bytes) else {
        return false;
    };
    [(188, 0), (192, 4)].into_iter().any(|(stride, offset)| {
        n >= stride * 3 && (0..3).all(|i| bytes[i * stride + offset] == 0x47)
    })
}

#[derive(Default)]
struct Section {
    bytes: Vec<u8>,
}
impl Section {
    fn push(&mut self, payload: &[u8], start: bool) -> Vec<Vec<u8>> {
        let mut result = Vec::new();
        let mut payload = payload;
        if start {
            if payload.is_empty() {
                return result;
            }
            let pointer = payload[0] as usize;
            if payload.len() < pointer + 1 {
                self.bytes.clear();
                return result;
            }
            if !self.bytes.is_empty() {
                self.bytes.extend_from_slice(&payload[1..1 + pointer]);
                self.extract(&mut result);
            }
            self.bytes.clear();
            payload = &payload[1 + pointer..];
        }
        self.bytes.extend_from_slice(payload);
        self.extract(&mut result);
        result
    }
    fn extract(&mut self, result: &mut Vec<Vec<u8>>) {
        while self.bytes.len() >= 3 {
            if self.bytes[0] == 0xff {
                self.bytes.clear();
                break;
            }
            let size = 3 + (((self.bytes[1] as usize & 15) << 8) | self.bytes[2] as usize);
            if size > 4096 {
                self.bytes.clear();
                break;
            }
            if self.bytes.len() < size {
                break;
            }
            let section: Vec<_> = self.bytes.drain(..size).collect();
            if crc32(&section) == 0 {
                result.push(section);
            }
        }
    }
}
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffff_u32;
    for &b in bytes {
        crc ^= u32::from(b) << 24;
        for _ in 0..8 {
            crc = (crc << 1)
                ^ if crc & 0x8000_0000 != 0 {
                    0x04c1_1db7
                } else {
                    0
                };
        }
    }
    crc
}
fn hdv_registration(mut descriptors: &[u8]) -> bool {
    while descriptors.len() >= 2 {
        let length = usize::from(descriptors[1]);
        let Some(data) = descriptors.get(2..2 + length) else {
            return false;
        };
        if descriptors[0] == 5
            && data
                .get(..4)
                .is_some_and(|id| id == b"TSHV" || id == b"TSMV")
        {
            return true;
        }
        descriptors = &descriptors[2 + length..];
    }
    false
}
pub fn pid(packet: &[u8; PACKET]) -> u16 {
    (u16::from(packet[1] & 31) << 8) | u16::from(packet[2])
}
pub fn pcr(packet: &[u8; PACKET]) -> Option<u64> {
    if packet[3] & 0x20 == 0 || packet[4] < 7 || packet[4] > 183 || packet[5] & 0x10 == 0 {
        return None;
    }
    let p = &packet[6..12];
    let base = (u64::from(p[0]) << 25)
        | (u64::from(p[1]) << 17)
        | (u64::from(p[2]) << 9)
        | (u64::from(p[3]) << 1)
        | u64::from(p[4] >> 7);
    Some(base * 300 + (u64::from(p[4] & 1) << 8) + u64::from(p[5]))
}
pub fn random_access(packet: &[u8; PACKET]) -> bool {
    if packet[1] & 0x40 == 0 {
        return false;
    }
    if packet[3] & 0x20 != 0 && packet[4] > 0 && packet[5] & 0x40 != 0 {
        return true;
    }
    let offset = if packet[3] & 0x20 != 0 {
        5 + packet[4] as usize
    } else {
        4
    };
    // Some cameras omit RAI but repeat the sequence header at a video PES/GOP.
    offset < PACKET && packet[offset..].windows(4).any(|p| p == [0, 0, 1, 0xb3])
}

/// Sony/Canon HDV AUX (PMT stream type A1), with the original PES header included.
/// Unknown metadata layouts stay in the capture and never trigger scene cuts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingMetadata {
    pub recorded: Option<chrono::NaiveDateTime>,
    pub timecode: Option<String>,
    pub scene_start: bool,
}
impl RecordingMetadata {
    fn sony_aux(data: &[u8]) -> Option<Self> {
        if data.len() < 50 || data[..4] != [0, 0, 1, 0xbf] || data[6..8] != [0x44, 0x39] {
            return None;
        }
        fn bcd(b: u8) -> Option<u32> {
            (b >> 4 <= 9 && b & 15 <= 9).then_some(u32::from(b >> 4) * 10 + u32::from(b & 15))
        }
        let recorded = (|| {
            chrono::NaiveDate::from_ymd_opt(
                2000 + bcd(data[44])? as i32,
                bcd(data[43] & 31)?,
                bcd(data[42] & 63)?,
            )?
            .and_hms_opt(
                bcd(data[48] & 63)?,
                bcd(data[47] & 127)?,
                bcd(data[46] & 127)?,
            )
        })();
        let timecode = (|| {
            let (h, m, s, f) = (
                bcd(data[40] & 63)?,
                bcd(data[39] & 127)?,
                bcd(data[38] & 127)?,
                bcd(data[37] & 63)?,
            );
            (h < 24 && m < 60 && s < 60 && f < 60).then(|| format!("{h:02}:{m:02}:{s:02}:{f:02}"))
        })();
        (recorded.is_some() || timecode.is_some()).then_some(Self {
            recorded,
            timecode,
            scene_start: data[49] & 0x20 == 0,
        })
    }
}

pub struct Inspector {
    pub video: Option<VideoInfo>,
    pub packets: u64,
    pub pictures: u64,
    pub errors: u64,
    pub seconds: f64,
    pub bitrate: u32,
    pub pmt_pid: Option<u16>,
    pub video_pid: Option<u16>,
    pub pcr_pid: Option<u16>,
    pub audio_pids: Vec<u16>,
    pub metadata: Option<RecordingMetadata>,
    pub metadata_serial: u64,
    tape_registration: bool,
    aux_pid: Option<u16>,
    aux: Vec<u8>,
    pat: Section,
    pmt: Section,
    continuity: Vec<Option<u8>>,
    elementary: Vec<u8>,
    pes_header: Option<Vec<u8>>,
    base_rate: Option<Rate>,
    pending_sequence: Option<VideoInfo>,
    pub format_changes: u64,
    last_clock: Option<(u64, u64)>,
    pub tables: Vec<[u8; PACKET]>,
    first_counters: Vec<(u16, u8)>,
    first_clock: Option<u64>,
    pat_packets: Vec<[u8; PACKET]>,
    pmt_packets: Vec<[u8; PACKET]>,
}
impl Default for Inspector {
    fn default() -> Self {
        Self {
            video: None,
            packets: 0,
            pictures: 0,
            errors: 0,
            seconds: 0.,
            bitrate: 0,
            pmt_pid: None,
            video_pid: None,
            pcr_pid: None,
            audio_pids: Vec::new(),
            metadata: None,
            metadata_serial: 0,
            tape_registration: false,
            aux_pid: None,
            aux: Vec::new(),
            pat: Section::default(),
            pmt: Section::default(),
            continuity: vec![None; 8192],
            elementary: Vec::new(),
            pes_header: None,
            base_rate: None,
            pending_sequence: None,
            format_changes: 0,
            last_clock: None,
            tables: Vec::new(),
            first_counters: Vec::new(),
            first_clock: None,
            pat_packets: Vec::new(),
            pmt_packets: Vec::new(),
        }
    }
}
impl Inspector {
    pub fn push(&mut self, packet: &[u8; PACKET]) {
        self.packets += 1;
        let id = pid(packet);
        let start = packet[1] & 0x40 != 0;
        let afc = (packet[3] >> 4) & 3;
        if packet[0] != 0x47 || packet[1] & 0x80 != 0 || afc == 0 || packet[3] & 0xc0 != 0 {
            self.errors += 1;
            return;
        }
        let discontinuity = afc & 2 != 0 && packet[4] > 0 && packet[5] & 0x80 != 0;
        if discontinuity {
            if Some(id) == self.aux_pid {
                self.aux.clear();
            }
            self.continuity[id as usize] = None;
            if id == 0 {
                self.pat.bytes.clear();
            }
            if Some(id) == self.pmt_pid {
                self.pmt.bytes.clear();
            }
            if Some(id) == self.video_pid {
                self.elementary.clear();
                self.pes_header = None;
            }
        }
        let offset = if afc & 2 != 0 {
            5 + packet[4] as usize
        } else {
            4
        };
        if offset > PACKET {
            self.errors += 1;
            return;
        }
        if Some(id) == self.pcr_pid
            && let Some(clock) = pcr(packet)
        {
            self.first_clock.get_or_insert(clock);
            if let Some((last, index)) = self.last_clock {
                let delta = (clock + PCR_WRAP - last) % PCR_WRAP;
                if !discontinuity && delta > 0 && delta < 27_000_000 * 10 {
                    self.seconds += delta as f64 / 27_000_000.;
                    self.bitrate = (((self.packets - index) * 188 * 8 * 27_000_000) / delta) as u32;
                }
            }
            self.last_clock = Some((clock, self.packets));
        }
        if afc & 1 == 0 {
            return;
        }
        let cc = packet[3] & 15;
        if id != 8191 && !self.first_counters.iter().any(|(pid, _)| *pid == id) {
            self.first_counters.push((id, cc));
        }
        let previous = self.continuity[id as usize].replace(cc);
        if !discontinuity
            && id != 8191
            && let Some(previous) = previous
        {
            if cc == previous {
                return;
            } // duplicate TS packet
            if cc != (previous + 1) & 15 {
                self.errors += 1;
                if Some(id) == self.aux_pid {
                    self.aux.clear();
                }
                if Some(id) == self.video_pid {
                    self.elementary.clear();
                    self.pes_header = None;
                }
            }
        }
        let payload = &packet[offset..];
        if id == 0 {
            if start {
                self.pat_packets.clear();
            }
            if self.pat_packets.len() < 24 {
                self.pat_packets.push(*packet);
            }
            for s in self.pat.push(payload, start) {
                if s.len() < 12 || s[0] != 0 || s[5] & 1 == 0 {
                    continue;
                }
                for entry in s[8..s.len() - 4].chunks_exact(4) {
                    if entry[0] != 0 || entry[1] != 0 {
                        self.pmt_pid = Some((u16::from(entry[2] & 31) << 8) | u16::from(entry[3]));
                        break;
                    }
                }
            }
        } else if Some(id) == self.pmt_pid {
            if start {
                self.pmt_packets.clear();
            }
            if self.pmt_packets.len() < 24 {
                self.pmt_packets.push(*packet);
            }
            for s in self.pmt.push(payload, start) {
                if s.len() < 16 || s[0] != 2 || s[5] & 1 == 0 {
                    continue;
                }
                self.pcr_pid = Some((u16::from(s[8] & 31) << 8) | u16::from(s[9]));
                let mut audio_pids = Vec::new();
                let mut aux_pid = None;
                let mut at = 12 + (((s[10] as usize & 15) << 8) | s[11] as usize);
                if at > s.len() - 4 {
                    continue;
                }
                self.tape_registration = hdv_registration(&s[12..at]);
                while at + 5 <= s.len() - 4 {
                    let elementary_pid = (u16::from(s[at + 1] & 31) << 8) | u16::from(s[at + 2]);
                    if s[at] == 2 {
                        self.video_pid = Some(elementary_pid);
                    }
                    if matches!(s[at], 3 | 4 | 6 | 0x80 | 0x81) {
                        audio_pids.push(elementary_pid);
                    }
                    if s[at] == 0xa1 {
                        aux_pid = Some(elementary_pid);
                    }
                    at += 5 + (((s[at + 3] as usize & 15) << 8) | s[at + 4] as usize);
                }
                self.audio_pids = audio_pids;
                if self.aux_pid != aux_pid {
                    self.aux.clear();
                    self.metadata = None;
                    self.aux_pid = aux_pid;
                }
                self.tables = [self.pat_packets.clone(), self.pmt_packets.clone()].concat();
            }
        } else if Some(id) == self.video_pid {
            self.video_payload(payload, start);
        } else if Some(id) == self.aux_pid {
            if start {
                self.aux.clear();
            }
            if (start || !self.aux.is_empty()) && self.aux.len() + payload.len() <= 1024 {
                self.aux.extend_from_slice(payload);
                if self.aux.len() >= 6 {
                    let length = 6 + usize::from(u16::from_be_bytes([self.aux[4], self.aux[5]]));
                    if self.aux.len() >= length {
                        if let Some(metadata) = RecordingMetadata::sony_aux(&self.aux[..length]) {
                            self.metadata = Some(metadata);
                            self.metadata_serial += 1;
                        }
                        self.aux.clear();
                    }
                }
            } else {
                self.aux.clear();
            }
        }
    }
    fn video_payload(&mut self, payload: &[u8], start: bool) {
        if start {
            self.pes_header = Some(Vec::new());
        }
        if let Some(header) = &mut self.pes_header {
            header.extend_from_slice(payload);
            if header.len() < 9 {
                return;
            }
            if header[..3] != [0, 0, 1] {
                self.pes_header = None;
                return;
            }
            let skip = 9 + header[8] as usize;
            if header.len() < skip {
                return;
            }
            self.elementary.extend_from_slice(&header[skip..]);
            self.pes_header = None;
        } else {
            self.elementary.extend_from_slice(payload);
        }
        let mut at = 0;
        while at + 4 <= self.elementary.len() {
            if self.elementary[at..at + 3] != [0, 0, 1] {
                at += 1;
                continue;
            }
            let code = self.elementary[at + 3];
            let needed = match code {
                0xb3 => 8,
                0xb5 => 10,
                _ => 4,
            };
            if at + needed > self.elementary.len() {
                break;
            }
            let b = &self.elementary[at + 4..];
            match code {
                0 => self.pictures += 1,
                0xb3 => {
                    if let Some(rate) = Rate::code(b[3] & 15) {
                        self.base_rate = Some(rate);
                        self.pending_sequence = Some(VideoInfo {
                            width: (u32::from(b[0]) << 4) | u32::from(b[1] >> 4),
                            height: (u32::from(b[1] & 15) << 8) | u32::from(b[2]),
                            rate,
                            progressive: false,
                        });
                    }
                }
                0xb5 if b[0] >> 4 == 1 => {
                    if let (Some(mut video), Some(base)) =
                        (self.pending_sequence.take(), self.base_rate)
                    {
                        video.progressive = b[1] & 8 != 0;
                        video.width =
                            (video.width & 4095) | (u32::from((b[1] & 1) << 1 | b[2] >> 7) << 12);
                        video.height = (video.height & 4095) | (u32::from((b[2] >> 5) & 3) << 12);
                        video.rate = Rate::new(
                            base.numerator * (u32::from((b[5] >> 5) & 3) + 1),
                            base.denominator * (u32::from(b[5] & 31) + 1),
                        );
                        if self.video.as_ref().is_some_and(|old| old != &video) {
                            self.format_changes += 1;
                        }
                        self.video = Some(video);
                    }
                }
                _ => {}
            }
            at += needed;
        }
        self.elementary.drain(..at);
    }
    pub fn validate(&self) -> Result<&VideoInfo> {
        let video = self
            .video
            .as_ref()
            .context("No MPEG-2 video sequence in HDV transport stream")?;
        ensure!(
            video.is_hdv(),
            "Not an HDV 720p/1080 stream: {}",
            video.description()
        );
        ensure!(
            self.pmt_pid.is_some() && self.video_pid.is_some() && self.pcr_pid.is_some(),
            "HDV stream lacks PAT/PMT/PCR mapping"
        );
        Ok(video)
    }
    /// Tape output needs HDV signalling as well as a decodable MPEG-2 picture.
    /// These checks reject generic MPEG-TS encodes; they do not certify complete
    /// HDV conformance or a particular deck's recording capabilities.
    pub fn validate_tape_source(&self) -> Result<()> {
        let video = self.validate()?;
        ensure!(
            self.tape_registration,
            "This MPEG transport stream lacks HDV program signalling (TSHV/TSMV). Use an original HDV capture or an HDV tape-authoring encoder. Matching resolution and frame rate in FFmpeg is not sufficient for tape recording."
        );
        ensure!(
            video.height != 1080 || self.aux_pid.is_some(),
            "This 1080-line stream lacks the HDV video AUX stream. Adding an HDV identifier alone does not prepare a generic MPEG file for tape. Use an original HDV capture or an HDV tape-authoring encoder."
        );
        Ok(())
    }
    /// A tape playlist may restart PCR/PTS and continuity counters between clips.
    /// Signal this using adaptation-only TS packets, without touching any source
    /// video/audio packets. File export deliberately omits these additions.
    pub fn discontinuity_packets(&self) -> Vec<[u8; PACKET]> {
        self.first_counters
            .iter()
            .map(|&(id, cc)| {
                let mut p = [0xff; PACKET];
                p[0] = 0x47;
                p[1] = (id >> 8) as u8;
                p[2] = id as u8;
                p[3] = 0x20 | cc.wrapping_sub(1) & 15;
                p[4] = 183;
                p[5] = 0x80;
                if Some(id) == self.pcr_pid
                    && let Some(clock) = self.first_clock
                {
                    let base = clock / 300;
                    let ext = clock % 300;
                    p[5] |= 0x10;
                    p[6] = (base >> 25) as u8;
                    p[7] = (base >> 17) as u8;
                    p[8] = (base >> 9) as u8;
                    p[9] = (base >> 1) as u8;
                    p[10] = ((base & 1) << 7) as u8 | 0x7e | (ext >> 8) as u8;
                    p[11] = ext as u8;
                }
                p
            })
            .collect()
    }
}
pub fn inspect_file(path: &Path) -> Result<Inspector> {
    let mut input = Reader::open(path)?;
    let mut inspector = Inspector::default();
    while let Some(packets) = input.next_packets()? {
        for packet in packets {
            inspector.push(&packet);
        }
    }
    inspector.validate()?;
    ensure!(
        inspector.errors == 0,
        "HDV stream contains {} transport errors; repair it before tape output",
        inspector.errors
    );
    ensure!(
        inspector.bitrate > 0 && inspector.seconds > 0.,
        "HDV tape output needs at least two PCR timestamps"
    );
    Ok(inspector)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tape_preflight_requires_hdv_signalling_without_restricting_file_workflows() {
        let mut inspector = Inspector {
            video: Some(VideoInfo {
                width: 1440,
                height: 1080,
                rate: Rate::new(25, 1),
                progressive: false,
            }),
            pmt_pid: Some(0x81),
            video_pid: Some(0x810),
            pcr_pid: Some(0x134),
            ..Default::default()
        };
        assert!(inspector.validate().is_ok());
        assert!(
            inspector
                .validate_tape_source()
                .unwrap_err()
                .to_string()
                .contains("TSHV/TSMV")
        );
        // A real Sony PMT, including its registration and both AUX declarations.
        let hex = "02b02e0064c10000e134f00d050b54534856ff1080ffffffff02e810f00003e814f000a0e815f000a1e811f0004c856be8";
        let section: Vec<_> = hex
            .as_bytes()
            .chunks_exact(2)
            .map(|b| u8::from_str_radix(std::str::from_utf8(b).unwrap(), 16).unwrap())
            .collect();
        assert_eq!(crc32(&section), 0);
        let mut p = [0xff; PACKET];
        p[..5].copy_from_slice(&[0x47, 0x40, 0x81, 0x10, 0]);
        p[5..5 + section.len()].copy_from_slice(&section);
        inspector.push(&p);
        assert!(inspector.validate_tape_source().is_ok());
        inspector.aux_pid = None;
        assert!(
            inspector
                .validate_tape_source()
                .unwrap_err()
                .to_string()
                .contains("AUX")
        );
        // HDV1 uses its registration but does not use HDV2's AUX-V layout.
        inspector.video = Some(VideoInfo {
            width: 1280,
            height: 720,
            rate: Rate::new(60000, 1001),
            progressive: true,
        });
        assert!(inspector.validate_tape_source().is_ok());
        assert!(hdv_registration(b"\x05\x04TSMV"));
        assert!(!hdv_registration(b"\x05\x04HDMV"));
        assert!(!hdv_registration(b"\x05\x08TSHV"));
    }
    #[test]
    fn sequence_extension_preserves_exact_rational_rates() {
        // MPEG-2 sequence and sequence-extension headers, deliberately split
        // across PES payload boundaries to cover streaming header assembly.
        let mut inspector = Inspector::default();
        let header = [
            0, 0, 1, 0xe0, 0, 0, 0x80, 0, 0, 0, 0, 1, 0xb3, 0x50, 0x02, 0xd0, 0x14,
        ];
        inspector.video_payload(&header, true);
        assert!(inspector.video.is_none());
        inspector.video_payload(&[0, 0, 1, 0xb5, 0x14, 0x8a], false);
        inspector.video_payload(&[0, 1, 0, 3 << 5 | 4], false);
        let video = inspector.video.unwrap();
        assert_eq!(video.rate, Rate::new(24000, 1001));
        assert!(video.progressive);
        assert_eq!((video.width, video.height), (1280, 720));
    }
    fn clock_packet(clock: u64, discontinuity: bool) -> [u8; PACKET] {
        let mut p = [0xff; PACKET];
        p[..6].copy_from_slice(&[
            0x47,
            1,
            0,
            0x20,
            183,
            if discontinuity { 0x90 } else { 0x10 },
        ]);
        let base = clock / 300;
        let ext = clock % 300;
        p[6] = (base >> 25) as u8;
        p[7] = (base >> 17) as u8;
        p[8] = (base >> 9) as u8;
        p[9] = (base >> 1) as u8;
        p[10] = ((base & 1) << 7) as u8 | 0x7e | (ext >> 8) as u8;
        p[11] = ext as u8;
        p
    }
    #[test]
    fn pcr_wrap_and_discontinuities_do_not_change_duration() {
        let mut inspector = Inspector {
            pcr_pid: Some(256),
            ..Default::default()
        };
        inspector.push(&clock_packet(PCR_WRAP - 27_000_000, false));
        inspector.push(&clock_packet(13_500_000, false));
        assert!((inspector.seconds - 1.5).abs() < 1e-9);
        inspector.push(&clock_packet(0, true));
        assert!((inspector.seconds - 1.5).abs() < 1e-9);
        inspector.push(&clock_packet(27_000_000, false));
        assert!((inspector.seconds - 2.5).abs() < 1e-9);
    }
}

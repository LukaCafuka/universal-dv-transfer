use anyhow::{Result, bail, ensure};
use chrono::{NaiveDate, NaiveDateTime};
use std::io::{ErrorKind, Read};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standard {
    Pal,
    Ntsc,
}
impl Standard {
    pub fn frame_size(self) -> usize {
        match self {
            Self::Pal => 144_000,
            Self::Ntsc => 120_000,
        }
    }
    pub fn rate(self) -> (u32, u32) {
        match self {
            Self::Pal => (25, 1),
            Self::Ntsc => (30_000, 1001),
        }
    }
    pub fn height(self) -> u32 {
        if self == Self::Pal { 576 } else { 480 }
    }
    pub fn seconds(self, frames: u64) -> f64 {
        let (n, d) = self.rate();
        frames as f64 * d as f64 / n as f64
    }
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub data: Vec<u8>,
    pub standard: Standard,
    pub recorded: Option<NaiveDateTime>,
    pub timecode: Option<String>,
    pub field_order: Option<FieldOrder>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldOrder {
    Progressive,
    BottomFirst,
    TopFirst,
}
impl Frame {
    pub fn parse(data: Vec<u8>) -> Result<Self> {
        ensure!(
            data.len() >= 4 && data[0] & 0xe0 == 0 && data[1] & 0xf0 == 0 && data[2] == 0,
            "Not a DV25 DIF frame (HDV requires the transport-stream capture path)"
        );
        let standard = if data[3] & 0x80 != 0 {
            Standard::Pal
        } else {
            Standard::Ntsc
        };
        ensure!(
            data.len() == standard.frame_size(),
            "Incomplete DV frame: {} bytes",
            data.len()
        );
        // Validate each sequence header, without rejecting corrupt picture/audio blocks.
        for seq in 0..data.len() / 12_000 {
            let p = seq * 12_000;
            ensure!(
                data[p] & 0xe0 == 0 && data[p + 2] == 0 && data[p + 1] >> 4 == seq as u8,
                "Invalid DIF sequence header at {p}"
            );
        }
        let recorded = recording_time(&data);
        let field_order = field_order(&data);
        let timecode = packs(&data).find(|p| p[0] == 0x13).and_then(|p| {
            let f = bcd(p[1], 3)?;
            let s = bcd(p[2], 7)?;
            let m = bcd(p[3], 7)?;
            let h = bcd(p[4], 3)?;
            if h > 23 || m > 59 || s > 59 || f >= if standard == Standard::Pal { 25 } else { 30 } {
                return None;
            }
            Some(format!(
                "{h:02}:{m:02}:{s:02}{}{f:02}",
                if p[1] & 0x40 != 0 { ';' } else { ':' }
            ))
        });
        Ok(Self {
            data,
            standard,
            recorded,
            timecode,
            field_order,
        })
    }
}

/// DV25 VAUX source-control pack (0x61), PC3: IL is bit 4, FS is bit 6.
/// Use the majority of the redundant copies so one damaged pack cannot change
/// the AVI header. Missing or contradictory metadata remains unknown.
fn field_order(data: &[u8]) -> Option<FieldOrder> {
    let mut votes = [0; 3];
    for (seq, sequence) in data.chunks_exact(12_000).enumerate() {
        for block in 0..3 {
            let dif = &sequence[(3 + block) * 80..(4 + block) * 80];
            if dif[0] & 0xe0 != 0x40 || dif[1] >> 4 != seq as u8 || dif[2] != block as u8 {
                continue;
            }
            for pack in dif[3..78].chunks_exact(5).filter(|p| p[0] == 0x61) {
                let index = if pack[3] & 0x10 == 0 {
                    0
                } else if pack[3] & 0x40 != 0 {
                    1
                } else {
                    2
                };
                votes[index] += 1;
            }
        }
    }
    let total: usize = votes.iter().sum();
    [
        FieldOrder::Progressive,
        FieldOrder::BottomFirst,
        FieldOrder::TopFirst,
    ]
    .into_iter()
    .zip(votes)
    .find_map(|(order, count)| (count > total / 2).then_some(order))
}

fn bcd(v: u8, mask: u8) -> Option<u32> {
    let lo = v & 15;
    let hi = (v >> 4) & mask;
    (lo < 10 && hi < 10).then_some(u32::from(hi) * 10 + u32::from(lo))
}
fn packs(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    (0..data.len() / 12_000).flat_map(move |s| {
        (1..=2)
            .flat_map(move |b| (0..6).map(move |p| &data[s * 12_000 + b * 80 + 6 + p * 8..][..5]))
    })
}
pub fn recording_time(data: &[u8]) -> Option<NaiveDateTime> {
    let d = packs(data).find(|p| p[0] == 0x62)?;
    let t = packs(data).find(|p| p[0] == 0x63)?;
    let y = bcd(d[4], 15)? as i32;
    NaiveDate::from_ymd_opt(
        y + if y < 50 { 2000 } else { 1900 },
        bcd(d[3], 1)?,
        bcd(d[2], 3)?,
    )?
    .and_hms_opt(bcd(t[4], 3)?, bcd(t[3], 7)?, bcd(t[2], 7)?)
}

/// Raw DV is self-framing. Short reads are normal for pipes, a partial final frame is an error.
pub fn read_frame(reader: &mut impl Read) -> Result<Option<Frame>> {
    let mut header = [0; 80];
    match reader.read(&mut header[..1]) {
        Ok(0) => return Ok(None),
        Ok(_) => {}
        Err(e) => return Err(e.into()),
    }
    reader.read_exact(&mut header[1..])?;
    if header[0] & 0xe0 != 0 || header[2] != 0 {
        bail!("Input is not raw DV (or starts between frames)");
    }
    let standard = if header[3] & 0x80 != 0 {
        Standard::Pal
    } else {
        Standard::Ntsc
    };
    let mut data = vec![0; standard.frame_size()];
    data[..80].copy_from_slice(&header);
    if let Err(e) = reader.read_exact(&mut data[80..]) {
        if e.kind() == ErrorKind::UnexpectedEof {
            bail!("Truncated final DV frame");
        }
        return Err(e.into());
    }
    Frame::parse(data).map(Some)
}

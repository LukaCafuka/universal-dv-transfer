//! An indexed Type 1 DV AVI writer. OpenDML keeps large captures in one file.
use crate::dv::{Frame, Standard};
use anyhow::{Result, ensure};
use std::{
    fs::File,
    io::{BufWriter, Seek, SeekFrom, Write},
};

// Leave room for both indexes in the first section and keep all RIFF
// lengths and relative offsets within the range supported by AVI readers.
const RIFF_SECTION_BYTES: u64 = 1 << 30;
const SUPER_INDEX_CAPACITY: usize = 1024;
const DV_CHUNK: &[u8; 4] = b"00dc";

struct RiffSection {
    start: u64,
    movi: u64,
    frames: u32,
}

struct IndexEntry {
    offset: u64,
    size: u32,
    frames: u32,
}

pub struct Type1Writer {
    file: BufWriter<File>,
    standard: Standard,
    riff: RiffSection,
    section_limit: u64,
    frames: u32,
    avih_frames: u64,
    stream_length: u64,
    odml: u64,
    super_index: u64,
    indexes: Vec<IndexEntry>,
}

fn u32le(w: &mut impl Write, n: u32) -> std::io::Result<()> {
    w.write_all(&n.to_le_bytes())
}

fn chunk(w: &mut impl Write, id: &[u8; 4], data: &[u8]) -> std::io::Result<()> {
    w.write_all(id)?;
    u32le(w, data.len() as u32)?;
    w.write_all(data)?;
    if !data.len().is_multiple_of(2) {
        w.write_all(&[0])?;
    }
    Ok(())
}

fn start_list(w: &mut (impl Write + Seek), tag: &[u8; 4], form: &[u8; 4]) -> std::io::Result<u64> {
    let pos = w.stream_position()?;
    w.write_all(tag)?;
    u32le(w, 0)?;
    w.write_all(form)?;
    Ok(pos)
}

fn patch_u32(w: &mut (impl Write + Seek), pos: u64, value: u32) -> std::io::Result<()> {
    w.seek(SeekFrom::Start(pos))?;
    u32le(w, value)
}

fn patch_size(w: &mut (impl Write + Seek), start: u64, end: u64) -> Result<()> {
    patch_u32(w, start + 4, u32::try_from(end - start - 8)?)?;
    Ok(())
}

fn finish_list(w: &mut (impl Write + Seek), start: u64) -> Result<()> {
    let end = w.stream_position()?;
    patch_size(w, start, end)?;
    w.seek(SeekFrom::Start(end))?;
    Ok(())
}

impl Type1Writer {
    pub fn new(file: File, standard: Standard) -> Result<Self> {
        let mut w = BufWriter::new(file);
        let (n, d) = standard.rate();
        let start = start_list(&mut w, b"RIFF", b"AVI ")?;
        let hdrl = start_list(&mut w, b"LIST", b"hdrl")?;
        let mut avih = Vec::new();
        for v in [
            ((1_000_000u64 * d as u64) / n as u64) as u32,
            standard.frame_size() as u32 * n / d,
            0,
            0x10,
            0,
            0,
            1,
            standard.frame_size() as u32,
            720,
            standard.height(),
            0,
            0,
            0,
            0,
        ] {
            u32le(&mut avih, v)?;
        }
        let avih_frames = w.stream_position()? + 8 + 16;
        chunk(&mut w, b"avih", &avih)?;
        let strl = start_list(&mut w, b"LIST", b"strl")?;
        let mut strh = Vec::new();
        strh.extend_from_slice(b"iavsdvsd");
        for v in [
            0,
            0,
            0,
            d,
            n,
            0,
            0,
            standard.frame_size() as u32,
            u32::MAX,
            0,
            0,
            0,
        ] {
            u32le(&mut strh, v)?;
        }
        let stream_length = w.stream_position()? + 8 + 32;
        chunk(&mut w, b"strh", &strh)?;
        // DVINFO, as required by interleaved DV AVI (not a BITMAPINFOHEADER).
        let mut dvinfo = [0u8; 32];
        dvinfo[..8].fill(0xff);
        chunk(&mut w, b"strf", &dvinfo)?;

        // Reserve the OpenDML headers. Single-section files use the legacy
        // idx1 alone; the reserved chunks remain ordinary AVI padding.
        let super_index = w.stream_position()?;
        let mut index_header = vec![0; 24 + SUPER_INDEX_CAPACITY * 16];
        index_header[0] = 4; // 4 DWORDs per entry, AVI_INDEX_OF_INDEXES.
        index_header[8..12].copy_from_slice(DV_CHUNK);
        chunk(&mut w, b"JUNK", &index_header)?;
        finish_list(&mut w, strl)?;
        let odml = start_list(&mut w, b"JUNK", b"odml")?;
        chunk(&mut w, b"dmlh", &[0; 248])?;
        finish_list(&mut w, odml)?;
        finish_list(&mut w, hdrl)?;
        let movi = start_list(&mut w, b"LIST", b"movi")? + 8;
        Ok(Self {
            file: w,
            standard,
            riff: RiffSection {
                start,
                movi,
                frames: 0,
            },
            section_limit: RIFF_SECTION_BYTES,
            frames: 0,
            avih_frames,
            stream_length,
            odml,
            super_index,
            indexes: Vec::new(),
        })
    }

    pub fn write(&mut self, frame: &Frame) -> Result<()> {
        ensure!(
            frame.standard == self.standard,
            "PAL/NTSC changed inside AVI"
        );
        ensure!(
            frame.data.len() == self.standard.frame_size(),
            "Incomplete DV frame"
        );
        let next_frame = self
            .frames
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("AVI frame count limit reached"))?;
        let n = u64::from(self.riff.frames) + 1;
        let first = self.riff.start == 0;
        let legacy_index = if first { 8 + n * 16 } else { 0 };
        let section_bytes = self.riff.movi + 4 - self.riff.start
            + n * (8 + frame.data.len() as u64)
            + 32 + n * 8 // OpenDML standard index including its header.
            + legacy_index;
        if self.riff.frames != 0 && section_bytes > self.section_limit {
            ensure!(
                self.indexes.len() + 1 < SUPER_INDEX_CAPACITY,
                "AVI OpenDML index capacity reached"
            );
            let (_, index) = self.close_section(true)?;
            let start = start_list(&mut self.file, b"RIFF", b"AVIX")?;
            let movi = start_list(&mut self.file, b"LIST", b"movi")? + 8;
            self.indexes.extend(index);
            self.riff = RiffSection {
                start,
                movi,
                frames: 0,
            };
        }
        chunk(&mut self.file, DV_CHUNK, &frame.data)?;
        self.riff.frames += 1;
        self.frames = next_frame;
        Ok(())
    }

    fn close_section(&mut self, open_dml: bool) -> Result<(u64, Option<IndexEntry>)> {
        let RiffSection {
            start,
            movi,
            frames,
        } = self.riff;
        let size = self.standard.frame_size() as u32;
        let stride = size + 8;
        let ix_start = movi + 4 + u64::from(stride) * u64::from(frames);
        let w = &mut self.file;
        w.seek(SeekFrom::Start(ix_start))?;
        let index = if open_dml {
            w.write_all(b"ix00")?;
            let ix_size = 24 + frames * 8;
            u32le(w, ix_size)?;
            w.write_all(&[2, 0, 0, 1])?; // 2 DWORDs per entry, AVI_INDEX_OF_CHUNKS.
            u32le(w, frames)?;
            w.write_all(DV_CHUNK)?;
            w.write_all(&movi.to_le_bytes())?;
            u32le(w, 0)?;
            for n in 0..frames {
                u32le(w, 12 + n * stride)?; // OpenDML points to the frame payload.
                u32le(w, size)?; // DV frames are independently decodable.
            }
            Some(IndexEntry {
                offset: ix_start,
                size: ix_size + 8,
                frames,
            })
        } else {
            None
        };
        let end_movi = w.stream_position()?;
        if start == 0 {
            w.write_all(b"idx1")?;
            u32le(w, frames * 16)?;
            for n in 0..frames {
                w.write_all(DV_CHUNK)?;
                u32le(w, 0x10)?;
                u32le(w, 4 + n * stride)?; // idx1 points to the chunk header.
                u32le(w, size)?;
            }
        }
        let end = w.stream_position()?;
        patch_size(w, movi - 8, end_movi)?;
        patch_size(w, start, end)?;
        w.seek(SeekFrom::Start(end))?;
        Ok((end, index))
    }

    pub fn finish(mut self) -> Result<()> {
        let open_dml = !self.indexes.is_empty();
        let (end, index) = self.close_section(open_dml)?;
        self.indexes.extend(index);
        let first_frames = self
            .indexes
            .first()
            .map_or(self.frames, |index| index.frames);
        let w = &mut self.file;
        patch_u32(w, self.avih_frames, first_frames)?;
        patch_u32(w, self.stream_length, self.frames)?;
        if open_dml {
            w.seek(SeekFrom::Start(self.odml))?;
            w.write_all(b"LIST")?;
            patch_u32(w, self.odml + 20, self.frames)?; // dmlh's total across all RIFFs.
            w.seek(SeekFrom::Start(self.super_index))?;
            w.write_all(b"indx")?;
            patch_u32(w, self.super_index + 12, self.indexes.len() as u32)?;
            w.seek(SeekFrom::Start(self.super_index + 32))?;
            for entry in self.indexes {
                w.write_all(&entry.offset.to_le_bytes())?;
                u32le(w, entry.size)?;
                u32le(w, entry.frames)?;
            }
        }
        w.flush()?;
        // Keep only complete frames and indexes if a write stopped mid-frame.
        w.get_ref().set_len(end)?;
        w.get_ref().sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_dml_indexes_point_to_every_original_frame_across_sections() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let standard = Standard::Pal;
        let mut writer = Type1Writer::new(file.reopen().unwrap(), standard).unwrap();
        // Force rollover every two frames without generating gigabytes of test data.
        writer.section_limit = 350_000;
        let mut frame = Frame {
            data: vec![0; standard.frame_size()],
            standard,
            recorded: None,
            timecode: None,
            field_order: None,
        };
        for id in 1..=5 {
            frame.data.fill(id);
            writer.write(&frame).unwrap();
        }
        let super_index = writer.super_index as usize;
        let odml = writer.odml as usize;
        let stream_length = writer.stream_length as usize;
        writer.finish().unwrap();

        let mut data = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut data).unwrap();
        let read32 =
            |offset| u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        let read64 =
            |offset| u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap()) as usize;

        assert_eq!(read32(48), 2); // Legacy header describes the first RIFF.
        assert_eq!(read32(stream_length), 5);
        assert_eq!(&data[odml..odml + 4], b"LIST");
        assert_eq!(&data[odml + 12..odml + 16], b"dmlh");
        assert_eq!(read32(odml + 20), 5);
        assert_eq!(&data[super_index..super_index + 4], b"indx");
        assert_eq!(read32(super_index + 12), 3);

        let mut riff_start = 0;
        let mut frame_id = 1;
        for (section, frames) in [2, 2, 1].into_iter().enumerate() {
            let riff_end = riff_start + 8 + read32(riff_start + 4);
            assert_eq!(&data[riff_start..riff_start + 4], b"RIFF");
            assert_eq!(
                &data[riff_start + 8..riff_start + 12],
                if section == 0 { b"AVI " } else { b"AVIX" }
            );
            assert!(riff_end - riff_start <= 350_000);

            let entry = super_index + 32 + 16 * section;
            let ix = read64(entry);
            assert_eq!(read32(entry + 12), frames);
            assert_eq!(&data[ix..ix + 4], b"ix00");
            assert_eq!(read32(entry + 8), read32(ix + 4) + 8);
            assert_eq!(read32(ix + 12), frames);
            let base = read64(ix + 20);
            assert_eq!(&data[base..base + 4], b"movi");
            let movi_end = base + read32(base - 4);
            assert_eq!(ix + read32(entry + 8), movi_end);
            if section == 0 {
                assert_eq!(&data[movi_end..movi_end + 4], b"idx1");
                assert_eq!(read32(movi_end + 16), 4);
                assert_eq!(movi_end + 8 + read32(movi_end + 4), riff_end);
            } else {
                assert_eq!(movi_end, riff_end);
            }
            for i in 0..frames {
                let pos = base + read32(ix + 32 + i * 8);
                let len = read32(ix + 36 + i * 8);
                assert_eq!(len, standard.frame_size());
                assert_eq!(&data[pos - 8..pos - 4], b"00dc");
                assert_eq!(read32(pos - 4), len);
                assert_eq!(pos, base + 12 + i * (len + 8));
                assert!(data[pos..pos + len].iter().all(|&byte| byte == frame_id));
                frame_id += 1;
            }
            riff_start = riff_end;
        }
        assert_eq!(riff_start, data.len());
        assert_eq!(frame_id, 6);
    }
}

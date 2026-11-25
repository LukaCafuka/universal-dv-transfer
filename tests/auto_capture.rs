use std::{fs, path::Path};
use universal_dv_transfer::{
    auto_capture::CaptureArchive,
    config::{AviType, CaptureOptions, TransferFormat},
    dv::Standard,
    hdv, media,
};

fn dv(standard: Standard) -> Vec<u8> {
    let mut data = vec![0xff; standard.frame_size()];
    for seq in 0..data.len() / 12000 {
        let offset = seq * 12000;
        data[offset..offset + 4].copy_from_slice(&[
            0x1f,
            (seq as u8) << 4 | 7,
            0,
            if standard == Standard::Pal {
                0xbf
            } else {
                0x3f
            },
        ]);
    }
    data
}
fn options(path: &Path) -> CaptureOptions {
    CaptureOptions {
        format: TransferFormat::Auto,
        folder: path.into(),
        avi_type: AviType::Type1,
        date_format: String::new(),
        max_frames: 0,
        discontinuity_seconds: 0,
        preview: false,
        ..Default::default()
    }
}
fn tagged(kind: u8, bytes: &[u8]) -> Vec<u8> {
    let mut result = vec![kind];
    result.extend_from_slice(bytes);
    result
}
fn null_ts(count: usize) -> Vec<u8> {
    (0..count)
        .flat_map(|index| {
            let mut packet = [0xff; 188];
            packet[..4].copy_from_slice(&[0x47, 0x1f, 0xff, 0x10 | (index as u8 & 15)]);
            packet
        })
        .collect()
}
fn outputs(archive: &CaptureArchive, extension: &str) -> Vec<Vec<u8>> {
    archive
        .status()
        .files
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == extension))
        .map(|p| fs::read(p).unwrap())
        .collect()
}
fn events(archive: &CaptureArchive) -> Vec<serde_json::Value> {
    fs::read_to_string(archive.status().report.as_ref().unwrap())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn avi_payload(bytes: &[u8]) -> Vec<u8> {
    let offset = bytes.windows(4).position(|w| w == b"movi").unwrap() + 4;
    let mut at = offset;
    let mut data = Vec::new();
    while bytes.get(at..at + 4) == Some(b"00dc") {
        let len = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        data.extend_from_slice(&bytes[at + 8..at + 8 + len]);
        at += 8 + len + (len % 2);
    }
    data
}

#[test]
fn repeated_dv_hdv_switches_keep_boundary_packets_and_finalize_avis() {
    for stride in [188, 192] {
        let dir = tempfile::tempdir().unwrap();
        let mut archive = CaptureArchive::new(options(dir.path())).unwrap();
        let pal = dv(Standard::Pal);
        let ntsc = dv(Standard::Ntsc);
        let ts = null_ts(17);
        let bytes: Vec<u8> = ts
            .chunks_exact(188)
            .flat_map(|p| {
                let mut v = if stride == 192 {
                    vec![0, 0, 0, 1]
                } else {
                    Vec::new()
                };
                v.extend_from_slice(p);
                v
            })
            .collect();
        archive.push_tagged(&tagged(1, &pal)).unwrap();
        for chunk in bytes.chunks(13) {
            archive.push_tagged(&tagged(2, chunk)).unwrap();
        }
        archive.push_tagged(&tagged(1, &ntsc)).unwrap();
        archive.push_tagged(&tagged(2, &bytes)).unwrap();
        archive.push_tagged(&tagged(1, &pal)).unwrap();
        archive.finish().unwrap();
        assert_eq!(archive.status().switches, 4);
        assert_eq!(
            outputs(&archive, "dv"),
            vec![pal.clone(), ntsc.clone(), pal.clone()]
        );
        assert_eq!(outputs(&archive, "m2t"), vec![ts.clone(), ts]);
        archive.prepare_avis("").unwrap();
        let avis: Vec<_> = outputs(&archive, "avi")
            .iter()
            .map(|a| avi_payload(a))
            .collect();
        assert_eq!(avis, vec![pal.clone(), ntsc, pal]);
        assert!(
            events(&archive).iter().any(
                |e| e["event"] == "format_switch" && e["boundary_completeness"] == "unverified"
            )
        );
        assert!(
            events(&archive)
                .iter()
                .filter(|e| e["event"] == "capture_end")
                .all(|e| e["zero_loss_verified"] == false)
        );
        assert!(archive.push_tagged(&tagged(1, &dv(Standard::Pal))).is_err());
    }
}

fn cip_dv(archive: &mut CaptureArchive, bytes: &[u8], dbc: &mut u8) {
    for payload in bytes.chunks(480) {
        let mut packet = vec![3, 0, 120, 0, *dbc, 0, 0x80, 0xff, 0xff];
        packet.extend_from_slice(payload);
        archive.push_tagged(&packet).unwrap();
        *dbc = dbc.wrapping_add(1);
    }
}
fn cip_ts(archive: &mut CaptureArchive, bytes: &[u8], dbc: &mut u8) {
    for payload in bytes.chunks_exact(188) {
        let mut packet = vec![3, 0, 6, 0xc4, *dbc, 0xa0, 0x80, 0, 0, 0, 0, 0, 0];
        packet.extend_from_slice(payload);
        archive.push_tagged(&packet).unwrap();
        *dbc = dbc.wrapping_add(8);
    }
}
#[test]
fn universal_cip_receiver_preserves_both_dv_standards_and_all_hdv_packets() {
    let dir = tempfile::tempdir().unwrap();
    let mut archive = CaptureArchive::new(options(dir.path())).unwrap();
    let pal = dv(Standard::Pal);
    let ntsc = dv(Standard::Ntsc);
    let ts = null_ts(41);
    let mut dbc = 250;
    cip_dv(&mut archive, &pal, &mut dbc);
    dbc = 253;
    cip_ts(&mut archive, &ts, &mut dbc);
    dbc = 2;
    cip_dv(&mut archive, &ntsc, &mut dbc);
    archive.finish().unwrap();
    assert_eq!(outputs(&archive, "dv"), vec![pal, ntsc]);
    assert_eq!(outputs(&archive, "m2t"), vec![ts]);
    assert_eq!(archive.status().switches, 2);
    assert!(
        !events(&archive)
            .iter()
            .any(|e| e["event"] == "incomplete_data")
    );
}

#[test]
fn partial_frames_packets_and_driver_gaps_are_retained_and_reported() {
    let dir = tempfile::tempdir().unwrap();
    let mut archive = CaptureArchive::new(options(dir.path())).unwrap();
    let frame = dv(Standard::Pal);
    let mut dbc = 0;
    cip_dv(&mut archive, &frame[..960], &mut dbc);
    let ts = null_ts(4);
    archive
        .push_tagged(&tagged(2, &ts[..ts.len() - 7]))
        .unwrap();
    archive
        .interruption("Simulated driver re-enumeration")
        .unwrap();
    archive.push_tagged(&tagged(1, &frame)).unwrap();
    archive.finish().unwrap();
    assert_eq!(
        outputs(&archive, "dv"),
        vec![frame[..960].to_vec(), frame.clone()]
    ); // .partial.dv also has extension dv.
    assert_eq!(
        outputs(&archive, "ts"),
        vec![ts[188 * 3..ts.len() - 7].to_vec()]
    );
    assert_eq!(outputs(&archive, "m2t"), vec![ts[..188 * 3].to_vec()]);
    assert!(
        events(&archive)
            .iter()
            .any(|e| e["event"] == "interruption" && e["missing_frames"].is_null())
    );
    assert!(archive.status().issues >= 3);
    archive.prepare_avis("").unwrap(); // Partial DV is retained, never fed to an AVI writer.
    assert_eq!(outputs(&archive, "avi").len(), 1);
}

#[test]
fn cip_continuity_gaps_never_create_a_falsely_complete_dv_frame() {
    let dir = tempfile::tempdir().unwrap();
    let mut archive = CaptureArchive::new(options(dir.path())).unwrap();
    let frame = dv(Standard::Pal);
    let mut dbc = 0;
    cip_dv(&mut archive, &frame[..960], &mut dbc);
    dbc = dbc.wrapping_add(1);
    cip_dv(&mut archive, &frame[1440..], &mut dbc);
    cip_dv(&mut archive, &frame, &mut dbc);
    archive.finish().unwrap();
    let complete: Vec<_> = archive
        .status()
        .files
        .iter()
        .filter(|p| {
            p.extension().is_some_and(|e| e == "dv")
                && !p.to_string_lossy().ends_with(".partial.dv")
        })
        .map(|p| fs::read(p).unwrap())
        .collect();
    assert_eq!(complete, vec![frame]);
    assert!(events(&archive).iter().any(|e| {
        e["reason"]
            .as_str()
            .is_some_and(|s| s.contains("continuity gap"))
    }));
}

#[test]
fn failed_avi_preparation_keeps_original_dv_and_existing_files() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = options(dir.path());
    o.avi_type = AviType::Type2;
    fs::write(dir.path().join("capture.00.avi"), b"existing recording").unwrap();
    let mut archive = CaptureArchive::new(o).unwrap();
    let frame = dv(Standard::Pal);
    archive.push_tagged(&tagged(1, &frame)).unwrap();
    archive.finish().unwrap();
    assert!(archive.prepare_avis("a-nonexistent-ffmpeg-binary").is_err());
    assert_eq!(outputs(&archive, "dv"), vec![frame]);
    assert_eq!(
        fs::read(dir.path().join("capture.00.avi")).unwrap(),
        b"existing recording"
    );
    assert!(
        events(&archive)
            .iter()
            .any(|e| e["event"] == "avi_failed" && e["raw_dv_retained"] == true)
    );
}

#[test]
#[ignore = "requires FFmpeg installed"]
fn real_hdv1_hdv2_switches_keep_complete_streams_and_rates() {
    let dir = tempfile::tempdir().unwrap();
    let mut originals = Vec::new();
    for (name, size, rate, interlaced) in [
        ("720p", "1280x720", "60000/1001", false),
        ("1080i", "1440x1080", "25", true),
    ] {
        let path = dir.path().join(format!("{name}.ts"));
        let mut command = media::command("ffmpeg");
        command.args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc2=size={size}:rate={rate}"),
            "-t",
            "0.4",
            "-c:v",
            "mpeg2video",
            "-threads",
            "1",
            "-g",
            "6",
            "-bf",
            "2",
            "-b:v",
            "18000000",
            "-muxrate",
            "25000000",
        ]);
        if interlaced {
            command.args(["-flags", "+ildct+ilme", "-top", "1"]);
        }
        assert!(
            command
                .args(["-f", "mpegts"])
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        originals.push(fs::read(&path).unwrap());
    }
    let mut archive = CaptureArchive::new(options(&dir.path().join("captures"))).unwrap();
    let frame = dv(Standard::Pal);
    archive.push_tagged(&tagged(1, &frame)).unwrap();
    for bytes in &originals {
        let mut dbc = 0;
        cip_ts(&mut archive, bytes, &mut dbc);
        archive.push_tagged(&tagged(1, &frame)).unwrap();
    }
    archive.finish().unwrap();
    assert_eq!(archive.status().issues, 0);
    assert_eq!(archive.status().dropped, 0);
    assert_eq!(archive.status().switches, 4);
    assert_eq!(outputs(&archive, "m2t"), originals);
    let files: Vec<_> = archive
        .status()
        .files
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "m2t"))
        .collect();
    let first = hdv::inspect_file(files[0]).unwrap();
    let second = hdv::inspect_file(files[1]).unwrap();
    assert_eq!(first.validate().unwrap().rate, hdv::Rate::new(60000, 1001));
    assert!(first.validate().unwrap().progressive);
    assert_eq!(second.validate().unwrap().rate, hdv::Rate::new(25, 1));
    assert!(!second.validate().unwrap().progressive);
    archive.prepare_avis("").unwrap();
    assert_eq!(outputs(&archive, "avi").len(), 3);
}

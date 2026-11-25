use std::{collections::HashSet, path::Path};
use universal_dv_transfer::{
    auto_capture::CaptureArchive,
    config::{CaptureOptions, TransferFormat},
    engine::{Controller, Phase, Source},
    hdv::{self, PACKET},
    hdv_capture, media,
};

fn crc(bytes: &[u8]) -> u32 {
    let mut value = u32::MAX;
    for b in bytes {
        value ^= u32::from(*b) << 24;
        for _ in 0..8 {
            value = (value << 1)
                ^ if value & 0x80000000 != 0 {
                    0x04c11db7
                } else {
                    0
                };
        }
    }
    value
}
fn payload(packet: &[u8]) -> usize {
    if packet[3] & 32 != 0 {
        5 + packet[4] as usize
    } else {
        4
    }
}
fn aux(cc: u8, second: u8, start: bool) -> [u8; PACKET] {
    let mut p = [0xff; PACKET];
    p[..4].copy_from_slice(&[0x47, 0x41, 0xe0, 0x10 | (cc & 15)]);
    let d = &mut p[4..];
    d[..8].copy_from_slice(&[0, 0, 1, 0xbf, 0, 115, 0x44, 0x39]);
    d[37..41].copy_from_slice(&[0x04, 0x27, 0x02, 0x00]);
    d[42..45].copy_from_slice(&[0x30, 0x04, 0x23]);
    d[46..50].copy_from_slice(&[second, 0x21, 0x20, if start { 0 } else { 0x20 }]);
    p
}
fn pmt_with_aux(p: &mut [u8]) {
    let at = payload(p) + 1;
    let len = 3 + (((p[at + 1] & 15) as usize) << 8 | p[at + 2] as usize);
    let mut section = p[at..at + len - 4].to_vec();
    section.extend([0xa1, 0xe1, 0xe0, 0xf0, 0]);
    let len = section.len() + 4 - 3;
    section[1] = (section[1] & 0xf0) | ((len >> 8) as u8);
    section[2] = len as u8;
    section.extend(crc(&section).to_be_bytes());
    assert!(at + section.len() <= PACKET);
    p[at..].fill(0xff);
    p[at..at + section.len()].copy_from_slice(&section);
}
fn inject(original: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut picture = 0u8;
    for p in original.chunks_exact(PACKET) {
        let mut p: [u8; PACKET] = p.try_into().unwrap();
        if hdv::pid(&p) == 4096 {
            pmt_with_aux(&mut p);
        }
        if hdv::pid(&p) == 256 && p[1] & 64 != 0 {
            // Two distinct scene triggers: date gap, then recording-start edge.
            let mut a = aux(
                picture,
                if picture < 6 { 0x21 } else { 0x31 },
                (6..9).contains(&picture) || (12..15).contains(&picture),
            );
            if picture == 10 {
                a[4 + 42] = 0x3a;
                a[4 + 37] = 0x3a;
                a[4 + 38] = 0x6a;
            }
            output.extend(a);
            picture += 1;
        }
        output.extend(p);
    }
    output
}
fn options(folder: &Path) -> CaptureOptions {
    CaptureOptions {
        format: TransferFormat::Hdv,
        folder: folder.into(),
        max_frames: 0,
        preview: false,
        ..Default::default()
    }
}
fn reconstruct(report: &Path) -> (Vec<u8>, Vec<serde_json::Value>) {
    let mut bytes = Vec::new();
    let mut entries = Vec::new();
    let events: Vec<serde_json::Value> = std::fs::read_to_string(report)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    let tails: std::collections::HashMap<_, _> = events
        .iter()
        .filter(|e| e["event"] == "segment_tail")
        .map(|e| {
            (
                e["path"].as_str().unwrap(),
                e["tail_packets"].as_u64().unwrap() as usize,
            )
        })
        .collect();
    for event in &events {
        if event["event"] != "segment" {
            continue;
        }
        assert_eq!(
            bytes.len() / 188,
            event["input_start_packet"].as_u64().unwrap() as usize
        );
        let data = std::fs::read(event["path"].as_str().unwrap()).unwrap();
        let tail = tails
            .get(event["path"].as_str().unwrap())
            .copied()
            .unwrap_or(0);
        bytes.extend_from_slice(
            &data
                [event["prefix_packets"].as_u64().unwrap() as usize * 188..data.len() - tail * 188],
        );
        entries.push(event.clone());
    }
    (bytes, entries)
}
fn generate(path: &Path, height: u32, rate: &str) {
    generate_seconds(path, height, rate, "1.2");
}
fn generate_seconds(path: &Path, height: u32, rate: &str, seconds: &str) {
    let size = if height == 720 {
        "1280x720"
    } else {
        "1440x1080"
    };
    let mut command = media::command("ffmpeg");
    command.args([
        "-v",
        "error",
        "-f",
        "lavfi",
        "-i",
        &format!("testsrc2=size={size}:rate={rate}"),
        "-f",
        "lavfi",
        "-i",
        "sine=sample_rate=48000",
        "-t",
        seconds,
        "-c:v",
        "mpeg2video",
        "-threads:v",
        "1",
        "-g",
        "6",
        "-bf",
        "2",
        "-b:v",
        "18000000",
        "-c:a",
        "mp2",
        "-b:a",
        "384k",
        "-ac",
        "2",
        "-muxrate",
        "25000000",
        "-pcr_period",
        "20",
    ]);
    if height == 1080 {
        command.args(["-flags", "+ildct+ilme", "-top", "1"]);
    }
    let out = command.args(["-f", "mpegts"]).arg(path).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
fn decoded(path: &Path, stream: &str) -> HashSet<(i64, String)> {
    let out = media::command("ffmpeg")
        .args(["-v", "error", "-copyts", "-i"])
        .arg(path)
        .args([
            "-map",
            stream,
            "-vsync",
            "0",
            "-f",
            "framehash",
            "-hash",
            "sha256",
            "-",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter(|s| !s.starts_with('#'))
        .map(|s| {
            let fields: Vec<_> = s.split(',').map(str::trim).collect();
            (fields[2].parse().unwrap(), fields[5].to_string())
        })
        .collect()
}

fn encoded_audio(path: &Path) -> HashSet<(i64, String)> {
    let out = media::command("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_packets",
            "-show_entries",
            "packet=pts,data_hash",
            "-show_data_hash",
            "sha256",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success());
    let result: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    result["packets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["pts"].as_i64().unwrap(),
                p["data_hash"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[test]
fn metadata_parses_fragmented_aux_and_rejects_invalid_bcd_and_unknown_layouts() {
    // Minimal CRC-valid PAT and PMT, followed by a split private PES.
    fn psi(id: u16, mut section: Vec<u8>) -> [u8; 188] {
        section.extend(crc(&section).to_be_bytes());
        let mut p = [0xff; 188];
        p[..5].copy_from_slice(&[0x47, 0x40 | (id >> 8) as u8, id as u8, 0x10, 0]);
        p[5..5 + section.len()].copy_from_slice(&section);
        p
    }
    let pat = psi(0, vec![0, 0xb0, 13, 0, 1, 0xc1, 0, 0, 0, 1, 0xf0, 0]);
    let pmt = psi(
        4096,
        vec![
            2, 0xb0, 18, 0, 1, 0xc1, 0, 0, 0xe1, 0, 0xf0, 0, 0xa1, 0xe1, 0xe0, 0xf0, 0,
        ],
    );
    let mut info = hdv::Inspector::default();
    info.push(&pat);
    info.push(&pmt);
    let a = aux(0, 0x21, false);
    let mut first = [0xff; 188];
    first[..4].copy_from_slice(&[0x47, 0x41, 0xe0, 0x30]);
    first[4] = 160;
    first[5] = 0;
    first[165..].copy_from_slice(&a[4..27]);
    let mut last = [0xff; 188];
    last[..4].copy_from_slice(&[0x47, 1, 0xe0, 0x11]);
    last[4..102].copy_from_slice(&a[27..125]);
    info.push(&first);
    assert!(info.metadata.is_none());
    info.push(&last);
    let metadata = info.metadata.as_ref().unwrap();
    assert_eq!(
        metadata.recorded.unwrap().to_string(),
        "2023-04-30 20:21:21"
    );
    assert_eq!(metadata.timecode.as_deref(), Some("00:02:27:04"));
    assert!(!metadata.scene_start);
    let serial = info.metadata_serial;
    let mut bad = aux(2, 0x6a, true);
    bad[4 + 42] = 0x3a;
    bad[4 + 37] = 0x3a;
    bad[4 + 38] = 0x6a;
    info.push(&bad);
    assert_eq!(info.metadata_serial, serial);
    let mut unknown = aux(3, 0x22, true);
    unknown[10] = 0;
    info.push(&unknown);
    assert_eq!(info.metadata_serial, serial);
    // Missing continuation must not combine unrelated bytes into metadata.
    first[3] = 0x34;
    last[3] = 0x17;
    info.push(&first);
    info.push(&last);
    assert_eq!(info.metadata_serial, serial);
}

#[test]
#[ignore = "requires FFmpeg installed"]
fn scenes_preserve_all_packets_decoded_video_and_encoded_audio_with_open_gops() {
    std::fs::create_dir_all("test-output").unwrap();
    let dir = tempfile::tempdir_in("test-output").unwrap();
    for (height, rate) in [(720, "60000/1001"), (1080, "25")] {
        let source = dir.path().join(format!("{height}.m2t"));
        generate(&source, height, rate);
        let bytes = inject(&std::fs::read(&source).unwrap());
        std::fs::write(&source, &bytes).unwrap();
        let mut writer =
            hdv_capture::Writer::new(options(&dir.path().join(format!("out-{height}"))));
        for p in bytes.chunks_exact(188) {
            writer.push(p.try_into().unwrap()).unwrap();
        }
        writer.finish().unwrap();
        writer.finish().unwrap();
        assert_eq!(writer.inspector.errors, 0);
        assert_eq!(writer.files.len(), 3);
        assert!(
            writer.files[0]
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("2023-04-30_20-21-21")
        );
        let (restored, entries) = reconstruct(writer.report.as_ref().unwrap());
        assert_eq!(restored, bytes);
        assert_eq!(entries[1]["reason"], "recording_date_gap");
        assert_eq!(entries[2]["reason"], "recording_start");
        for stream in ["0:v:0", "0:a:0"] {
            let expected = if stream == "0:a:0" {
                encoded_audio(&source)
            } else {
                decoded(&source, stream)
            };
            let mut actual = HashSet::new();
            for file in &writer.files {
                actual.extend(if stream == "0:a:0" {
                    encoded_audio(file)
                } else {
                    decoded(file, stream)
                });
            }
            if stream == "0:a:0" {
                assert!(
                    actual.is_subset(&expected),
                    "splitting introduced incomplete audio packets"
                );
            }
            let missing: Vec<_> = expected.difference(&actual).collect();
            if !missing.is_empty() {
                eprintln!(
                    "expected {} actual {} missing {:?} actual {:?}",
                    expected.len(),
                    actual.len(),
                    missing.iter().take(4).collect::<Vec<_>>(),
                    actual.iter().take(4).collect::<Vec<_>>()
                );
                let kept = dir.keep();
                panic!(
                    "{height} {stream}: {} missing decoded frames; fixtures {}",
                    missing.len(),
                    kept.display()
                );
            }
        }
        // Disable scene detection: keep a single byte-identical stream.
        let mut opts = options(&dir.path().join(format!("off-{height}")));
        opts.discontinuity_seconds = 0;
        let mut off = hdv_capture::Writer::new(opts);
        for p in bytes.chunks_exact(188) {
            off.push(p.try_into().unwrap()).unwrap();
        }
        off.finish().unwrap();
        assert_eq!(off.files.len(), 1);
        assert!(off.report.is_none());
        assert_eq!(std::fs::read(&off.files[0]).unwrap(), bytes);
        // The native mixed-format path uses precisely the same splitter.
        let mut opts = options(&dir.path().join(format!("auto-{height}")));
        opts.format = TransferFormat::Auto;
        let mut archive = CaptureArchive::new(opts).unwrap();
        for chunk in bytes.chunks(188 * 17) {
            let mut tagged = vec![2];
            tagged.extend(chunk);
            archive.push_tagged(&tagged).unwrap();
        }
        archive.finish().unwrap();
        assert_eq!(archive.status().files.len(), 3);
        let reports: Vec<_> = std::fs::read_dir(dir.path().join(format!("auto-{height}")))
            .unwrap()
            .filter_map(|e| {
                let path = e.unwrap().path();
                path.to_string_lossy()
                    .ends_with(".hdv.jsonl")
                    .then_some(path)
            })
            .collect();
        assert_eq!(reports.len(), 1);
        assert_eq!(reconstruct(&reports[0]).0, bytes);
        // Controller capture publishes both segment paths and the sidecar.
        let mut c = Controller::default();
        c.capture(
            Source::File(source),
            options(&dir.path().join(format!("controller-{height}"))),
            String::new(),
            false,
        )
        .unwrap();
        c.wait();
        let status = c.snapshot();
        assert_eq!(status.phase, Phase::Finished, "{}", status.message);
        assert_eq!(status.files.len(), 3);
        assert_eq!(reconstruct(status.report.as_ref().unwrap()).0, bytes);
    }
}

#[test]
#[ignore = "requires FFmpeg installed"]
fn frame_limit_splits_preserve_packets_at_all_hdv_frame_rates() {
    let dir = tempfile::tempdir().unwrap();
    let rates = [
        "24000/1001",
        "24",
        "25",
        "30000/1001",
        "30",
        "50",
        "60000/1001",
        "60",
    ];
    for (height, count) in [(720, 8), (1080, 5)] {
        for (index, rate) in rates.iter().take(count).enumerate() {
            let source = dir.path().join(format!("{height}-{index}.m2t"));
            generate(&source, height, rate);
            let original = std::fs::read(source).unwrap();
            let mut opts = options(&dir.path().join(format!("out-{height}-{index}")));
            opts.max_frames = 3;
            let mut writer = hdv_capture::Writer::new(opts);
            for p in original.chunks_exact(188) {
                writer.push(p.try_into().unwrap()).unwrap();
            }
            writer.finish().unwrap();
            assert!(writer.files.len() >= 2, "{height} {rate}");
            assert_eq!(writer.inspector.errors, 0);
            assert_eq!(reconstruct(writer.report.as_ref().unwrap()).0, original);
            for path in &writer.files {
                assert_eq!(
                    hdv::inspect_file(path).unwrap().video,
                    writer.inspector.video
                );
            }
        }
    }
}

#[test]
#[ignore = "requires FFmpeg installed"]
fn capture_with_many_segments_publishes_every_file_and_preserves_all_packets() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("many-scenes.m2t");
    generate_seconds(&source, 720, "25", "10");
    let original = std::fs::read(&source).unwrap();
    let expected = hdv::inspect_file(&source).unwrap();
    let mut opts = options(&dir.path().join("capture"));
    opts.max_frames = 1;
    opts.control_deck = false;
    let mut controller = Controller::default();
    controller
        .capture(Source::File(source), opts, String::new(), false)
        .unwrap();
    controller.wait();
    let status = controller.snapshot();
    assert_eq!(status.phase, Phase::Finished, "{}", status.message);
    assert!(status.files.len() >= 30, "{} segments", status.files.len());
    assert_eq!(status.dropped, 0);
    assert_eq!(status.received, expected.pictures);
    assert_eq!(status.written, expected.pictures);
    assert_eq!(status.bytes, original.len() as u64);
    let (reconstructed, entries) = reconstruct(status.report.as_ref().unwrap());
    assert_eq!(reconstructed, original);
    let reported: Vec<_> = entries
        .iter()
        .map(|entry| std::path::PathBuf::from(entry["path"].as_str().unwrap()))
        .collect();
    assert_eq!(status.files, reported);
}

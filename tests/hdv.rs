use std::{io::Write, path::Path};
use universal_dv_transfer::{
    config::{CaptureOptions, TransferFormat},
    engine::{Controller, Phase, Source},
    hdv::{self, PACKET, Packetizer, Rate},
    media,
};

fn packet(index: u8) -> [u8; PACKET] {
    let mut p = [index; PACKET];
    p[..4].copy_from_slice(&[0x47, 0x1f, 0xff, 0x10]);
    p
}
#[test]
fn fragmented_188_and_192_byte_packets_are_preserved() {
    let original: Vec<_> = (0..17).map(packet).collect();
    for stride in [188, 192] {
        let mut bytes = Vec::new();
        for p in &original {
            if stride == 192 {
                bytes.extend_from_slice(&[1, 2, 3, 4]);
            }
            bytes.extend_from_slice(p);
        }
        for size in [1, 13, 187, 188, 191, 4096] {
            let mut parser = Packetizer::default();
            let mut parsed = Vec::new();
            for chunk in bytes.chunks(size) {
                parsed.extend(parser.push(chunk));
            }
            parsed.extend(parser.finish().unwrap());
            assert_eq!(parsed, original);
            assert_eq!(parser.discarded, 0);
        }
        bytes.pop();
        let mut parser = Packetizer::default();
        parser.push(&bytes);
        assert!(parser.finish().is_err());
    }
    let mut bytes = vec![0xaa; 19];
    for p in original {
        bytes.extend(p);
    }
    let mut parser = Packetizer::default();
    assert_eq!(parser.push(&bytes).len(), 17);
    assert_eq!(parser.discarded, 19);
    parser.finish().unwrap();
}
#[test]
fn hdv_settings_reject_sampling_and_m2t_names_never_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let mut options = CaptureOptions {
        format: TransferFormat::Hdv,
        folder: dir.path().into(),
        date_format: String::new(),
        every_nth: 2,
        ..Default::default()
    };
    assert!(options.validate().is_err());
    options.every_nth = 1;
    options.validate().unwrap();
    let (first, mut file) =
        universal_dv_transfer::naming::reserve_extension(&options, None, "m2t").unwrap();
    file.write_all(b"keep").unwrap();
    let (second, _) =
        universal_dv_transfer::naming::reserve_extension(&options, None, "m2t").unwrap();
    assert_ne!(first, second);
    assert_eq!(std::fs::read(first).unwrap(), b"keep");
}

#[test]
#[ignore = "requires FFmpeg installed"]
fn hdv_preview_decodes_continuous_ts_with_widescreen_letterboxing() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("preview.m2t");
    generate(&input, 720, "60000/1001", false, "0.8");
    let bytes = std::fs::read(input).unwrap();
    let preview = media::Preview::new_hdv("").unwrap();
    for chunk in bytes.chunks(hdv::BLOCK) {
        preview.offer_bytes(chunk);
    }
    let rgb = preview
        .rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .unwrap();
    assert_eq!(rgb.len(), 320 * 240 * 3);
    assert!(rgb[..320 * 30 * 3].iter().all(|v| *v == 0));
    assert!(rgb[320 * 30 * 3..320 * 210 * 3].iter().any(|v| *v != 0));
    assert!(rgb[320 * 210 * 3..].iter().all(|v| *v == 0));
}
fn generate(path: &Path, height: u32, rate: &str, interlaced: bool, seconds: &str) {
    let size = if height == 720 {
        "1280x720"
    } else {
        "1440x1080"
    };
    let source = format!("testsrc2=size={size}:rate={rate}");
    let mut command = media::command("ffmpeg");
    command.args([
        "-v",
        "error",
        "-f",
        "lavfi",
        "-i",
        &source,
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
        "-pix_fmt",
        "yuv420p",
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
    if interlaced {
        command.args(["-flags", "+ildct+ilme", "-top", "1"]);
    }
    let output = command.args(["-f", "mpegts"]).arg(path).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn finish(c: &mut Controller) {
    c.wait();
    let s = c.snapshot();
    assert_eq!(s.phase, Phase::Finished, "{}", s.message);
}
#[test]
#[ignore = "requires FFmpeg installed"]
fn all_hdv_rates_capture_export_and_probe_without_changing_packets() {
    let dir = tempfile::tempdir().unwrap();
    let rates = [
        ("24000/1001", Rate::new(24000, 1001)),
        ("24", Rate::new(24, 1)),
        ("25", Rate::new(25, 1)),
        ("30000/1001", Rate::new(30000, 1001)),
        ("30", Rate::new(30, 1)),
        ("50", Rate::new(50, 1)),
        ("60000/1001", Rate::new(60000, 1001)),
        ("60", Rate::new(60, 1)),
    ];
    for (height, interlaced, count) in [(720, false, 8), (1080, false, 5), (1080, true, 5)] {
        for (i, (rate, expected)) in rates.iter().take(count).enumerate() {
            let input = dir.path().join(format!("{height}-{interlaced}-{i}.m2t"));
            generate(&input, height, rate, interlaced, "0.28");
            let original = std::fs::read(&input).unwrap();
            let info = hdv::inspect_file(&input).unwrap();
            let video = info.video.as_ref().unwrap();
            assert_eq!(video.height, height);
            assert_eq!(video.rate, *expected);
            assert_eq!(video.progressive, !interlaced);
            assert!(info.pictures > 0);
            assert!(info.seconds > 0.);
            assert_eq!(info.errors, 0);
            assert_eq!(info.format_changes, 0);
            let mut controller = Controller::default();
            let options = CaptureOptions {
                format: TransferFormat::Hdv,
                folder: dir.path().join(format!("out-{height}-{interlaced}-{i}")),
                date_format: String::new(),
                max_frames: 0,
                preview: false,
                ..Default::default()
            };
            controller
                .capture(Source::File(input.clone()), options, String::new(), false)
                .unwrap();
            finish(&mut controller);
            let status = controller.snapshot();
            assert_eq!(status.files.len(), 1);
            assert_eq!(status.hdv.as_ref(), Some(video));
            assert_eq!(status.dropped, 0);
            assert_eq!(std::fs::read(&status.files[0]).unwrap(), original);
            let exported = dir
                .path()
                .join(format!("export-{height}-{interlaced}-{i}.m2t"));
            controller
                .record(
                    vec![status.files[0].clone()],
                    Source::File(exported.clone()),
                    String::new(),
                    false,
                    false,
                )
                .unwrap();
            finish(&mut controller);
            assert_eq!(std::fs::read(&exported).unwrap(), original);
            let probe = media::command("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-select_streams",
                    "v:0",
                    "-show_entries",
                    "stream=r_frame_rate,width,height",
                    "-of",
                    "json",
                ])
                .arg(exported)
                .output()
                .unwrap();
            assert!(probe.status.success());
            let json: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
            assert_eq!(json["streams"][0]["height"], height);
            assert_eq!(
                json["streams"][0]["r_frame_rate"],
                format!("{}/{}", expected.numerator, expected.denominator)
            );
        }
    }
}
#[test]
#[ignore = "requires FFmpeg installed"]
fn hdv_m2ts_splits_and_preflight_failures() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("source.m2t");
    generate(&input, 720, "30000/1001", false, "0.8");
    let original = std::fs::read(&input).unwrap();
    // Hardware playlists add explicit clock/counter discontinuities between
    // clips. These must reset the TS analyzer without altering media packets.
    let first = hdv::inspect_file(&input).unwrap();
    let mut joined = first.discontinuity_packets();
    joined.extend(
        original
            .chunks_exact(188)
            .map(|p| <[u8; 188]>::try_from(p).unwrap()),
    );
    joined.extend(first.discontinuity_packets());
    joined.extend(
        original
            .chunks_exact(188)
            .map(|p| <[u8; 188]>::try_from(p).unwrap()),
    );
    let mut checked = hdv::Inspector::default();
    for packet in joined {
        checked.push(&packet);
    }
    assert_eq!(checked.errors, 0);
    assert_eq!(checked.pictures, first.pictures * 2);
    let mut prefixed = Vec::new();
    for packet in original.chunks_exact(188) {
        let mut packet = <[u8; 188]>::try_from(packet).unwrap();
        if packet[3] & 0x20 != 0 && packet[4] > 0 {
            packet[5] &= !0x40;
        }
        prefixed.extend([0, 0, 0, 0]);
        prefixed.extend(packet);
    }
    let m2ts = dir.path().join("source.m2ts");
    std::fs::write(&m2ts, prefixed).unwrap();
    let mut c = Controller::default();
    let options = CaptureOptions {
        format: TransferFormat::Hdv,
        folder: dir.path().join("out"),
        date_format: String::new(),
        max_frames: 7,
        preview: false,
        ..Default::default()
    };
    c.capture(Source::File(m2ts), options, String::new(), false)
        .unwrap();
    finish(&mut c);
    let files = c.snapshot().files;
    assert!(files.len() >= 2);
    for file in &files {
        let info = hdv::inspect_file(file).unwrap();
        assert_eq!(info.video.unwrap().rate, Rate::new(30000, 1001));
    }
    let other = dir.path().join("other.m2t");
    generate(&other, 1080, "25", true, "0.28");
    let output = dir.path().join("must-not-exist.m2t");
    c.record(
        vec![input.clone(), other],
        Source::File(output.clone()),
        String::new(),
        false,
        false,
    )
    .unwrap();
    c.wait();
    assert_eq!(c.snapshot().phase, Phase::Error);
    assert!(!output.exists());
    std::fs::write(&output, b"keep").unwrap();
    c.record(
        vec![input.clone()],
        Source::File(output.clone()),
        String::new(),
        false,
        false,
    )
    .unwrap();
    c.wait();
    assert_eq!(std::fs::read(&output).unwrap(), b"keep");
    let mut broken = original;
    broken.pop();
    let truncated = dir.path().join("bad.m2t");
    std::fs::write(&truncated, broken).unwrap();
    assert!(hdv::inspect_file(&truncated).is_err());
    // A changed payload continuity counter must be detected, not silently retimed.
    let mut broken = std::fs::read(input).unwrap();
    let mut changed = false;
    for packet in broken.chunks_exact_mut(188).skip(300) {
        if ((u16::from(packet[1] & 31) << 8) | u16::from(packet[2])) == 256 && packet[3] & 0x10 != 0
        {
            packet[3] ^= 3;
            changed = true;
            break;
        }
    }
    assert!(changed);
    let corrupt = dir.path().join("corrupt.m2t");
    std::fs::write(&corrupt, broken).unwrap();
    assert!(hdv::inspect_file(&corrupt).is_err());
}

use std::{io::Cursor, path::Path};
use universal_dv_transfer::{
    avi::Type1Writer,
    config::{AviType, CaptureOptions},
    dv::{self, FieldOrder, Frame, Standard},
    engine::{Controller, Phase, Source},
    media, naming,
};
fn synthetic(standard: Standard) -> Vec<u8> {
    let mut f = vec![0xff; standard.frame_size()];
    for seq in 0..f.len() / 12000 {
        let p = seq * 12000;
        f[p] = 0x1f;
        f[p + 1] = (seq as u8) << 4 | 7;
        f[p + 2] = 0;
        f[p + 3] = if standard == Standard::Pal {
            0xbf
        } else {
            0x3f
        };
    }
    f
}
fn date(frame: &mut [u8], day: u8, second: u8) {
    frame[86..91].copy_from_slice(&[0x62, 0xff, day, 0x10, 0x20]);
    frame[94..99].copy_from_slice(&[0x63, 0, second, 0, 0]);
}

fn vaux_control(frame: &mut [u8], sequence: usize, block: usize, flags: u8) {
    let offset = sequence * 12000 + (3 + block) * 80;
    frame[offset..offset + 3].copy_from_slice(&[0x56, (sequence as u8) << 4 | 7, block as u8]);
    frame[offset + 3..offset + 8].copy_from_slice(&[0x61, 0x3f, 0xc8, flags, 0xff]);
}

#[test]
fn dv_scan_metadata_uses_valid_redundant_vaux_packs() {
    for standard in [Standard::Pal, Standard::Ntsc] {
        assert_eq!(Frame::parse(synthetic(standard)).unwrap().field_order, None);
        for (flags, expected) in [
            (0xec, FieldOrder::Progressive),
            (0xfc, FieldOrder::BottomFirst),
            (0xbc, FieldOrder::TopFirst),
        ] {
            let mut data = synthetic(standard);
            // A false pack ID in picture data must not be interpreted as VAUX.
            data[1000..1005].copy_from_slice(&[0x61, 0x3f, 0xc8, flags ^ 0x10, 0xff]);
            vaux_control(&mut data, 0, 0, flags ^ 0x10);
            vaux_control(&mut data, 1, 1, flags);
            vaux_control(&mut data, 2, 2, flags);
            assert_eq!(
                Frame::parse(data.clone()).unwrap().field_order,
                Some(expected)
            );
            // Contradictory copies with no majority remain unknown.
            data[2 * 12000 + 5 * 80 + 3] = 0xff;
            assert_eq!(Frame::parse(data.clone()).unwrap().field_order, None);
            // A VAUX block whose sequence ID is corrupt is ignored.
            data[3 * 80 + 1] = 0xf7;
            assert_eq!(Frame::parse(data).unwrap().field_order, Some(expected));
        }
    }
}
#[test]
fn dv_metadata_rejects_invalid_bcd_and_partial_frames() {
    let mut f = synthetic(Standard::Pal);
    date(&mut f, 0x16, 0x22);
    let parsed = Frame::parse(f.clone()).unwrap();
    assert_eq!(parsed.recorded.unwrap().to_string(), "2020-10-16 00:00:22");
    f[88] = 0x3f;
    assert!(Frame::parse(f.clone()).unwrap().recorded.is_none());
    f.pop();
    assert!(Frame::parse(f).is_err());
    assert!(dv::read_frame(&mut Cursor::new(vec![1; 50])).is_err());
}
#[test]
fn ntsc_detected_from_dsf_not_read_chunk_size() {
    let f = synthetic(Standard::Ntsc);
    let parsed = dv::read_frame(&mut Cursor::new(f.clone()))
        .unwrap()
        .unwrap();
    assert_eq!(parsed.standard, Standard::Ntsc);
    assert_eq!(parsed.data, f);
    assert!((Standard::Ntsc.seconds(30000) - 1001.).abs() < 0.00001);
}
#[test]
fn capture_names_are_reserved_and_never_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let o = CaptureOptions {
        folder: dir.path().into(),
        prefix: "tape".into(),
        date_format: String::new(),
        digits: 0,
        ..Default::default()
    };
    let (a, _) = naming::reserve(&o, None).unwrap();
    assert_eq!(a.file_name().unwrap(), "tape.avi");
    let (b, _) = naming::reserve(&o, None).unwrap();
    assert_eq!(b.file_name().unwrap(), "tape.1.avi");
    let (c, _) = naming::reserve(&o, None).unwrap();
    assert_eq!(c.file_name().unwrap(), "tape.2.avi");
}
#[test]
fn bad_settings_fail_before_opening_a_source() {
    let mut o = CaptureOptions {
        every_nth: 0,
        ..Default::default()
    };
    assert!(o.validate().is_err());
    o.every_nth = 1;
    o.date_format = "%Q".into();
    assert!(o.validate().is_err());
    o.date_format = "%H:%M".into();
    assert!(o.validate().is_err());
}
#[test]
fn indexed_type1_header_has_correct_container_lengths() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("one.avi");
    let mut w = Type1Writer::new(std::fs::File::create(&path).unwrap(), Standard::Pal).unwrap();
    w.write(&Frame::parse(synthetic(Standard::Pal)).unwrap())
        .unwrap();
    w.finish().unwrap();
    let bytes = std::fs::read(path).unwrap();
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize + 8,
        bytes.len()
    );
    assert_eq!(u32::from_le_bytes(bytes[48..52].try_into().unwrap()), 1);
    let hdrl_size = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize;
    let movi_start = 20 + hdrl_size;
    assert_eq!(&bytes[movi_start..movi_start + 4], b"LIST");
    assert_eq!(&bytes[movi_start + 8..movi_start + 12], b"movi");
    assert_eq!(&bytes[movi_start + 12..movi_start + 16], b"00dc");
}
fn finish(c: &mut Controller) {
    c.wait();
    let s = c.snapshot();
    assert_eq!(s.phase, Phase::Finished, "{}", s.message);
}
#[test]
fn capture_splits_scenes_and_finalizes_on_source_error() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input.dv");
    let mut bytes = Vec::new();
    for day in [0x16, 0x16, 0x17, 0x17] {
        let mut f = synthetic(Standard::Pal);
        date(&mut f, day, 0);
        bytes.extend(f);
    }
    std::fs::write(&input, &bytes).unwrap();
    let o = CaptureOptions {
        folder: dir.path().join("out"),
        avi_type: AviType::Type1,
        date_format: String::new(),
        preview: false,
        ..Default::default()
    };
    let mut c = Controller::default();
    c.capture(Source::File(input.clone()), o.clone(), String::new(), false)
        .unwrap();
    finish(&mut c);
    let s = c.snapshot();
    assert_eq!(s.files.len(), 2);
    assert_eq!(s.written, 4);
    bytes.extend_from_slice(&[0x1f, 7, 0]);
    std::fs::write(&input, bytes).unwrap();
    let mut c = Controller::default();
    c.capture(Source::File(input), o, String::new(), false)
        .unwrap();
    c.wait();
    let s = c.snapshot();
    assert_eq!(s.phase, Phase::Error);
    assert_eq!(s.written, 4);
    for p in s.files {
        let b = std::fs::read(p).unwrap();
        assert!(b.windows(4).any(|b| b == b"idx1"));
    }
}
#[test]
fn sampling_frame_limits_and_recording_playlist_order() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input.dv");
    let mut bytes = Vec::new();
    for i in 0..8 {
        let mut f = synthetic(Standard::Pal);
        f[1000] = i;
        bytes.extend(f);
    }
    std::fs::write(&input, &bytes).unwrap();
    let o = CaptureOptions {
        folder: dir.path().join("out"),
        avi_type: AviType::Type1,
        date_format: String::new(),
        preview: false,
        max_frames: 2,
        every_nth: 2,
        ..Default::default()
    };
    let mut c = Controller::default();
    c.capture(Source::File(input.clone()), o, String::new(), false)
        .unwrap();
    finish(&mut c);
    let s = c.snapshot();
    assert_eq!(s.written, 5);
    assert_eq!(s.files.len(), 3);
    let output = dir.path().join("playlist.dv");
    let mut c = Controller::default();
    c.record(
        vec![input.clone(), input],
        Source::File(output.clone()),
        String::new(),
        false,
        false,
    )
    .unwrap();
    finish(&mut c);
    assert_eq!(
        std::fs::read(output).unwrap(),
        [bytes.clone(), bytes].concat()
    );
}
#[test]
fn mixed_standard_playlist_and_existing_output_are_rejected_before_writing() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("pal.dv");
    let n = dir.path().join("ntsc.dv");
    let out = dir.path().join("out.dv");
    std::fs::write(&p, synthetic(Standard::Pal)).unwrap();
    std::fs::write(&n, synthetic(Standard::Ntsc)).unwrap();
    let mut c = Controller::default();
    c.record(
        vec![p.clone(), n],
        Source::File(out.clone()),
        String::new(),
        false,
        false,
    )
    .unwrap();
    c.wait();
    assert_eq!(c.snapshot().phase, Phase::Error);
    assert!(!out.exists());
    std::fs::write(&out, b"keep").unwrap();
    c.record(
        vec![p],
        Source::File(out.clone()),
        String::new(),
        false,
        false,
    )
    .unwrap();
    c.wait();
    assert_eq!(c.snapshot().phase, Phase::Error);
    assert_eq!(std::fs::read(out).unwrap(), b"keep");
}

fn ffmpeg(args: &[&str]) -> bool {
    media::command("ffmpeg")
        .args(args)
        .status()
        .unwrap()
        .success()
}
fn extract(path: &Path) -> Vec<u8> {
    let out = media::command("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-map", "0:v:0", "-c:v", "copy", "-f", "rawvideo", "pipe:1"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn assert_avi_scan_metadata(path: &Path, order: FieldOrder) {
    let bytes = std::fs::read(path).unwrap();
    let p = bytes.windows(4).position(|b| b == b"vprp").unwrap();
    let len = u32::from_le_bytes(bytes[p + 4..p + 8].try_into().unwrap()) as usize;
    let properties = &bytes[p + 8..p + 8 + len];
    let number = |offset| u32::from_le_bytes(properties[offset..offset + 4].try_into().unwrap());
    if order == FieldOrder::Progressive {
        assert_eq!(len, 68);
        assert_eq!(number(32), 1, "progressive AVI must declare one field");
    } else {
        assert_eq!(len, 100);
        assert_eq!(number(32), 2, "interlaced AVI must declare two fields");
        let bottom_first = u32::from(order == FieldOrder::BottomFirst);
        assert_eq!(number(64), bottom_first, "wrong first field in AVI header");
        assert_eq!(
            number(96),
            1 - bottom_first,
            "wrong second field in AVI header"
        );
    }
}

fn update_vaux_controls(data: &mut [u8], mut update: impl FnMut(&mut [u8])) {
    for sequence in data.chunks_exact_mut(12000) {
        for block in 3..6 {
            for pack in sequence[block * 80 + 3..block * 80 + 78].chunks_exact_mut(5) {
                if pack[0] == 0x61 {
                    update(pack);
                }
            }
        }
    }
}

#[test]
#[ignore = "requires FFmpeg installed"]
fn ffmpeg_type2_scan_metadata_and_mode_changes_preserve_dv_payload() {
    let dir = tempfile::tempdir().unwrap();
    for (standard, size, rate, pix) in [
        (Standard::Pal, "720x576", "25", "yuv420p"),
        (Standard::Ntsc, "720x480", "30000/1001", "yuv411p"),
    ] {
        let mut segments = Vec::new();
        for order in [
            FieldOrder::Progressive,
            FieldOrder::BottomFirst,
            FieldOrder::TopFirst,
        ] {
            let input = dir.path().join(format!("{standard:?}-{order:?}.dv"));
            let video = format!("testsrc2=size={size}:rate={rate}");
            let mut args = vec![
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                &video,
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000",
                "-t",
                "0.24",
                "-c:v",
                "dvvideo",
                "-pix_fmt",
                pix,
                "-c:a",
                "pcm_s16le",
                "-ac",
                "2",
            ];
            if order != FieldOrder::Progressive {
                args.extend([
                    "-flags",
                    "+ildct+ilme",
                    "-top",
                    if order == FieldOrder::BottomFirst {
                        "0"
                    } else {
                        "1"
                    },
                ]);
            }
            args.extend(["-f", "dv", input.to_str().unwrap()]);
            assert!(ffmpeg(&args));
            let mut data = std::fs::read(&input).unwrap();
            if order == FieldOrder::Progressive {
                // FFmpeg's DV encoder unconditionally sets IL=1 even for
                // progressive pictures. Set this fixture's IL bit to 0.
                update_vaux_controls(&mut data, |pack| pack[3] &= !0x10);
            }
            let frame = dv::read_frame(&mut Cursor::new(&data)).unwrap().unwrap();
            assert_eq!(frame.field_order, Some(order), "DV encoder fixture flags");
            segments.push((order, data));
        }
        // Continuous tape can change scan mode without a recording-date gap.
        // A missing control pack between two frames of the same mode must not split.
        let mut unknown = segments[0].1[..standard.frame_size()].to_vec();
        update_vaux_controls(&mut unknown, |pack| pack.fill(0xff));
        assert_eq!(Frame::parse(unknown.clone()).unwrap().field_order, None);
        segments[0].1.extend(unknown);
        let input = dir.path().join(format!("{standard:?}-mixed.dv"));
        std::fs::write(
            &input,
            segments
                .iter()
                .flat_map(|(_, d)| d.iter().copied())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let options = CaptureOptions {
            folder: dir.path().join(format!("{standard:?}-out")),
            avi_type: AviType::Type2,
            date_format: String::new(),
            discontinuity_seconds: 0,
            max_frames: 0,
            preview: false,
            ..Default::default()
        };
        let mut controller = Controller::default();
        controller
            .capture(Source::File(input), options, String::new(), false)
            .unwrap();
        finish(&mut controller);
        let files = controller.snapshot().files;
        assert_eq!(files.len(), 3, "scan-mode changes must start a new AVI");
        for (path, (order, data)) in files.iter().zip(segments) {
            assert_avi_scan_metadata(path, order);
            assert_eq!(
                extract(path),
                data,
                "DV payload changed for {standard:?} {order:?}"
            );
        }
    }
}
/// Requires FFmpeg: exercised in CI, covers real encoded DV rather than synthetic DIF headers.
#[test]
#[ignore = "requires FFmpeg installed"]
fn ffmpeg_pal_ntsc_type1_type2_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    for (standard, size, rate, pix) in [
        (Standard::Pal, "720x576", "25", "yuv420p"),
        (Standard::Ntsc, "720x480", "30000/1001", "yuv411p"),
    ] {
        let input = dir.path().join(format!("{standard:?}.dv"));
        assert!(ffmpeg(&[
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc2=size={size}:rate={rate}"),
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-t",
            "0.24",
            "-c:v",
            "dvvideo",
            "-pix_fmt",
            pix,
            "-c:a",
            "pcm_s16le",
            "-ac",
            "2",
            "-f",
            "dv",
            input.to_str().unwrap()
        ]));
        let original = std::fs::read(&input).unwrap();
        assert!(
            dv::read_frame(&mut Cursor::new(&original))
                .unwrap()
                .is_some()
        );
        for kind in [AviType::Type1, AviType::Type2] {
            let o = CaptureOptions {
                folder: dir.path().join(format!("{standard:?}-{kind:?}")),
                avi_type: kind,
                date_format: String::new(),
                preview: false,
                ..Default::default()
            };
            let mut c = Controller::default();
            c.capture(Source::File(input.clone()), o, String::new(), false)
                .unwrap();
            finish(&mut c);
            let p = c.snapshot().files[0].clone();
            assert_eq!(extract(&p), original, "DV payload changed in {kind:?}");
            let mut source = media::Input::open(&p, "").unwrap();
            let mut frames = 0;
            while source
                .next_frame()
                .unwrap_or_else(|e| panic!("{standard:?} {kind:?}: {e:#}"))
                .is_some()
            {
                frames += 1;
            }
            assert_eq!(frames, original.len() / standard.frame_size());
            let probe = media::command("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-show_entries",
                    "stream=codec_type,codec_name",
                    "-of",
                    "json",
                ])
                .arg(p)
                .output()
                .unwrap();
            assert!(probe.status.success());
            let json: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
            assert!(
                json["streams"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|s| s["codec_type"] == "audio")
            );
        }
    }
}
#[test]
fn timed_capture_stops_on_source_duration_and_disabled_split_stays_off() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("timed.dv");
    let mut data = Vec::new();
    for i in 0..32 {
        let mut frame = synthetic(Standard::Pal);
        date(&mut frame, if i % 2 == 0 { 0x16 } else { 0x17 }, 0);
        data.extend(frame);
    }
    std::fs::write(&input, data).unwrap();
    let options = CaptureOptions {
        folder: dir.path().join("out"),
        avi_type: AviType::Type1,
        preview: false,
        date_format: String::new(),
        duration_seconds: 1,
        discontinuity_seconds: 0,
        max_frames: 0,
        ..Default::default()
    };
    let mut controller = Controller::default();
    controller
        .capture(Source::File(input), options, String::new(), false)
        .unwrap();
    finish(&mut controller);
    let status = controller.snapshot();
    assert_eq!(status.received, 25);
    assert_eq!(status.written, 25);
    assert_eq!(status.files.len(), 1);
    assert!((status.seconds - 1.).abs() < 0.00001);
}

#[test]
#[ignore = "requires FFmpeg installed"]
fn preview_produces_rgb_without_blocking_the_capture_queue() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("preview.dv");
    assert!(ffmpeg(&[
        "-v",
        "error",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=720x576:rate=25",
        "-t",
        "0.2",
        "-c:v",
        "dvvideo",
        "-pix_fmt",
        "yuv420p",
        "-f",
        "dv",
        input.to_str().unwrap()
    ]));
    let frame = dv::read_frame(&mut Cursor::new(std::fs::read(input).unwrap()))
        .unwrap()
        .unwrap();
    let preview = media::Preview::new("").unwrap();
    for _ in 0..12 {
        preview.offer(&frame);
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    let rgb = preview
        .rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    assert_eq!(rgb.len(), 320 * 240 * 3);
    assert!(rgb.iter().any(|v| *v != 0));
}

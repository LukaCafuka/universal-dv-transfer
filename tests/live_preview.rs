use std::{
    path::Path,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};
use universal_dv_transfer::{
    hdv,
    media::{self, LivePreview, Preview},
};

static LIVE_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn generate(path: &Path, height: u32, rate: &str, dv: bool, color: &str) {
    let width = match height {
        576 | 480 => 720,
        720 => 1280,
        _ => 1440,
    };
    let source = format!("color=c={color}:size={width}x{height}:rate={rate}");
    let mut command = media::command("ffmpeg");
    command.args([
        "-v",
        "error",
        "-f",
        "lavfi",
        "-i",
        &source,
        "-t",
        "4.0",
        "-an",
        "-threads:v",
        "1",
        "-pix_fmt",
        if height == 480 && dv {
            "yuv411p"
        } else {
            "yuv420p"
        },
    ]);
    if dv {
        command.args(["-c:v", "dvvideo", "-f", "dv"]);
    } else {
        command.args([
            "-c:v",
            "mpeg2video",
            "-b:v",
            "18000000",
            "-g",
            "12",
            "-bf",
            "2",
            "-muxrate",
            "25000000",
            "-pcr_period",
            "20",
        ]);
        if height == 1080 {
            command.args(["-flags", "+ildct+ilme", "-top", "1"]);
        }
        command.args(["-f", "mpegts"]);
    }
    let output = command.arg(path).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn paced(bytes: &[u8], chunk: usize, bytes_per_second: f64, mut offer: impl FnMut(&[u8])) {
    let start = Instant::now();
    for (index, data) in bytes.chunks(chunk).enumerate() {
        let at = Duration::from_secs_f64(index as f64 * chunk as f64 / bytes_per_second);
        if let Some(wait) = at.checked_sub(start.elapsed()) {
            std::thread::sleep(wait);
        }
        offer(data);
    }
}

#[test]
#[ignore = "requires FFmpeg; paced live-input regression"]
fn live_preview_outputs_field_rate_before_the_input_is_closed() {
    let _exclusive = LIVE_TEST.lock().unwrap_or_else(|e| e.into_inner());
    let _diagnostics = std::env::var_os("UDVT_TEST_LOG")
        .map(|path| universal_dv_transfer::diagnostics::init(Path::new(&path)).unwrap());
    let dir = tempfile::tempdir().unwrap();
    for (height, rate, dv, expected_fps) in [
        (576, "25", true, 50.),
        (480, "30000/1001", true, 60000. / 1001.),
        (1080, "25", false, 50.),
        (1080, "30000/1001", false, 60000. / 1001.),
        (720, "60000/1001", false, 60000. / 1001.),
    ] {
        let path = dir
            .path()
            .join(format!("{height}-{}.bin", rate.replace('/', "-")));
        generate(&path, height, rate, dv, "red");
        let bytes = std::fs::read(path).unwrap();
        let preview = if dv {
            Preview::new("")
        } else {
            Preview::new_hdv("")
        }
        .unwrap();
        let sink = std::sync::Arc::new(std::sync::Mutex::new(None));
        preview.output_to(&sink);
        let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = finished.clone();
        let output = sink.clone();
        let display = std::thread::spawn(move || {
            let mut shown = 0;
            let mut first = None;
            let mut last = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                if output.lock().unwrap().take().is_some() {
                    shown += 1;
                    last = Instant::now();
                    first.get_or_insert(last);
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            (shown, first.map(|first| last.duration_since(first)))
        });
        let (chunk, bitrate) = if dv {
            if height == 480 {
                (120000, 120000. * 30000. / 1001.)
            } else {
                (144000, 3_600_000.)
            }
        } else {
            (hdv::BLOCK, 3_125_000.)
        };
        paced(&bytes, chunk, bitrate, |data| preview.offer_bytes(data));
        std::thread::sleep(Duration::from_millis(400));
        assert!(preview.error().is_none(), "{:?}", preview.error());
        finished.store(true, Ordering::Relaxed);
        let (shown, elapsed) = display.join().unwrap();
        assert!(
            shown as f64 >= expected_fps * 1.5,
            "{height}: only {shown} frames reached the live display"
        );
        let decoded = preview.decoded.load(Ordering::Relaxed);
        assert!(
            decoded as f64 >= expected_fps * 1.5,
            "{height}: only {decoded} frames were decoded from a 4s live input"
        );
        let fps = (shown - 1) as f64 / elapsed.unwrap().as_secs_f64();
        assert!(
            fps >= expected_fps * 0.85,
            "{height}: preview delivered {fps:.2} fps, expected {expected_fps:.2}"
        );
        eprintln!(
            "{height}: {decoded} decoded / {shown} displayed frames before EOF, {fps:.2} fps; target {expected_fps:.2} fps"
        );
        let rgb = preview.rx.try_iter().last().unwrap();
        let center = (120 * 320 + 160) * 3;
        assert!(rgb[center] > 150 && rgb[center + 2] < 80);
    }
}

#[test]
#[ignore = "requires FFmpeg; paced format-switch regression"]
fn preview_restarts_cleanly_for_dv_hdv_dv() {
    let _exclusive = LIVE_TEST.lock().unwrap_or_else(|e| e.into_inner());
    let _diagnostics = std::env::var_os("UDVT_TEST_LOG")
        .map(|path| universal_dv_transfer::diagnostics::init(Path::new(&path)).unwrap());
    let dir = tempfile::tempdir().unwrap();
    let dv = dir.path().join("dv.bin");
    generate(&dv, 576, "25", true, "red");
    let hdv = dir.path().join("hdv.bin");
    generate(&hdv, 720, "60000/1001", false, "blue");
    let dv = std::fs::read(dv).unwrap();
    let hdv = std::fs::read(hdv).unwrap();
    let mut preview = LivePreview::new("");
    for kind in [1, 2, 1, 2] {
        eprintln!("Switching live preview to format {kind}");
        let (bytes, chunk, bitrate) = if kind == 1 {
            (&dv, 144000, 3_600_000.)
        } else {
            (&hdv, hdv::BLOCK, 3_125_000.)
        };
        paced(bytes, chunk, bitrate, |data| preview.offer(kind, data));
        let deadline = Instant::now() + Duration::from_secs(3);
        let rgb = loop {
            if let Some(rgb) = preview.latest() {
                break rgb;
            }
            assert!(Instant::now() < deadline, "{:?}", preview.error());
            std::thread::sleep(Duration::from_millis(10));
        };
        let center = (120 * 320 + 160) * 3;
        assert!(if kind == 1 {
            rgb[center] > 150 && rgb[center + 2] < 80
        } else {
            rgb[center + 2] > 150 && rgb[center] < 80
        });
        assert!(preview.error().is_none());
    }
}

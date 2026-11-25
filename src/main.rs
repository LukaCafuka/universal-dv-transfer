use anyhow::{Result, ensure};
use clap::{Parser, Subcommand};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use universal_dv_transfer::{
    backend,
    config::{AviType, CaptureOptions, TransferFormat},
    engine::{Control, Controller, Phase, Source},
    media,
};
#[derive(Parser)]
#[command(
    version,
    about = "Lossless DV/DVCAM and HDV 720p/1080 tape capture and recording."
)]
struct Args {
    #[command(subcommand)]
    command: Option<Commands>,
}
#[derive(Subcommand)]
enum Commands {
    /// Open the desktop application.
    Gui,
    /// List connected FireWire tape devices (stable IDs).
    Devices,
    /// Check native device enumeration and FFmpeg availability.
    Doctor {
        #[arg(long, default_value = "ffmpeg")]
        ffmpeg: String,
    },
    /// Inspect the format, recording date and timecode of the first DV frame.
    Inspect {
        file: PathBuf,
        #[arg(long, default_value = "ffmpeg")]
        ffmpeg: String,
    },
    /// Capture from FireWire, or replay a DV/AVI file through the same capture engine.
    Capture {
        /// Auto follows mixed DV/DVCAM/HDV tapes and keeps original streams plus a capture report.
        #[arg(long, value_enum, default_value = "auto")]
        format: TransferFormat,
        #[arg(long, conflicts_with = "source")]
        device: Option<String>,
        #[arg(long, conflicts_with = "device")]
        source: Option<PathBuf>,
        #[arg(long, default_value = "captures")]
        output: PathBuf,
        #[arg(long, default_value = "capture")]
        prefix: String,
        #[arg(long, value_enum, default_value = "type2")]
        avi_type: AviType,
        #[arg(long, default_value = "%Y-%m-%d_%H-%M-%S")]
        date_format: String,
        #[arg(long, default_value_t = 2)]
        digits: usize,
        #[arg(long, default_value_t = 1)]
        discontinuity: u32,
        #[arg(long, default_value_t = 22500)]
        max_frames: u64,
        #[arg(long, default_value_t = 1)]
        every_nth: u64,
        #[arg(long, default_value_t = 0)]
        duration: u64,
        #[arg(long, default_value_t = 100)]
        buffer: usize,
        #[arg(long)]
        control_deck: bool,
        #[arg(long, default_value = "ffmpeg")]
        ffmpeg: String,
    },
    /// Send a DV/AVI playlist to tape, or export its raw DV stream for verification.
    Record {
        #[arg(required = true)]
        files: Vec<PathBuf>,
        #[arg(long, conflicts_with = "output")]
        device: Option<String>,
        #[arg(long, conflicts_with = "device")]
        output: Option<PathBuf>,
        #[arg(long, default_value = "")]
        leader: String,
        #[arg(long, default_value = "")]
        trailer: String,
        /// Automatically engage the deck's record mode. This records over the tape.
        #[arg(long)]
        control_deck: bool,
        #[arg(long, default_value = "ffmpeg")]
        ffmpeg: String,
    },
}
fn device(id: Option<String>) -> Result<String> {
    if let Some(id) = id {
        return Ok(id);
    }
    let d = backend::devices()?;
    ensure!(
        d.len() == 1,
        "Expected one tape device; found {}. Select one with --device (see devices).",
        d.len()
    );
    Ok(d[0].id.clone())
}
fn run(mut controller: Controller) -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    let signal = stop.clone();
    ctrlc::set_handler(move || signal.store(true, Ordering::Relaxed))?;
    while controller.busy() {
        if stop.load(Ordering::Relaxed) {
            controller.send(Control::Stop);
        }
        let s = controller.snapshot();
        eprint!(
            "\r{:?}: {} frames, {} written, {} dropped, {:.1}s     ",
            s.phase, s.received, s.written, s.dropped, s.seconds
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    controller.wait();
    let s = controller.snapshot();
    eprintln!("\n{}", s.message);
    for p in s.files {
        println!("{}", p.display());
    }
    if let Some(report) = s.report {
        eprintln!("Capture report: {}", report.display());
    }
    ensure!(s.phase != Phase::Error, "Transfer failed");
    Ok(())
}
fn gui() -> Result<()> {
    #[cfg(feature = "gui")]
    {
        universal_dv_transfer::ui::run().map_err(|e| anyhow::anyhow!("{e}"))
    }
    #[cfg(not(feature = "gui"))]
    {
        anyhow::bail!("This build has no GUI. Build with default features or use --help.")
    }
}
fn main() -> Result<()> {
    let command = Args::parse().command.unwrap_or(Commands::Gui);
    let _diagnostics = if matches!(command, Commands::Gui) {
        None
    } else {
        let folder = if let Commands::Capture { output, .. } = &command {
            output.clone()
        } else {
            PathBuf::from("captures")
        };
        universal_dv_transfer::diagnostics::init(&folder).ok()
    };
    match command {
        Commands::Gui => gui(),
        Commands::Devices => {
            for d in backend::devices()? {
                println!("{}\n  {}\n  DV output: {}", d.name, d.id, d.can_record);
            }
            Ok(())
        }
        Commands::Doctor { ffmpeg } => {
            println!("OS: {} / {}", std::env::consts::OS, std::env::consts::ARCH);
            media::check_ffmpeg(&ffmpeg)?;
            println!("FFmpeg: available");
            let d = backend::devices()?;
            println!("Native backend: available; {} tape device(s)", d.len());
            for d in d {
                println!("  {}", d.name);
            }
            Ok(())
        }
        Commands::Inspect { file, ffmpeg } => {
            if universal_dv_transfer::hdv::is_hdv_file(&file) {
                let info = universal_dv_transfer::hdv::inspect_file(&file)?;
                println!(
                    "{}, {} pictures, {:.3}s, {} TS bits/s",
                    info.validate()?.description(),
                    info.pictures,
                    info.seconds,
                    info.bitrate
                );
                return Ok(());
            }
            let mut input = media::Input::open(&file, &ffmpeg)?;
            let f = input
                .next_frame()?
                .ok_or_else(|| anyhow::anyhow!("No frames"))?;
            println!(
                "{:?}, {} bytes, {}x{}, recording date: {:?}, timecode: {:?}",
                f.standard,
                f.data.len(),
                720,
                f.standard.height(),
                f.recorded,
                f.timecode
            );
            Ok(())
        }
        Commands::Capture {
            format,
            device: id,
            source,
            output,
            prefix,
            avi_type,
            date_format,
            digits,
            discontinuity,
            max_frames,
            every_nth,
            duration,
            buffer,
            control_deck,
            ffmpeg,
        } => {
            let source = if let Some(p) = source {
                Source::File(p)
            } else {
                Source::Device(device(id)?)
            };
            let o = CaptureOptions {
                format,
                folder: output,
                prefix,
                date_format,
                digits,
                avi_type,
                discontinuity_seconds: discontinuity,
                max_frames,
                every_nth,
                duration_seconds: duration,
                buffer_frames: buffer,
                control_deck,
                preview: false,
            };
            let mut c = Controller::default();
            c.capture(source, o, ffmpeg, false)?;
            run(c)
        }
        Commands::Record {
            files,
            device: id,
            output,
            leader,
            trailer,
            control_deck,
            ffmpeg,
        } => {
            let list = media::expand_playlist(&files, &leader, &trailer)?;
            let target = if let Some(p) = output {
                Source::File(p)
            } else {
                Source::Device(device(id)?)
            };
            let mut c = Controller::default();
            c.record(list, target, ffmpeg, control_deck, false)?;
            run(c)
        }
    }
}

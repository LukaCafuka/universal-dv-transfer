use crate::{
    backend::{self, Device, Transport},
    config::{AviType, Settings, TransferFormat},
    engine::{Control, Controller, Phase, Source, Status},
    media,
};
use eframe::egui::{self, Color32, RichText};
use std::{
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

pub fn run() -> eframe::Result<()> {
    eframe::run_native(
        "Universal DV Transfer",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([1120., 850.])
                .with_min_inner_size([850., 650.]),
            ..Default::default()
        },
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}
struct App {
    _diagnostics: Option<crate::diagnostics::Guard>,
    settings: Settings,
    controller: Controller,
    monitor: Controller,
    monitor_source: Option<(String, String, usize)>,
    monitor_retry: Instant,
    refreshed: Instant,
    preview_frames: u32,
    preview_fps: f64,
    preview_window: Instant,
    preview_seen: Instant,
    devices: Vec<Device>,
    scan: Option<mpsc::Receiver<anyhow::Result<Vec<Device>>>>,
    file_source: bool,
    record_tab: bool,
    error: String,
    texture: Option<egui::TextureHandle>,
    record_to_file: bool,
    record_output: String,
    #[cfg(feature = "ui-smoke")]
    smoke_requested: bool,
}
impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut style = (*cc.egui_ctx.style()).clone();
        style.visuals = egui::Visuals::dark();
        style.visuals.panel_fill = Color32::from_rgb(24, 28, 32);
        style.visuals.window_fill = Color32::from_rgb(30, 35, 40);
        style.spacing.item_spacing = egui::vec2(10., 9.);
        cc.egui_ctx.set_style(style);
        let (mut settings, error) = match Settings::load() {
            Ok(s) => (s, String::new()),
            Err(e) => (Settings::default(), format!("Could not load settings: {e}")),
        };
        settings.capture.preview = true;
        let diagnostics = crate::diagnostics::init(&settings.capture.folder).ok();
        crate::diagnostics::log(
            "settings",
            format!(
                "capture={:?} record={:?} ffmpeg={:?}",
                settings.capture, settings.record, settings.ffmpeg
            ),
        );
        let mut app = Self {
            _diagnostics: diagnostics,
            settings,
            controller: Controller::default(),
            monitor: Controller::default(),
            monitor_source: None,
            monitor_retry: Instant::now(),
            refreshed: Instant::now(),
            preview_frames: 0,
            preview_fps: 0.,
            preview_window: Instant::now(),
            preview_seen: Instant::now(),
            devices: Vec::new(),
            scan: None,
            file_source: false,
            record_tab: false,
            error,
            texture: None,
            record_to_file: false,
            record_output: "playlist.dv".into(),
            #[cfg(feature = "ui-smoke")]
            smoke_requested: false,
        };
        app.refresh();
        app
    }
    fn refresh(&mut self) {
        if self.scan.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.scan = Some(rx);
        self.refreshed = Instant::now();
        std::thread::spawn(move || {
            let _ = tx.send(backend::devices());
        });
    }
    fn capture(&mut self, paused: bool) {
        self.stop_monitor();
        self.settings.capture.preview = true;
        let source = if self.file_source {
            Source::File(PathBuf::from(&self.settings.source_file))
        } else {
            Source::Device(self.settings.device_id.clone())
        };
        if let Err(e) = self.controller.capture(
            source,
            self.settings.capture.clone(),
            self.settings.ffmpeg.clone(),
            paused,
        ) {
            self.error = format!("{e:#}");
        }
    }
    fn record(&mut self) {
        self.stop_monitor();
        let result = (|| -> anyhow::Result<()> {
            let list = media::expand_playlist(
                &self.settings.playlist,
                &self.settings.leader,
                &self.settings.trailer,
            )?;
            if self.record_to_file {
                let hdv = list.first().is_some_and(|p| crate::hdv::is_hdv_file(p));
                if hdv && self.record_output == "playlist.dv" {
                    self.record_output = "playlist.m2t".into();
                } else if !hdv && self.record_output == "playlist.m2t" {
                    self.record_output = "playlist.dv".into();
                }
            }
            let target = if self.record_to_file {
                Source::File(PathBuf::from(&self.record_output))
            } else {
                Source::Device(self.settings.device_id.clone())
            };
            self.controller.record(
                list,
                target,
                self.settings.ffmpeg.clone(),
                !self.record_to_file && self.settings.record.control_deck,
                self.settings.capture.preview,
            )
        })();
        if let Err(e) = result {
            self.error = format!("{e:#}");
        }
    }
    fn stop_monitor(&mut self) {
        self.monitor.send(Control::Stop);
        self.monitor.wait();
        self.monitor_source = None;
    }
    fn sync_monitor(&mut self) {
        #[cfg(feature = "ui-smoke")]
        if std::env::var_os("UDVT_SCREENSHOT_TO").is_some() {
            return;
        }
        let available =
            !self.controller.busy() || self.controller.snapshot().phase == Phase::Finalizing;
        let desired = (available
            && (!self.file_source || self.record_tab)
            && !self.settings.device_id.is_empty())
        .then(|| {
            (
                self.settings.device_id.clone(),
                self.settings.ffmpeg.clone(),
                self.settings.capture.buffer_frames,
            )
        });
        if self.monitor_source != desired && self.monitor_source.is_some() {
            self.stop_monitor();
        }
        if let Some((id, ffmpeg, capacity)) = desired
            && !self.monitor.busy()
            && Instant::now() >= self.monitor_retry
        {
            if let Err(e) = self.monitor.monitor(id.clone(), capacity, ffmpeg.clone()) {
                crate::diagnostics::log("preview.error", format!("{e:#}"));
            }
            self.monitor_source = Some((id, ffmpeg, capacity));
            self.monitor_retry = Instant::now() + Duration::from_secs(2);
        }
        if available && self.refreshed.elapsed() >= Duration::from_secs(5) {
            self.refresh();
        }
    }
    fn source_ui(&mut self, ui: &mut egui::Ui, busy: bool) {
        ui.add_enabled_ui(!busy, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.record_tab, false, "Capture from tape");
                ui.selectable_value(&mut self.record_tab, true, "Record to tape");
            });
            ui.separator();
            ui.horizontal(|ui| {
                ui.label("FireWire device");
                let name = self
                    .devices
                    .iter()
                    .find(|d| d.id == self.settings.device_id)
                    .map(|d| d.name.as_str())
                    .unwrap_or("No tape device found");
                egui::ComboBox::from_id_salt("device")
                    .selected_text(name)
                    .width(350.)
                    .show_ui(ui, |ui| {
                        for d in &self.devices {
                            ui.selectable_value(
                                &mut self.settings.device_id,
                                d.id.clone(),
                                &d.name,
                            );
                        }
                    });
                if ui
                    .button(if self.scan.is_some() {
                        "Scanning…"
                    } else {
                        "Refresh"
                    })
                    .clicked()
                {
                    self.refresh();
                }
            });
            if !self.record_tab {
                ui.checkbox(
                    &mut self.file_source,
                    "Use a DV / AVI / HDV file as the source",
                );
                if self.file_source {
                    file_row(ui, "Source file", &mut self.settings.source_file);
                }
            }
        });
    }
    fn preview_ui(&mut self, ui: &mut egui::Ui, status: &Status) {
        let preview_status = if self.monitor.busy() {
            self.monitor.snapshot()
        } else {
            status.clone()
        };
        ui.columns(2, |columns| {
            egui::Frame::new()
                .fill(Color32::from_rgb(7, 10, 13))
                .inner_margin(12.)
                .show(&mut columns[0], |ui| {
                    let size = egui::vec2(ui.available_width(), 240.);
                    if let Some(t) = &self.texture {
                        ui.allocate_ui_with_layout(
                            size,
                            egui::Layout::top_down(egui::Align::Center),
                            |ui| {
                                let height = size.y.min(size.x * 3.0 / 4.0);
                                ui.add(
                                    egui::Image::new(t)
                                        .fit_to_exact_size(egui::vec2(height * 4.0 / 3.0, height)),
                                );
                            },
                        );
                    } else {
                        ui.allocate_ui_with_layout(
                            size,
                            egui::Layout::top_down(egui::Align::Center),
                            |ui| {
                                ui.add_space(75.);
                                ui.label(
                                    RichText::new("DV / HDV PREVIEW")
                                        .size(24.)
                                        .color(Color32::from_gray(110)),
                                );
                                ui.label(RichText::new("Connect a deck in VTR mode").weak());
                                ui.label(
                                    RichText::new(
                                        "Preview uses FFmpeg; capture keeps the original stream",
                                    )
                                    .small()
                                    .weak(),
                                );
                            },
                        );
                    }
                });
            columns[0].horizontal_wrapped(|ui| {
                ui.monospace(&preview_status.timecode);
                ui.label(&preview_status.recorded);
                ui.label(format!("Preview: {:.0} fps", self.preview_fps));
            });
            if self.monitor_source.is_some() {
                columns[0].label(RichText::new(&preview_status.message).small().weak());
            }
            let ui = &mut columns[1];
            ui.heading(if self.record_tab {
                "Tape output"
            } else {
                "Capture session"
            });
            let values = [
                (
                    if status.hdv.is_some() {
                        "Source pictures"
                    } else {
                        "Source frames"
                    },
                    status.received.to_string(),
                ),
                (
                    if status.hdv.is_some() {
                        "Written pictures"
                    } else {
                        "Written frames"
                    },
                    status.written.to_string(),
                ),
                ("Dropped / invalid", status.dropped.to_string()),
                ("Buffer", format!("{} blocks", status.buffered)),
                (
                    "Duration",
                    format!(
                        "{:02}:{:02}:{:02}",
                        status.seconds as u64 / 3600,
                        status.seconds as u64 / 60 % 60,
                        status.seconds as u64 % 60
                    ),
                ),
                (
                    "Format",
                    status
                        .hdv
                        .as_ref()
                        .map(|v| v.description())
                        .or_else(|| status.standard.map(|s| format!("{s:?} DV25")))
                        .unwrap_or_else(|| "Waiting for signal".into()),
                ),
                ("Files", status.files.len().to_string()),
            ];
            egui::Grid::new("counters")
                .num_columns(2)
                .spacing([24., 12.])
                .show(ui, |ui| {
                    for (label, value) in values {
                        ui.label(RichText::new(label).weak());
                        ui.monospace(value);
                        ui.end_row();
                    }
                });
        });
    }
    fn actions_ui(&mut self, ui: &mut egui::Ui, status: &Status, busy: bool) {
        ui.horizontal_wrapped(|ui| {
            if !busy {
                if self.record_tab {
                    let ready = self.record_to_file
                        || self
                            .devices
                            .iter()
                            .any(|d| d.id == self.settings.device_id && d.can_record);
                    let label = if self.record_to_file {
                        "Export original stream"
                    } else if !self.settings.record.control_deck {
                        "Send playlist to deck"
                    } else {
                        "Record playlist to tape"
                    };
                    if ui
                        .add_enabled(
                            ready,
                            egui::Button::new(label).fill(Color32::from_rgb(138, 59, 41)),
                        )
                        .clicked()
                    {
                        self.error.clear();
                        self.record();
                    }
                } else {
                    let ready = self.file_source || !self.settings.device_id.is_empty();
                    if ui
                        .add_enabled(
                            ready,
                            egui::Button::new("Start capture").fill(Color32::from_rgb(120, 82, 26)),
                        )
                        .clicked()
                    {
                        self.error.clear();
                        self.capture(false);
                    }
                }
            } else {
                let paused = matches!(status.phase, Phase::CapturePaused | Phase::RecordPaused);
                if ui
                    .add_enabled(
                        status.phase != Phase::Preparing,
                        egui::Button::new(if paused { "Resume" } else { "Pause" }),
                    )
                    .clicked()
                {
                    self.controller.send(if paused {
                        Control::Resume
                    } else {
                        Control::Pause
                    });
                }
                if ui.button("Stop").clicked() {
                    self.controller.send(Control::Stop);
                }
                if !self.record_tab && ui.button("New file").clicked() {
                    self.controller.send(Control::Split);
                }
                if !self.file_source {
                    for (label, t) in [
                        ("Rewind", Transport::Rewind),
                        ("Play", Transport::Play),
                        ("Pause deck", Transport::Pause),
                        ("Stop deck", Transport::Stop),
                        ("Forward", Transport::Forward),
                    ] {
                        if ui.small_button(label).clicked() {
                            self.controller.send(Control::Deck(t));
                        }
                    }
                }
            }
            if !busy && self.monitor.busy() {
                for (label, t) in [
                    ("Rewind", Transport::Rewind),
                    ("Play", Transport::Play),
                    ("Pause deck", Transport::Pause),
                    ("Stop deck", Transport::Stop),
                    ("Forward", Transport::Forward),
                ] {
                    if ui.small_button(label).clicked() {
                        self.monitor.send(Control::Deck(t));
                    }
                }
            }
        });
    }
    fn capture_options_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Capture folder");
            let mut folder = self.settings.capture.folder.display().to_string();
            if ui
                .add(egui::TextEdit::singleline(&mut folder).desired_width(430.))
                .changed()
            {
                self.settings.capture.folder = PathBuf::from(folder);
            }
            if ui.button("Browse…").clicked()
                && let Some(p) = rfd::FileDialog::new().pick_folder()
            {
                self.settings.capture.folder = p;
            }
        });
        let o = &mut self.settings.capture;
        ui.horizontal(|ui| {
            ui.label("Filename prefix");
            ui.text_edit_singleline(&mut o.prefix);
            ui.selectable_value(&mut o.format, TransferFormat::Auto, "Auto (mixed tape)");
            ui.selectable_value(&mut o.format, TransferFormat::Dv, "DV / DVCAM");
            ui.selectable_value(&mut o.format, TransferFormat::Hdv, "HDV");
        });
        if o.format != TransferFormat::Hdv {
            ui.horizontal(|ui| {
                ui.label("AVI format");
                ui.selectable_value(&mut o.avi_type, AviType::Type1, "Type 1");
                ui.selectable_value(&mut o.avi_type, AviType::Type2, "Type 2");
            });
        }
        if o.format == TransferFormat::Auto {
            o.every_nth = 1;
            ui.label("Auto: original .dv / .m2t segments; DV AVI files prepared after capture.");
            ui.label(
                "Switches and interruptions are logged. Deck/driver gaps can require recapture.",
            );
        } else if o.format == TransferFormat::Hdv {
            o.every_nth = 1;
            ui.label("HDV 720p / 1080 · Original .m2t stream · Frame rate detected automatically");
            ui.label("Scene splits use tape metadata and GOP preroll; adjacent files overlap.");
        }
        egui::CollapsingHeader::new("Capture settings")
            .default_open(true)
            .show(ui, |ui| {
                egui::Grid::new("capture-options")
                    .num_columns(4)
                    .spacing([12., 8.])
                    .show(ui, |ui| {
                        ui.label("Scene gap (seconds; 0 = off)");
                        ui.add(egui::DragValue::new(&mut o.discontinuity_seconds).range(0..=86400));
                        ui.label("Max frames / file (0 = unlimited)");
                        ui.add(egui::DragValue::new(&mut o.max_frames).range(0..=10_000_000));
                        ui.end_row();
                        ui.label("Every Nth frame");
                        ui.add_enabled(
                            o.format == TransferFormat::Dv,
                            egui::DragValue::new(&mut o.every_nth).range(1..=1_000_000),
                        );
                        ui.label("Duration (seconds; 0 = unlimited)");
                        ui.add(egui::DragValue::new(&mut o.duration_seconds).range(0..=86400));
                        ui.end_row();
                        ui.label("Buffer (frames / HDV blocks)");
                        ui.add(egui::DragValue::new(&mut o.buffer_frames).range(2..=2000));
                        ui.label("Numbering digits");
                        ui.add(egui::DragValue::new(&mut o.digits).range(0..=9));
                        ui.end_row();
                    });
                ui.horizontal(|ui| {
                    ui.label("Recording date format");
                    ui.add(egui::TextEdit::singleline(&mut o.date_format).desired_width(235.));
                    if chrono::format::StrftimeItems::new(&o.date_format)
                        .all(|i| !matches!(i, chrono::format::Item::Error))
                    {
                        let mut example = String::new();
                        use std::fmt::Write as _;
                        if write!(
                            &mut example,
                            "{}",
                            chrono::Local::now().naive_local().format(&o.date_format)
                        )
                        .is_ok()
                        {
                            ui.label(example);
                        } else {
                            ui.colored_label(Color32::LIGHT_RED, "Unsupported date field");
                        }
                    } else {
                        ui.colored_label(Color32::LIGHT_RED, "Invalid format");
                    }
                });
            });
    }
    fn record_options_ui(&mut self, ui: &mut egui::Ui) {
        if !self.record_to_file {
            ui.checkbox(
                &mut self.settings.record.control_deck,
                "Start recording on the deck automatically",
            );
            if self.settings.record.control_deck {
                ui.label("Record starts the deck's recording. Pause and Stop pause recording on the deck.");
            } else {
                ui.label(
                    "Video is sent to the deck. Start and stop recording using the deck's buttons.",
                );
            }
        }
        ui.label("Files play in order. Use DV/AVI, or HDV .m2t/.ts/.m2ts clips with matching video formats.");
        let mut remove = None;
        let mut swap = None;
        for (i, p) in self.settings.playlist.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.monospace(format!("{:02}", i + 1));
                if ui
                    .add_enabled(i > 0, egui::Button::new("Up").small())
                    .clicked()
                {
                    swap = Some((i, i - 1));
                }
                if ui
                    .add_enabled(
                        i + 1 < self.settings.playlist.len(),
                        egui::Button::new("Down").small(),
                    )
                    .clicked()
                {
                    swap = Some((i, i + 1));
                }
                if ui.small_button("Remove").clicked() {
                    remove = Some(i);
                }
                ui.label(p.display().to_string());
            });
        }
        if let Some(i) = remove {
            self.settings.playlist.remove(i);
        } else if let Some((a, b)) = swap {
            self.settings.playlist.swap(a, b);
        }
        if ui.button("Add DV / AVI / HDV files…").clicked()
            && let Some(files) = rfd::FileDialog::new()
                .add_filter("Tape video", &["avi", "dv", "m2t", "ts", "m2ts", "mts"])
                .pick_files()
        {
            self.settings.playlist.extend(files);
        }
        file_row(ui, "Leader clip (optional)", &mut self.settings.leader);
        file_row(ui, "Append clip (optional)", &mut self.settings.trailer);
        ui.checkbox(
            &mut self.record_to_file,
            "Export the original DV or HDV stream to a new file",
        );
        if self.record_to_file {
            ui.text_edit_singleline(&mut self.record_output);
        } else if self.settings.record.control_deck {
            ui.colored_label(
                Color32::from_rgb(237, 181, 86),
                "Recording replaces video at the tape's current position.",
            );
        }
    }
    #[cfg(feature = "ui-smoke")]
    fn smoke(&mut self, ctx: &egui::Context) {
        if std::env::var_os("UDVT_SMOKE_RECORD").is_some() {
            self.record_tab = true;
        }
        if std::env::var_os("UDVT_SMOKE_MANUAL_RECORD").is_some() {
            self.record_tab = true;
            self.settings.record.control_deck = false;
        }
        if std::env::var_os("UDVT_SMOKE_HDV").is_some() {
            self.settings.capture.format = TransferFormat::Hdv;
        }
        if std::env::var_os("UDVT_SMOKE_AUTO").is_some() {
            self.settings.capture.format = TransferFormat::Auto;
        }
        if let Ok(path) = std::env::var("UDVT_SCREENSHOT_TO") {
            if !self.smoke_requested && ctx.cumulative_pass_nr() >= 4 {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
                self.smoke_requested = true;
            }
            let screenshot = ctx.input(|i| {
                i.events.iter().find_map(|e| {
                    if let egui::Event::Screenshot { image, .. } = e {
                        Some(image.clone())
                    } else {
                        None
                    }
                })
            });
            if let Some(pixels) = screenshot {
                let data: Vec<u8> = pixels.pixels.iter().flat_map(|c| c.to_array()).collect();
                image::save_buffer(
                    path,
                    &data,
                    pixels.size[0] as u32,
                    pixels.size[1] as u32,
                    image::ColorType::Rgba8,
                )
                .expect("Save GUI screenshot");
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
}
fn file_row(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.label(label);
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(value).desired_width(330.));
        if ui.button("Browse…").clicked()
            && let Some(p) = rfd::FileDialog::new()
                .add_filter("Tape video", &["avi", "dv", "m2t", "ts", "m2ts", "mts"])
                .pick_file()
        {
            *value = p.display().to_string();
        }
    });
}
impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        #[cfg(feature = "ui-smoke")]
        self.smoke(ctx);
        if let Some(rx) = &self.scan
            && let Ok(result) = rx.try_recv()
        {
            self.scan = None;
            match result {
                Ok(d) => {
                    self.devices = d;
                    if self.settings.device_id.is_empty() {
                        self.settings.device_id = self
                            .devices
                            .first()
                            .map(|d| d.id.clone())
                            .unwrap_or_default();
                    }
                }
                Err(e) => self.error = e.to_string(),
            }
        }
        let rgb = if self.monitor.busy() {
            self.monitor.preview.lock().unwrap().take()
        } else {
            self.controller.preview.lock().unwrap().take()
        };
        if let Some(rgb) = rgb {
            self.preview_frames += 1;
            self.preview_seen = Instant::now();
            let image = egui::ColorImage::from_rgb([320, 240], &rgb);
            if let Some(t) = &mut self.texture {
                t.set(image, egui::TextureOptions::LINEAR);
            } else {
                self.texture =
                    Some(ctx.load_texture("dv-preview", image, egui::TextureOptions::LINEAR));
            }
        }
        if self.preview_window.elapsed() >= Duration::from_secs(1) {
            self.preview_fps =
                f64::from(self.preview_frames) / self.preview_window.elapsed().as_secs_f64();
            self.preview_frames = 0;
            self.preview_window = Instant::now();
        }
        let status = self.controller.snapshot();
        let busy = self.controller.busy();
        if !busy {
            for path in ctx.input(|i| {
                i.raw
                    .dropped_files
                    .iter()
                    .filter_map(|f| f.path.clone())
                    .collect::<Vec<_>>()
            }) {
                if self.record_tab {
                    self.settings.playlist.push(path);
                } else {
                    self.file_source = true;
                    self.settings.source_file = path.display().to_string();
                }
            }
        }
        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.add_space(8.);
            ui.horizontal(|ui| {
                ui.heading("Universal DV Transfer");
                ui.label(RichText::new("DV · DVCAM · HDV").color(Color32::from_rgb(237, 181, 86)));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new("0.1  /  Tape transfer").weak());
                });
            });
            ui.add_space(6.);
        });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.add_space(4.);
            ui.horizontal_wrapped(|ui| {
                let color = match status.phase {
                    Phase::Error => Color32::LIGHT_RED,
                    Phase::Capturing | Phase::Recording => Color32::from_rgb(237, 181, 86),
                    _ => Color32::LIGHT_GREEN,
                };
                ui.colored_label(color, format!("{:?}", status.phase));
                ui.label(&status.message);
            });
            if let Some(report) = &status.report {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!(
                        "Format switches: {}   Issues: {}   Report:",
                        status.switches, status.issues
                    ));
                    ui.add(egui::Label::new(report.display().to_string()).selectable(true));
                });
            }
            ui.add_space(4.);
        });
        egui::CentralPanel::default().show(ctx,|ui|{egui::ScrollArea::vertical().show(ui,|ui|{
            if !self.error.is_empty(){egui::Frame::group(ui.style()).show(ui,|ui|{ui.colored_label(Color32::LIGHT_RED,&self.error);if ui.small_button("Dismiss").clicked(){self.error.clear();}});}
            self.source_ui(ui,busy);ui.add_space(4.);self.preview_ui(ui,&status);ui.add_space(8.);self.actions_ui(ui,&status,busy);
            ui.add_enabled_ui(!busy,|ui|{
                ui.separator();if self.record_tab{self.record_options_ui(ui);}else{self.capture_options_ui(ui);}
                if !self.record_tab {ui.checkbox(&mut self.settings.capture.control_deck,"Start / pause playback on the deck automatically");}
                egui::CollapsingHeader::new("Tools & compatibility").show(ui,|ui|{
                    if let Some(path)=crate::diagnostics::path() {ui.label("Diagnostic log");ui.add(egui::Label::new(path.display().to_string()).selectable(true));}
                    ui.horizontal(|ui|{ui.label("FFmpeg executable");ui.add(egui::TextEdit::singleline(&mut self.settings.ffmpeg).hint_text("ffmpeg (from PATH)").desired_width(450.));});
                    ui.label("DV25 / DVCAM and HDV 720p / 1080. Original stream capture and tape output.");
                    ui.label("macOS FireWire requires Apple's FireWire drivers (macOS 15 or earlier). Physical transfer validation is still required.");
                });
            });
            if !status.files.is_empty(){egui::CollapsingHeader::new("Output files").show(ui,|ui|{for p in &status.files{ui.label(p.display().to_string());}});}
        });});
        self.sync_monitor();
        ctx.request_repaint_after(Duration::from_millis(
            if self.preview_seen.elapsed() < Duration::from_secs(1) {
                8
            } else {
                40
            },
        ));
    }
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.controller.send(Control::Stop);
        self.monitor.send(Control::Stop);
        self.controller.wait();
        self.monitor.wait();
        #[cfg(feature = "ui-smoke")]
        if std::env::var_os("UDVT_SCREENSHOT_TO").is_some() {
            return;
        }
        if let Err(e) = self.settings.save() {
            eprintln!("Could not save settings: {e}");
        }
    }
}

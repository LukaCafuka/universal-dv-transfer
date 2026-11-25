use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
pub enum AviType {
    Type1,
    Type2,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum, Default)]
pub enum TransferFormat {
    #[default]
    Auto,
    Dv,
    Hdv,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureOptions {
    pub format: TransferFormat,
    pub folder: PathBuf,
    pub prefix: String,
    pub date_format: String,
    pub digits: usize,
    pub avi_type: AviType,
    pub discontinuity_seconds: u32,
    pub max_frames: u64,
    pub every_nth: u64,
    pub duration_seconds: u64,
    pub buffer_frames: usize,
    pub control_deck: bool,
    pub preview: bool,
}
impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            format: TransferFormat::Auto,
            folder: PathBuf::from("captures"),
            prefix: "capture".into(),
            date_format: "%Y-%m-%d_%H-%M-%S".into(),
            digits: 2,
            avi_type: AviType::Type2,
            discontinuity_seconds: 1,
            max_frames: 22_500,
            every_nth: 1,
            duration_seconds: 0,
            buffer_frames: 100,
            control_deck: false,
            preview: true,
        }
    }
}
impl CaptureOptions {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.prefix.is_empty()
                && !self
                    .prefix
                    .contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|'])
                && self.prefix != "."
                && self.prefix != "..",
            "Prefix must be a filename, without path separators or reserved characters"
        );
        let base = self
            .prefix
            .split('.')
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        ensure!(
            !self.prefix.ends_with([' ', '.'])
                && !self.prefix.chars().any(char::is_control)
                && ![
                    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6",
                    "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7",
                    "LPT8", "LPT9"
                ]
                .contains(&base.as_str()),
            "Prefix is not a portable filename"
        );
        ensure!(self.every_nth > 0, "Every Nth frame must be at least 1");
        ensure!(
            self.format == TransferFormat::Dv || self.every_nth == 1,
            "Every-Nth sampling requires DV mode; Auto and HDV preserve all received pictures"
        );
        ensure!(
            (2..=2000).contains(&self.buffer_frames),
            "Buffer must contain 2–2000 frames"
        );
        ensure!(self.digits <= 9, "Numbering digits must be 0–9");
        ensure!(
            chrono::format::StrftimeItems::new(&self.date_format)
                .all(|i| !matches!(i, chrono::format::Item::Error)),
            "Invalid date format"
        );
        ensure!(
            !self
                .date_format
                .contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|']),
            "Date format must produce a portable filename (use dashes instead of colons)"
        );
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordOptions {
    pub control_deck: bool,
}
impl Default for RecordOptions {
    fn default() -> Self {
        Self { control_deck: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Settings {
    pub capture: CaptureOptions,
    pub record: RecordOptions,
    pub source_file: String,
    pub playlist: Vec<PathBuf>,
    pub leader: String,
    pub trailer: String,
    pub ffmpeg: String,
    pub device_id: String,
}
impl Settings {
    pub fn path() -> Result<PathBuf> {
        Ok(
            directories::ProjectDirs::from("org", "UniversalDV", "UniversalDVTransfer")
                .context("No settings directory")?
                .config_dir()
                .join("settings.json"),
        )
    }
    pub fn load() -> Result<Self> {
        let p = Self::path()?;
        if !p.exists() {
            return Ok(Self::default());
        }
        Ok(serde_json::from_slice(&std::fs::read(&p)?)?)
    }
    pub fn save(&self) -> Result<()> {
        let p = Self::path()?;
        std::fs::create_dir_all(p.parent().unwrap())?;
        let mut temp = tempfile::NamedTempFile::new_in(p.parent().unwrap())?;
        use std::io::Write;
        temp.write_all(&serde_json::to_vec_pretty(self)?)?;
        temp.as_file().sync_all()?;
        temp.persist(p)?;
        Ok(())
    }
}

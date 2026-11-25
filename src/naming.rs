use crate::config::CaptureOptions;
use anyhow::{Context, Result, bail};
use chrono::{Local, NaiveDateTime};
use std::{
    fs::{File, OpenOptions},
    path::PathBuf,
};

/// Atomically reserve the name. Existing media is never overwritten, even by another capture.
pub fn reserve(options: &CaptureOptions, date: Option<NaiveDateTime>) -> Result<(PathBuf, File)> {
    reserve_extension(options, date, "avi")
}
pub fn reserve_extension(
    options: &CaptureOptions,
    date: Option<NaiveDateTime>,
    extension: &str,
) -> Result<(PathBuf, File)> {
    std::fs::create_dir_all(&options.folder)?;
    let mut stamp = String::new();
    use std::fmt::Write as _;
    write!(
        &mut stamp,
        "{}",
        date.unwrap_or_else(|| Local::now().naive_local())
            .format(&options.date_format)
    )
    .context("Date format requires fields unavailable in a tape recording date")?;
    let stamp: String = stamp
        .chars()
        .map(|c| {
            if c.is_control() || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let stem = if stamp.is_empty() {
        options.prefix.clone()
    } else {
        format!("{}.{}", options.prefix, stamp)
    };
    let mut highest = None;
    for entry in std::fs::read_dir(&options.folder)? {
        let name = entry?.file_name();
        let name = name.to_string_lossy();
        if let Some(n) = name
            .strip_prefix(&format!("{stem}."))
            .and_then(|s| s.strip_suffix(&format!(".{extension}")))
            && !n.is_empty()
            && n.bytes().all(|c| c.is_ascii_digit())
            && let Ok(n) = n.parse::<u64>()
        {
            highest = Some(highest.map_or(n, |h: u64| h.max(n)));
        }
    }
    let mut number = highest.map_or(0, |n| n.saturating_add(1));
    loop {
        let filename = if options.digits == 0 && highest.is_none() && number == 0 {
            format!("{stem}.{extension}")
        } else {
            format!(
                "{stem}.{number:0width$}.{extension}",
                width = options.digits
            )
        };
        let path = options.folder.join(filename);
        match OpenOptions::new()
            .write(true)
            .read(true)
            .create_new(true)
            .open(&path)
        {
            Ok(f) => return Ok((path, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                number = number
                    .checked_add(1)
                    .context("Filename counter exhausted")?
            }
            Err(e) => bail!("Cannot create {}: {e}", path.display()),
        }
    }
}

//! Append-only agent log.
//!
//! Records deploy events to a file on the target host so operators
//! can inspect what happened after the fact.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

/// Appends timestamped lines to a file.
pub struct FileLog {
    file: File,
}

impl FileLog {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(FileLog { file })
    }

    pub fn log(&self, msg: fmt::Arguments<'_>) {
        let ts = format_timestamp();
        let _ = writeln!(&self.file, "{ts} {msg}");
    }
}

fn format_timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is after 1970");
    let secs = now.as_secs() as i64;
    let ms = now.subsec_millis();
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::gmtime_r(&secs, &mut tm) };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        ms,
    )
}

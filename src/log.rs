// Deptool -- A declarative configuration deployment tool.
// Copyright 2026 Ruud van Asseldonk

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// A copy of the License has been included in the root of the repository.

//! Append-only agent log.
//!
//! Records deploy events to a file on the target host so operators
//! can inspect what happened after the fact.

use std::fmt;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use crate::prim::gmtime;

/// Appends timestamped lines to a file.
///
/// Each line is formatted into a local buffer and written in a single
/// `write_all` call -- one syscall per log line.
pub struct FileLog {
    file: File,
}

impl FileLog {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
        // OpenOptions::mode only applies on creation; tighten existing
        // files too.
        fs::set_permissions(path, Permissions::from_mode(0o600))?;
        Ok(FileLog { file })
    }

    pub fn log(&self, msg: fmt::Arguments<'_>) {
        // We could keep a buffer across calls to avoid reallocating, but
        // that would require &mut self, which complicates the call sites.
        // At ~15 calls per deploy a fresh buffer is negligible.
        let mut buf = Vec::with_capacity(4096);
        write_timestamp(&mut buf);
        let _ = write!(buf, " {msg}\n");
        let _ = (&self.file).write_all(&buf);
    }
}

fn write_timestamp(buf: &mut Vec<u8>) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is after 1970");
    let tm = gmtime(now.as_secs() as i64);
    let _ = write!(
        buf,
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        now.subsec_millis(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_log_open_locks_file_to_owner() {
        let dir = crate::testutil::TempDir::new("log_perms");
        let path = dir.path().join("agent.log");

        // Open creates the file; widening between two opens checks
        // that the post-open chmod runs on existing files too, not
        // just on creation.
        let _ = FileLog::open(&path).expect("fresh log opens");
        fs::set_permissions(&path, Permissions::from_mode(0o644)).expect("widen succeeds");
        let _ = FileLog::open(&path).expect("reopen succeeds");
        let mode = fs::metadata(&path)
            .expect("file exists")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "file is owner-only after reopen");
    }
}

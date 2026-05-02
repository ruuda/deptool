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
use std::fs::{File, OpenOptions};
use std::io::Write;
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
        let file = OpenOptions::new().create(true).append(true).open(path)?;
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

// Deptool -- A declarative configuration deployment tool.
// Copyright 2026 Ruud van Asseldonk

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// A copy of the License has been included in the root of the repository.

//! Installation and cleanup of the `deptool` binary on target hosts.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use crate::deploy::Connection;
use crate::error::{HostError, Result};
use crate::prim::Hostname;

/// How to connect to and install the agent on a host.
pub trait HostConnector: Send + Sync {
    fn connect(&self, host: &Hostname) -> std::result::Result<Box<dyn Connection>, HostError>;
    fn install(&self, host: &Hostname) -> std::result::Result<(), HostError>;
}

pub const BIN_DIR: &str = "/var/lib/deptool/bin";

/// Build an `ssh` command with our connection-tuning options.
///
/// Bounds the connect handshake -- so unreachable hosts fail fast
/// instead of waiting the OS TCP default (~2 min) -- and the idle
/// keepalive, so a host that hangs mid-session releases the deploy
/// instead of blocking it indefinitely.
pub fn ssh_command() -> Command {
    let connect_timeout_seconds = 10;
    let server_alive_interval_seconds = 10;
    let server_alive_count_max = 3;
    let mut cmd = Command::new("ssh");
    cmd.args([
        "-o",
        &format!("ConnectTimeout={connect_timeout_seconds}"),
        "-o",
        &format!("ServerAliveInterval={server_alive_interval_seconds}"),
        "-o",
        &format!("ServerAliveCountMax={server_alive_count_max}"),
    ]);
    cmd
}

/// Git commit this binary was built from.
///
/// Release builds refuse to start from a dirty tree (see `build.rs`), so
/// the commit alone identifies the source the binary was built from.
pub const BUILD_COMMIT: &str = env!("BUILD_COMMIT");

/// Committer date of `BUILD_COMMIT` in `YYYY-MM-DD` form.
pub const BUILD_COMMIT_DATE: &str = env!("BUILD_COMMIT_DATE");

/// `uname -sm` output the build target prints, e.g. "Linux x86_64".
///
/// Set by `build.rs` from the cargo target triple. Used to skip the
/// binaries-cache lookup when deploying to a host of the same platform
/// as the operator's own binary.
pub const BUILD_PLATFORM: &str = env!("BUILD_PLATFORM");

/// Directory of deptool binaries to push to target hosts, with one subdir
/// per host platform.
///
/// Subdir names are the host's `uname -sm` output, lowercased with spaces
/// replaced by hyphens, e.g. `linux-x86_64`. Resolves in order:
///   1. `$DEPTOOL_BIN_DIR` -- explicit override, e.g. point at
///      `target/deptool-bin` for local-dev cross-arch deploys.
///   2. `$XDG_CACHE_HOME/deptool`.
///   3. `$HOME/.cache/deptool`.
pub fn binaries_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("DEPTOOL_BIN_DIR").filter(|s| !s.is_empty()) {
        return PathBuf::from(p);
    }
    let root = match std::env::var_os("XDG_CACHE_HOME").filter(|s| !s.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => {
            let home = std::env::var_os("HOME").expect("XDG_CACHE_HOME or HOME is set");
            PathBuf::from(home).join(".cache")
        }
    };
    root.join("deptool")
}

/// Cache-dir subdir name for a host platform: `uname -sm` output,
/// lowercased with spaces replaced by hyphens. `build.rcl` applies the
/// same transformation when laying out artifacts under `target/deptool-bin/`.
fn platform_subdir(platform: &str) -> String {
    let subdir = platform.to_lowercase().replace(' ', "-");
    // Defensive: uname comes from a remote host. Refuse anything that
    // could escape the cache directory when joined as a path component.
    assert!(
        !subdir.contains('/'),
        "uname output is a single path component, got {subdir:?}",
    );
    subdir
}

const GC_MAX_SIZE_BYTES: u64 = 64 * 1024 * 1024;
const GC_MIN_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Install the deptool binary on the target host.
///
/// We open a single SSH session running a megacommand that probes the
/// host's kernel and machine, then waits for the binary on stdin via
/// `dd`. The driver reads the probe output, picks the matching binary
/// from the local cache, and writes it to the session. The remote
/// `sha256sum` is read back to verify the transfer was not corrupted.
///
/// One SSH session covers probe + install -- saves a round trip versus
/// probing and installing separately.
pub fn install_binary(
    binaries_dir: &Path,
    bin_name: &str,
    remote_bin_path: &str,
    host: &Hostname,
) -> std::result::Result<(), HostError> {
    let install_command = [
        // -sm prints kernel-name and machine, e.g. "Linux x86_64".
        "uname -sm",
        "sudo mkdir -p /var/lib/deptool/{bin,apps,store}",
        &format!("sudo dd status=none of={remote_bin_path}"),
        &format!("sudo chmod +x {remote_bin_path}"),
        &format!("sudo sha256sum {remote_bin_path}"),
    ]
    .join(" && ");
    let mut child = ssh_command()
        .args([&host.0, &install_command])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(HostError::connection_failed)?;

    // Reap the SSH child on every exit path so we don't leave zombies.
    let result = run_install_session(&mut child, binaries_dir, bin_name);
    let _ = child.wait();
    result
}

/// Pick the local file to push to a host running `platform`.
///
/// If the host's platform matches the operator's own build, return the
/// running executable -- the common same-arch case shouldn't require a
/// populated binaries cache. Otherwise, return the cache path; uname
/// output is used verbatim as the subdir name (with spaces hyphenated).
/// Mapping uname output to Rust target triples is a build-time concern,
/// kept out of the binary so a release tarball and an operator's
/// `uname -sm` agree by construction.
fn resolve_binary_path(binaries_dir: &Path, bin_name: &str, platform: &str) -> PathBuf {
    if platform == BUILD_PLATFORM {
        return std::env::current_exe().expect("current exe path is known");
    }
    binaries_dir.join(platform_subdir(platform)).join(bin_name)
}

fn run_install_session(
    child: &mut std::process::Child,
    binaries_dir: &Path,
    bin_name: &str,
) -> std::result::Result<(), HostError> {
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));

    // First line on stdout is the uname output. uname runs before dd
    // in the && chain, so its output reaches us before dd needs the
    // binary on stdin.
    let mut uname_line = String::new();
    stdout
        .read_line(&mut uname_line)
        .map_err(HostError::connection_failed)?;
    if uname_line.is_empty() {
        return Err(HostError::ConnectionFailed(
            "host closed connection before reporting uname".into(),
        ));
    }
    let platform = uname_line.trim();
    let binary_path = resolve_binary_path(binaries_dir, bin_name, platform);
    // Distinguish "not in cache" (expected first-time case, with a
    // remediation hint) from other I/O errors (permission denied, disk
    // failure, etc.). Conflating them would lie to the operator.
    let binary = match std::fs::read(&binary_path) {
        Ok(b) => b,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(HostError::SetupMissingBinary {
                platform: platform.to_string(),
                path: binary_path,
            });
        }
        Err(err) => {
            return Err(HostError::SetupReadError {
                path: binary_path,
                cause: err.to_string(),
            });
        }
    };

    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(&binary)
        .map_err(HostError::connection_failed)?;

    // Compute the expected hash in parallel with the remote -- by the
    // time write_all returns, the remote shell is already running chmod
    // and sha256sum, so the local hash is ready before we'd otherwise
    // be able to read the remote sum.
    // TODO: We could compute this once at startup and pass it in,
    // avoiding a second pass over the binary on every install.
    let expected_hash: String = hmac_sha256::Hash::hash(&binary)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    // Parse the hash out of a `sha256sum` output line: `<hash>  <filename>\n`.
    let mut sha_line = String::new();
    stdout
        .read_line(&mut sha_line)
        .map_err(HostError::connection_failed)?;
    let actual_hash = sha_line.split_whitespace().next().ok_or_else(|| {
        HostError::SetupProtocolError("missing sha256sum output after install".into())
    })?;

    if actual_hash != expected_hash {
        return Err(HostError::SetupChecksumMismatch {
            actual_hash: actual_hash.into(),
            expected_hash,
        });
    }

    Ok(())
}

/// Remove old `deptool-*` binaries from the bin dir until total size is
/// under 64 MiB.
///
/// Skips if `current_exe` is not inside the bin dir (we're not running from
/// the expected production location). Never deletes the currently-running
/// binary or files younger than 24 hours.
pub fn gc_bin_dir(current_exe: &Path) -> Result<()> {
    // This function deliberately hard-codes the bin dir, so we don't delete
    // files in arbitrary locations from the system it runs on.
    let bin_dir = Path::new(BIN_DIR);

    if !current_exe.starts_with(bin_dir) {
        return Ok(());
    }

    let now = SystemTime::now();
    let mut total_size: u64 = 0;

    // Collect deletable files: deptool-* files that are not the current
    // exe and are older than 24h. Track total size of all deptool-* files
    // (including non-deletable ones) to decide whether GC is needed.
    let mut deletable: Vec<(std::path::PathBuf, u64, SystemTime)> = Vec::new();
    for entry in std::fs::read_dir(bin_dir)? {
        let entry = entry?;
        let path = entry.path();
        match path.file_name().and_then(|n| n.to_str()) {
            Some(n) if n.starts_with("deptool-") => {}
            _ => continue,
        };
        let meta = match std::fs::metadata(&path) {
            Ok(m) if m.is_file() => m,
            _ => continue,
        };
        let mtime = meta.modified().unwrap_or(now);
        total_size += meta.len();

        let age = now.duration_since(mtime).unwrap_or_default();
        if path != current_exe && age >= GC_MIN_AGE {
            deletable.push((path, meta.len(), mtime));
        }
    }

    // Oldest first.
    deletable.sort_by_key(|(_, _, mtime)| *mtime);

    for (path, size, _) in &deletable {
        if total_size <= GC_MAX_SIZE_BYTES {
            break;
        }
        std::fs::remove_file(path)?;
        total_size -= size;
    }

    Ok(())
}

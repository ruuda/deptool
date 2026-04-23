//! Installation and cleanup of the `deptool` binary on target hosts.

use std::path::Path;
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

const GC_MAX_SIZE_BYTES: u64 = 64 * 1024 * 1024;
const GC_MIN_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Return the truncated and hex-formatted SHA256 of `bytes`.
///
/// The prefix len is in bytes, so the returned hex string is twice as long.
pub fn truncated_sha256(bytes: &[u8], prefix_len: usize) -> String {
    let digest = hmac_sha256::Hash::hash(bytes);
    digest
        .iter()
        .take(prefix_len)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Install the the binary on the target host.
///
/// We execute a single command over SSH. The command reads the binary from
/// stdin via `dd`, makes it executable, and prints its sha256sum so the
/// caller can verify the transfer was successful.
pub fn install_binary(
    host: &Hostname,
    remote_bin_path: &str,
    binary: &[u8],
) -> std::result::Result<(), HostError> {
    let install_command = [
        "sudo mkdir -p /var/lib/deptool/{bin,apps,store}",
        &format!("sudo dd status=none of={remote_bin_path}"),
        &format!("sudo chmod +x {remote_bin_path}"),
        &format!("sudo sha256sum {remote_bin_path}"),
    ]
    .join(" && ");
    use std::io::Write;
    let mut child = Command::new("ssh")
        .args([&host.0, &install_command])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(HostError::connection_failed)?;
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(binary)
        .map_err(HostError::connection_failed)?;

    // Compute the expected shasum while sending.
    // SHA256 is 32 bytes, so this is the full hash.
    // TODO: We could compute it only once at startup and pass it here.
    let expected_hash = truncated_sha256(binary, 32);

    let output = child
        .wait_with_output()
        .map_err(HostError::connection_failed)?
        .stdout;

    // Parse the hash out of a `sha256sum` output line (`<hash>  <filename>\n`).
    let make_err =
        || HostError::SetupProtocolError("Failed to read sha256sum after installation.".into());
    let actual_hash = std::str::from_utf8(&output)
        .map_err(|_| make_err())?
        .split_whitespace()
        .next()
        .ok_or_else(make_err)?;

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

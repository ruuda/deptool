//! Installation of the `deptool` binary on target hosts.

use std::process::Command;

use crate::error::{Error, Result};
use crate::prim::Hostname;

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
/// stdin via `dd`, makes it executable, symlinks it as the current version, and
/// prints its sha256sum so the caller can verify the transfer was successful.
pub fn install_binary(host: &Hostname, remote_bin_path: &str, binary: &[u8]) -> Result<()> {
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
        .spawn()?;
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(binary)?;

    // Compute the expected shasum while sending.
    // SHA256 is 32 bytes, so this is the full hash.
    // TODO: We could compute it only once at startup and pass it here.
    let expected_hash = truncated_sha256(binary, 32);

    let output = child.wait_with_output()?.stdout;

    // Parse the hash out of a `sha256sum` output line (`<hash>  <filename>\n`).
    let make_err =
        || Error::SetupProtocolError("Failed to read sha256sum after installation.".into());
    let actual_hash = std::str::from_utf8(&output)
        .map_err(|_| make_err())?
        .split_whitespace()
        .next()
        .ok_or_else(make_err)?;

    if actual_hash != expected_hash {
        return Err(Error::SetupChecksumMismatch {
            actual_hash: actual_hash.into(),
            expected_hash,
        });
    }

    Ok(())
}

//! Binary installation on target hosts.

use crate::error::{Error, Result};

/// The number of hex characters of the sha256 digest used as the binary suffix.
const SUFFIX_LEN: usize = 10;

/// Compute the version suffix from the binary's sha256 digest.
pub fn binary_suffix(bytes: &[u8]) -> String {
    let digest = hmac_sha256::Hash::hash(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect::<String>()[..SUFFIX_LEN].to_string()
}

/// Compute the versioned binary name: `deptool-{version}-{suffix}`.
pub fn binary_name(version: &str, suffix: &str) -> String {
    format!("deptool-{version}-{suffix}")
}

/// Absolute path to the versioned binary on the target host.
pub fn remote_binary_path(name: &str) -> String {
    format!("/var/lib/deptool/bin/{name}")
}

/// Build the shell command that installs the binary on the target host.
///
/// The command reads the binary from stdin via `dd`, makes it executable,
/// symlinks it to `/usr/sbin/deptool`, and prints its sha256sum so the
/// caller can verify the transfer was successful.
pub fn install_command(remote_bin_path: &str) -> String {
    [
        "sudo mkdir -p /var/lib/deptool/{bin,apps,store}",
        &format!("sudo dd of={remote_bin_path}"),
        &format!("sudo chmod +x {remote_bin_path}"),
        &format!("sudo ln -sf {remote_bin_path} /usr/sbin/deptool"),
        &format!("sudo sha256sum {remote_bin_path}"),
    ]
    .join(" && ")
}

/// Parse the hash out of a `sha256sum` output line (`<hash>  <filename>\n`).
pub fn parse_sha256sum(output: &str) -> Result<String> {
    let hash = output
        .split_whitespace()
        .next()
        .ok_or_else(|| Error::SetupProtocolError("empty sha256sum output".into()))?;
    if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(hash.to_string())
    } else {
        Err(Error::SetupProtocolError(format!(
            "unexpected sha256sum token: {hash:?}"
        )))
    }
}

/// Install the deptool binary on a target host.
///
/// `run` receives the binary bytes as stdin and returns the stdout of the
/// install command (expected to contain the `sha256sum` line). The reported
/// hash is compared against the local binary's sha256.
pub fn install_binary(
    run: impl FnOnce(&[u8]) -> Result<Vec<u8>>,
    binary: &[u8],
    remote_bin_path: &str,
) -> Result<()> {
    let expected = binary_suffix(binary);
    let stdout = run(binary)?;
    let output = std::str::from_utf8(&stdout)
        .map_err(|_| Error::SetupProtocolError("sha256sum output is not utf-8".into()))?;
    let got = parse_sha256sum(output)?;
    // sha256sum prints the full 64-char hash; compare only the suffix prefix.
    if !got.starts_with(&expected) {
        return Err(Error::SetupChecksumMismatch { expected, got });
    }
    let _ = remote_bin_path; // used by the caller to build the command
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_suffix_is_first_10_hex_chars_of_sha256() {
        // sha256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        let suffix = binary_suffix(b"");
        assert_eq!(suffix, "e3b0c44298");
    }

    #[test]
    fn binary_name_formats_version_and_suffix() {
        assert_eq!(binary_name("0.1.0", "abc123"), "deptool-0.1.0-abc123");
    }

    #[test]
    fn remote_binary_path_is_under_var_lib() {
        assert_eq!(
            remote_binary_path("deptool-0.1.0-abc123"),
            "/var/lib/deptool/bin/deptool-0.1.0-abc123",
        );
    }

    #[test]
    fn install_command_joins_steps_with_and() {
        let cmd = install_command("/var/lib/deptool/bin/deptool-0.1.0-abc");
        // All steps must be &&-joined so failure aborts the chain.
        assert!(cmd.contains(" && "), "steps are &&-joined: {cmd}");
        assert!(!cmd.contains(" ; "), "no loose semicolons: {cmd}");
    }

    #[test]
    fn install_command_contains_required_operations() {
        let path = "/var/lib/deptool/bin/deptool-0.1.0-abc";
        let cmd = install_command(path);
        assert!(cmd.contains("mkdir -p"), "mkdir: {cmd}");
        assert!(cmd.contains(&format!("dd of={path}")), "dd: {cmd}");
        assert!(cmd.contains(&format!("chmod +x {path}")), "chmod: {cmd}");
        assert!(cmd.contains(&format!("ln -sf {path}")), "ln: {cmd}");
        assert!(cmd.contains(&format!("sha256sum {path}")), "sha256sum: {cmd}");
    }

    #[test]
    fn parse_sha256sum_extracts_hash_from_output() {
        let line = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  /some/path\n";
        let hash = parse_sha256sum(line).unwrap();
        assert_eq!(hash, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }

    #[test]
    fn parse_sha256sum_errors_on_empty_output() {
        assert!(matches!(
            parse_sha256sum(""),
            Err(Error::SetupProtocolError(_))
        ));
    }

    #[test]
    fn parse_sha256sum_errors_on_wrong_length_token() {
        assert!(matches!(
            parse_sha256sum("abc123  /path\n"),
            Err(Error::SetupProtocolError(_))
        ));
    }

    #[test]
    fn install_binary_succeeds_when_checksum_matches() {
        let binary = b"hello";
        // sha256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
        let stdout = b"2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824  /path\n";
        let result = install_binary(|_| Ok(stdout.to_vec()), binary, "/path");
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn install_binary_errors_when_checksum_mismatches() {
        let binary = b"hello";
        // Wrong hash (all zeros).
        let stdout = b"0000000000000000000000000000000000000000000000000000000000000000  /path\n";
        let result = install_binary(|_| Ok(stdout.to_vec()), binary, "/path");
        assert!(matches!(result, Err(Error::SetupChecksumMismatch { .. })));
    }

    #[test]
    fn install_binary_passes_binary_bytes_to_run() {
        let binary = b"my binary contents";
        let mut received = Vec::new();
        let _ = install_binary(
            |bytes| {
                received.extend_from_slice(bytes);
                // Return a matching hash so the function doesn't error on checksum.
                let digest: String = hmac_sha256::Hash::hash(bytes)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                Ok(format!("{digest}  /path\n").into_bytes())
            },
            binary,
            "/path",
        );
        assert_eq!(received, binary);
    }
}

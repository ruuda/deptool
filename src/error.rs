// Deptool -- A declarative configuration deployment tool.
// Copyright 2026 Ruud van Asseldonk

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// A copy of the License has been included in the root of the repository.

//! Error types and Result aliases.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::prim::Hostname;

/// An error from the Git store or its contents.
#[derive(Debug)]
pub enum StoreError {
    /// No store exists at the given path. Points the user at `deptool init`.
    NotFound(PathBuf),
    /// Git operation failure (libgit2).
    Git(git2::Error),
    /// Filesystem I/O failure.
    Io(std::io::Error),
    /// JSON parse or serialization failure.
    Json(serde_json::Error),
    /// A file name in the store is not valid UTF-8.
    NonUtf8FileName,
    /// A configuration value is structurally invalid.
    InvalidConfig(String),
}

impl From<git2::Error> for StoreError {
    fn from(e: git2::Error) -> Self {
        StoreError::Git(e)
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        StoreError::Json(e)
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            StoreError::NotFound(path) => write!(
                f,
                "no store at '{}'; run 'deptool init' to create one",
                path.display(),
            ),
            StoreError::Git(e) => write!(f, "{e}"),
            StoreError::Io(e) => write!(f, "{e}"),
            StoreError::Json(e) => write!(f, "{e}"),
            StoreError::NonUtf8FileName => write!(f, "non-utf8 file name"),
            StoreError::InvalidConfig(msg) => write!(f, "invalid config: {msg}"),
        }
    }
}

/// A failure on the agent during a deploy request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApplyError {
    /// Failed to create, remove, or verify a symlink.
    SymlinkFailed { link: String, cause: String },
    /// One or more systemd units failed to become active after apply.
    SystemdActivationFailed,
    /// systemd-sysusers failed to materialize declared system users.
    SysusersActivationFailed,
    /// A store operation failed during the apply phase.
    Store(String),
    /// An I/O error on the host.
    Io(String),
}

impl From<StoreError> for ApplyError {
    fn from(e: StoreError) -> Self {
        ApplyError::Store(e.to_string())
    }
}

impl From<std::io::Error> for ApplyError {
    fn from(e: std::io::Error) -> Self {
        ApplyError::Io(e.to_string())
    }
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ApplyError::SymlinkFailed { link, cause } => {
                write!(f, "cannot create symlink at {link}: {cause}")
            }
            ApplyError::SystemdActivationFailed => {
                write!(f, "one or more units failed to become active")
            }
            ApplyError::SysusersActivationFailed => {
                write!(f, "systemd-sysusers failed to create system users")
            }
            ApplyError::Store(msg) => write!(f, "{msg}"),
            ApplyError::Io(msg) => write!(f, "{msg}"),
        }
    }
}

/// A per-host failure during deployment.
/// Constructed on the driver side only; the status printer prefixes the
/// hostname, so variants don't need to include it.
#[derive(Debug)]
pub enum HostError {
    /// SSH or other transport-level connection failure.
    ConnectionFailed(String),
    /// The binary on the target host either isn't present, or it ran and
    /// exited with code 1 before sending a hello. The two cases are
    /// indistinguishable from the exit code alone (sudo exits 1 when the
    /// binary is missing, but a runtime error from the binary exits 1 too),
    /// so we carry the agent's stderr for diagnosis.
    AgentNotInstalled { stderr: String },
    /// The agent reported a different hostname than the driver expected.
    HostnameMismatch(String),
    /// The installed binary doesn't match the expected checksum.
    SetupChecksumMismatch {
        expected_hash: String,
        actual_hash: String,
    },
    /// No deptool binary for the host's platform in the local cache.
    SetupMissingBinary {
        /// Host's `uname -sm` output, e.g. "Linux x86_64".
        platform: String,
        path: PathBuf,
    },
    /// I/O error other than `NotFound` while reading a deptool binary
    /// from the local cache (e.g. permission denied, disk failure).
    /// Distinct from `SetupMissingBinary`, which is the expected
    /// "no such file" case with its own remediation hint.
    SetupReadError { path: PathBuf, cause: String },
    /// The install command finished without reporting the uploaded binary's
    /// checksum, so the transfer could not be verified. Usually the remote
    /// command chain failed before `sha256sum` ran (sudo denied, disk full,
    /// connection dropped). Carries the remote stderr for diagnosis.
    SetupNoChecksum { stderr: String },
    /// Unexpected or malformed message from the agent session.
    ProtocolError(String),
    /// A store operation failed.
    Store(StoreError),
    /// Agent error before any host modification (e.g. lock failure, pack
    /// write failure).
    PreApply(ApplyError),
    /// Apply failed; rollback wasn't attempted because the changes weren't
    /// rollback-safe.
    ApplyFailed(ApplyError),
    /// Apply failed and rollback also failed.
    RollbackFailed {
        apply_error: ApplyError,
        rollback_error: ApplyError,
    },
}

impl HostError {
    pub fn connection_failed(e: impl std::fmt::Display) -> Self {
        HostError::ConnectionFailed(e.to_string())
    }

    pub fn protocol_error(e: impl std::fmt::Display) -> Self {
        HostError::ProtocolError(e.to_string())
    }

    /// Long-form explanation, if this error class needs one. `host` is the
    /// hostname this error was raised for, so the item line can name it
    /// when the per-host detail isn't already in the variant.
    pub fn explain(&self, host: &str) -> Option<Explanation> {
        match self {
            HostError::SetupMissingBinary { platform, path } => Some(Explanation {
                description: format!(
                    "The agent runs on the target host, so Deptool needs a binary for the\n\
                     host's platform, built from the same source as your operator binary\n\
                     (Deptool {} at commit {}). Build or download such a binary \n\
                     for each platform below, and place it at the path shown:",
                    crate::protocol::VERSION,
                    &crate::setup::BUILD_COMMIT[..10],
                ),
                item: format!("  {platform}: {}", path.display()),
            }),
            HostError::HostnameMismatch(actual) => Some(Explanation {
                description: "The host's /etc/hostname differs from the name of the host in the config tree.\n\
                             Out of caution, we don't deploy to hosts when it's not clear that we connected\n\
                             to the intended target. Either change /etc/hostname on the target, or rename\n\
                             the host's directory in the config tree to match."
                    .to_string(),
                item: format!("  {host}: /etc/hostname contains {actual:?}"),
            }),
            HostError::SetupNoChecksum { stderr } => Some(Explanation {
                description: "The install command finished without reporting the uploaded binary's\n\
                             checksum, so the transfer could not be verified. Usually the remote\n\
                             command failed before it got that far. The host's stderr was:"
                    .to_string(),
                item: if stderr.is_empty() {
                    format!("  {host}: (the host produced no stderr)")
                } else {
                    format!("  {host}:\n    {}", stderr.replace('\n', "\n    "))
                },
            }),
            HostError::SetupChecksumMismatch {
                expected_hash,
                actual_hash,
            } => Some(Explanation {
                description: "After installing the deptool binary, the host's sha256 of the\n\
                             file on disk did not match the sha256 of what we uploaded."
                    .to_string(),
                item: format!("  {host}: expected {expected_hash}, got {actual_hash}"),
            }),
            HostError::ApplyFailed(apply_error) => Some(Explanation {
                description: "Apply failed and we did not roll back because the changes were not\n\
                              rollback-safe. The host is partially modified:"
                    .to_string(),
                item: format!("  {host}: {apply_error}"),
            }),
            HostError::RollbackFailed {
                apply_error,
                rollback_error,
            } => Some(Explanation {
                description: "Apply failed and the rollback we attempted also failed. The host is\n\
                             partially modified:"
                    .to_string(),
                item: [
                    format!("  {host}:"),
                    format!("    apply: {apply_error}"),
                    format!("    rollback: {rollback_error}"),
                ].join("\n"),
            }),
            _ => None,
        }
    }
}

/// Hosts that share a `description` group under it; their `item` lines list below.
pub struct Explanation {
    pub description: String,
    pub item: String,
}

impl From<StoreError> for HostError {
    fn from(e: StoreError) -> Self {
        HostError::Store(e)
    }
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            HostError::ConnectionFailed(msg) => write!(f, "{msg}"),
            HostError::AgentNotInstalled { stderr } if stderr.is_empty() => {
                write!(f, "binary not installed on target host")
            }
            HostError::AgentNotInstalled { stderr } => {
                write!(
                    f,
                    "binary not installed on target host, or exited before \
                     hello: {stderr}",
                )
            }
            HostError::HostnameMismatch(_) => f.write_str("hostname mismatch"),
            HostError::SetupChecksumMismatch { .. } => f.write_str("binary checksum mismatch"),
            HostError::SetupMissingBinary { platform, .. } => write!(
                f,
                "unable to deploy to {platform}, no binary for this platform"
            ),
            HostError::SetupReadError { path, cause } => {
                write!(f, "failed to read '{}': {cause}", path.display(),)
            }
            HostError::SetupNoChecksum { .. } => {
                f.write_str("install incomplete: no checksum from host")
            }
            HostError::ProtocolError(msg) => write!(f, "protocol error: {msg}"),
            HostError::Store(msg) => write!(f, "{msg}"),
            HostError::PreApply(err) => write!(f, "{err}"),
            HostError::ApplyFailed(_) => f.write_str("error during apply"),
            HostError::RollbackFailed { .. } => f.write_str("error during apply, rollback failed"),
        }
    }
}

/// Top-level errors for the deploy workflow and CLI.
#[derive(Debug)]
pub enum Error {
    /// An error from the store.
    Store(StoreError),
    /// Non-store I/O failure (terminal output, reading binary, etc.).
    Io(std::io::Error),
    /// Non-store JSON failure (agent protocol parsing).
    Json(serde_json::Error),
    /// No config tree was passed and none is recorded in the store.
    NoDefaultCluster,
    /// Hostnames passed via --limit don't appear in the cluster.
    UnknownHosts(Vec<Hostname>),
    /// One or more hosts failed during a deploy.
    DeployFailed(String),
}

impl From<StoreError> for Error {
    fn from(e: StoreError) -> Self {
        Error::Store(e)
    }
}

impl From<HostError> for Error {
    fn from(e: HostError) -> Self {
        Error::DeployFailed(e.to_string())
    }
}

impl From<ApplyError> for Error {
    fn from(e: ApplyError) -> Self {
        // ApplyError is a structured agent-side error; when it crosses
        // into the driver Error world we just stringify it.
        Error::DeployFailed(e.to_string())
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}

impl From<git2::Error> for Error {
    fn from(e: git2::Error) -> Self {
        Error::Store(StoreError::Git(e))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Store(e) => write!(f, "{e}"),
            Error::Io(e) => write!(f, "{e}"),
            Error::Json(e) => write!(f, "{e}"),
            Error::NoDefaultCluster => write!(
                f,
                "no default cluster; pass a config tree directory to set one",
            ),
            Error::UnknownHosts(hosts) => {
                let names: Vec<&str> = hosts.iter().map(|h| h.0.as_str()).collect();
                write!(f, "unknown hosts in --limit: {}", names.join(", "))
            }
            // TODO: Later we could also find the common ancestor `cc` between
            // what the host has (`current`) and what we try to deploy, and then
            // we can show the `git log cc..current` to point out the culprit,
            // especially including author timestamps and metadata.
            Error::DeployFailed(msg) => write!(f, "{msg}"),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explain_hostname_mismatch_names_host_and_reported_value() {
        let err = HostError::HostnameMismatch("spinner".into());
        let exp = err
            .explain("web1")
            .expect("hostname mismatch has explanation");
        assert!(exp.item.contains("web1"), "item names the expected host");
        assert!(
            exp.item.contains("spinner"),
            "item shows the reported hostname"
        );
    }
}

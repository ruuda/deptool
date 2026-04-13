//! Error types and Result aliases.

use std::fmt;

use serde::{Deserialize, Serialize};

/// An error from the Git store or its contents.
#[derive(Debug)]
pub enum StoreError {
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
    /// The agent binary is not present on the target host.
    AgentNotInstalled,
    /// The agent reported a different hostname than the driver expected.
    HostnameMismatch(String),
    /// The installed agent binary doesn't match the expected checksum.
    SetupChecksumMismatch {
        expected_hash: String,
        actual_hash: String,
    },
    /// Unexpected response during binary installation handshake.
    SetupProtocolError(String),
    /// Unexpected or malformed message from the agent session.
    ProtocolError(String),
    /// A store operation failed.
    Store(StoreError),
    /// The agent reported an error during the apply phase.
    Apply(ApplyError),
}

impl HostError {
    pub fn connection_failed(e: impl std::fmt::Display) -> Self {
        HostError::ConnectionFailed(e.to_string())
    }

    pub fn protocol_error(e: impl std::fmt::Display) -> Self {
        HostError::ProtocolError(e.to_string())
    }
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
            HostError::AgentNotInstalled => {
                write!(f, "agent binary not installed on target host")
            }
            HostError::HostnameMismatch(actual) => {
                write!(f, "hostname mismatch: /etc/hostname contains {actual:?}")
            }
            HostError::SetupChecksumMismatch {
                expected_hash,
                actual_hash,
            } => write!(
                f,
                "setup checksum mismatch: expected {expected_hash}, got {actual_hash}"
            ),
            HostError::SetupProtocolError(msg) => write!(f, "setup protocol error: {msg}"),
            HostError::ProtocolError(msg) => write!(f, "protocol error: {msg}"),
            HostError::Store(msg) => write!(f, "{msg}"),
            HostError::Apply(err) => write!(f, "{err}"),
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
    /// The deploy is not a fast-forward from the host's current state.
    Diverged(crate::prim::Hostname),
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
            // TODO: Later we could also find the common ancestor `cc` between
            // what the host has (`current`) and what we try to deploy, and then
            // we can show the `git log cc..current` to point out the culprit,
            // especially including author timestamps and metadata.
            Error::Diverged(host) => write!(
                f,
                "{host}: deploy is not a fast-forward. \
                 Pull the latest state, or run with --force-push to override."
            ),
            Error::DeployFailed(msg) => write!(f, "{msg}"),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

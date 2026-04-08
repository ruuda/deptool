//! Error type and Result alias.

use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// Filesystem I/O failure.
    Io(std::io::Error),
    /// Git operation failure (libgit2).
    Git(git2::Error),
    /// JSON parse or serialization failure.
    Json(serde_json::Error),
    /// A file name in the store is not valid UTF-8.
    NonUtf8FileName,
    /// A configuration value is structurally invalid (store content or CLI args).
    InvalidConfig(String),
    /// The agent binary is not present on the target host.
    AgentNotInstalled,
    /// SSH or other transport-level connection failure.
    ConnectionFailed(String),
    /// The deploy is not a fast-forward from the host's current state.
    Diverged(crate::prim::Hostname),
    /// One or more hosts failed during a deploy.
    DeployFailed(String),
    /// A runtime failure on the target host during the apply phase.
    AgentError(String),
    /// The installed agent binary doesn't match the expected checksum.
    SetupChecksumMismatch {
        expected_hash: String,
        actual_hash: String,
    },
    /// Unexpected response during binary installation handshake.
    SetupProtocolError(String),
    /// Unexpected or malformed message from the agent session.
    ProtocolError(String),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<git2::Error> for Error {
    fn from(e: git2::Error) -> Self {
        Error::Git(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Git(e) => write!(f, "{e}"),
            Error::Json(e) => write!(f, "{e}"),
            Error::NonUtf8FileName => write!(f, "non-utf8 file name"),
            Error::AgentNotInstalled => write!(f, "agent binary not installed on target host"),
            Error::ConnectionFailed(msg) => write!(f, "{msg}"),
            // TODO: Later we could also find the common ancestor `cc` between
            // what the host has (`current`) and what we try to deploy, and then
            // we can show the `git log cc..current` to point out the culprit,
            // especially including author timestamps and metadata.
            Error::Diverged(host) => write!(
                f,
                "{host}: deploy is not a fast-forward. \
                 Pull the latest state, or run with --force-push to override."
            ),
            Error::InvalidConfig(msg) => write!(f, "invalid config: {msg}"),
            Error::DeployFailed(msg) => write!(f, "{msg}"),
            Error::AgentError(msg) => write!(f, "{msg}"),
            Error::SetupChecksumMismatch {
                expected_hash,
                actual_hash,
            } => {
                write!(
                    f,
                    "setup checksum mismatch: expected {expected_hash}, got {actual_hash}"
                )
            }
            Error::SetupProtocolError(msg) => write!(f, "setup protocol error: {msg}"),
            Error::ProtocolError(msg) => write!(f, "protocol error: {msg}"),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

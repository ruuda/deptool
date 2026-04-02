//! Error type and Result alias.

use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Git(git2::Error),
    Json(serde_json::Error),
    NonUtf8FileName,
    InvalidConfig(String),
    AgentNotInstalled,
    SetupChecksumMismatch {
        expected_hash: String,
        actual_hash: String,
    },
    SetupProtocolError(String),
    ProtocolError(String),
    #[cfg(test)]
    GitPush {
        remote_url: String,
        message: String,
    },
    GitFetch {
        remote_url: String,
        message: String,
    },
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
            Error::InvalidConfig(msg) => write!(f, "invalid config: {msg}"),
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
            #[cfg(test)]
            Error::GitPush {
                remote_url,
                message,
            } => write!(f, "git push to {remote_url} failed: {message}"),
            Error::GitFetch {
                remote_url,
                message,
            } => write!(f, "git fetch from {remote_url} failed: {message}"),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

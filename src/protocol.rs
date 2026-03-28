//! Request and response types for the host session protocol.

use serde::{Deserialize, Serialize};

use crate::oid::Oid;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Apply {
        expected_current_commit: Option<Oid>,
        target_commit: Oid,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Message {
    Hello {
        version: String,
        hostname: String,
    },
    AppliedApp {
        app: String,
        diff: crate::plan::AppDiff,
    },
    ApplyComplete {
        commit: Oid,
    },
    Stale {
        expected_commit: Option<Oid>,
        actual_commit: Option<Oid>,
    },
    Error {
        message: String,
    },
}

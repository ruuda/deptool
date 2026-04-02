//! Request and response types for the host session protocol.

use serde::{Deserialize, Serialize};

use crate::prim::Oid;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Request {
    /// Acquire the deploy lock and verify the host's current commit.
    Lock {
        expected_current_commit: Option<Oid>,
    },
    Apply {
        expected_current_commit: Option<Oid>,
        target_commit: Oid,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hello {
    pub version: String,
    pub hostname: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    Locked,
    LockStale {
        expected_commit: Option<Oid>,
        actual_commit: Option<Oid>,
    },
    LockBusy,
    AppliedApp {
        app: String,
        diff: crate::plan::AppDiff,
    },
    ApplyComplete {
        commit: Oid,
        enabled_units: Vec<String>,
        restarted_units: Vec<String>,
        disabled_units: Vec<String>,
    },
    Stale {
        expected_commit: Option<Oid>,
        actual_commit: Option<Oid>,
    },
    Error {
        message: String,
    },
}

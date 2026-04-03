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
    /// Receive a base64-encoded packfile into the store.
    ReceivePack {
        pack_data: String,
    },
    /// Request a packfile containing the host's current commit.
    RequestObjects {
        have_commit: Option<Oid>,
    },
    Apply {
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
    PackReceived,
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
    /// A base64-encoded packfile in response to `RequestObjects`.
    SendPack {
        pack_data: String,
    },
    Error {
        message: String,
    },
}

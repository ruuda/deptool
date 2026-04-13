//! Request and response types for the host session protocol.

use git2::Oid;
use serde::{Deserialize, Serialize};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Request {
    /// Acquire the deploy lock and verify the host's current commit.
    Lock {
        #[serde(with = "crate::prim::ser::oid_option")]
        expected_current_commit: Option<Oid>,
        /// Who initiated the deploy (e.g. "deckard@spinner").
        operator: String,
    },
    /// Receive a base64-encoded packfile into the store.
    ReceivePack { pack_data: String },
    /// Request a packfile containing the host's current commit.
    RequestObjects {
        #[serde(with = "crate::prim::ser::oid_option")]
        have_commit: Option<Oid>,
    },
    Apply {
        #[serde(with = "crate::prim::ser::oid")]
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
        #[serde(with = "crate::prim::ser::oid_option")]
        expected_commit: Option<Oid>,
        #[serde(with = "crate::prim::ser::oid_option")]
        actual_commit: Option<Oid>,
    },
    LockBusy {
        /// Who currently holds the lock, if known.
        held_by: Option<String>,
    },
    /// Output of `systemctl status` for systemd units that were just changed.
    SystemdUnitStatus {
        output: String,
    },
    AppliedApp {
        app: String,
        diff: crate::plan::AppDiff,
    },
    ApplyComplete {
        #[serde(with = "crate::prim::ser::oid")]
        commit: Oid,
        enabled_units: Vec<String>,
        restarted_units: Vec<String>,
        disabled_units: Vec<String>,
    },
    /// A base64-encoded packfile in response to `RequestObjects`.
    SendPack {
        pack_data: String,
    },
    Error(crate::error::ApplyError),
}

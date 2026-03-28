//! Serializable newtype for Git object ids.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A Git object id as a hex string. Serializes cleanly and converts to/from `git2::Oid`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Oid(String);

impl From<git2::Oid> for Oid {
    fn from(oid: git2::Oid) -> Self {
        Oid(oid.to_string())
    }
}

impl From<&str> for Oid {
    fn from(s: &str) -> Self {
        Oid(s.to_string())
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}

//! Primitive newtypes: Git object ids and hostnames.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A Git object id as a hex string. Serializes cleanly and converts to/from `git2::Oid`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Oid(String);

// TODO: We use both this and git2::Oid extensively across the codebase and it
// gets messy. Is it possible to just pick one and be consistent, or at least
// push all conversions into a single module?

impl From<git2::Oid> for Oid {
    fn from(oid: git2::Oid) -> Self {
        Oid(oid.to_string())
    }
}

impl From<&Oid> for git2::Oid {
    fn from(oid: &Oid) -> Self {
        git2::Oid::from_str(&oid.0).expect("Oid wrapper contains valid oids.")
    }
}

impl From<Oid> for git2::Oid {
    fn from(oid: Oid) -> Self {
        git2::Oid::from_str(&oid.0).expect("Oid wrapper contains valid oids.")
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

/// A hostname, as a newtype over `String`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Hostname(pub String);

impl From<&str> for Hostname {
    fn from(s: &str) -> Self {
        Hostname(s.to_string())
    }
}

impl From<String> for Hostname {
    fn from(s: String) -> Self {
        Hostname(s)
    }
}

impl fmt::Display for Hostname {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}

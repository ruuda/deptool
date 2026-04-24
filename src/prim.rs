// Deptool -- A declarative configuration deployment tool.
// Copyright 2026 Ruud van Asseldonk

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// A copy of the License has been included in the root of the repository.

//! Primitive newtypes and serde helpers.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Serde helpers for types that don't implement Serialize/Deserialize.
///
/// Used via `#[serde(with = "crate::prim::ser::oid")]` on struct fields.
pub mod ser {
    pub mod oid {
        use ::serde::{Deserialize, Deserializer, Serialize, Serializer};

        pub fn serialize<S: Serializer>(oid: &git2::Oid, s: S) -> Result<S::Ok, S::Error> {
            oid.to_string().serialize(s)
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<git2::Oid, D::Error> {
            let hex = String::deserialize(d)?;
            git2::Oid::from_str(&hex).map_err(::serde::de::Error::custom)
        }
    }

    pub mod oid_option {
        use ::serde::{Deserialize, Deserializer, Serializer};

        pub fn serialize<S: Serializer>(oid: &Option<git2::Oid>, s: S) -> Result<S::Ok, S::Error> {
            match oid {
                Some(oid) => s.serialize_some(&oid.to_string()),
                None => s.serialize_none(),
            }
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<git2::Oid>, D::Error> {
            let hex: Option<String> = Option::deserialize(d)?;
            match hex {
                Some(h) => git2::Oid::from_str(&h)
                    .map(Some)
                    .map_err(::serde::de::Error::custom),
                None => Ok(None),
            }
        }
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

/// Read the local machine's hostname from `/etc/hostname`.
pub fn read_hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .unwrap_or_else(|_| "unknown".into())
        .trim()
        .to_string()
}

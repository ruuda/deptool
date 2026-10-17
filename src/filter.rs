// Deptool -- A declarative configuration deployment tool.
// Copyright 2026 Ruud van Asseldonk

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// A copy of the License has been included in the root of the repository.

//! Selection of the hosts a command operates on, from `--limit`.

use crate::error::{Error, Result};
use crate::prim::Hostname;

pub enum HostFilter {
    All,
    Only(Vec<Hostname>),
}

impl HostFilter {
    /// Build a filter from repeated --limit values.
    ///
    /// We accept both repetition (`--limit a --limit b`) and comma-separated
    /// values (`--limit a,b`), with append semantics. Empty pieces from a
    /// stray comma (`--limit a,`) are dropped. No `--limit` at all yields
    /// `HostFilter::All`; a `--limit` that lists no hosts (an empty value, or
    /// only commas) is an error, since selecting zero hosts is never intended.
    pub fn from_limit(args: &[String]) -> Result<Self> {
        // Distinguish no --limit (All) from a --limit that resolved to nothing.
        if args.is_empty() {
            return Ok(HostFilter::All);
        }
        let hosts: Vec<Hostname> = args
            .iter()
            .flat_map(|s| s.split(','))
            .filter(|s| !s.is_empty())
            .map(Hostname::from)
            .collect();
        if hosts.is_empty() {
            Err(Error::EmptyLimit)
        } else {
            Ok(HostFilter::Only(hosts))
        }
    }

    pub fn includes(&self, host: &Hostname) -> bool {
        match self {
            HostFilter::All => true,
            HostFilter::Only(limit) => limit.contains(host),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_limit_rejects_a_limit_that_lists_no_hosts() {
        // An empty value or only commas must error, not widen the blast
        // radius to every host.
        assert!(HostFilter::from_limit(&["".into()]).is_err());
        assert!(HostFilter::from_limit(&[",".into()]).is_err());
    }

    #[test]
    fn from_limit_maps_absent_and_present_values() {
        assert!(matches!(HostFilter::from_limit(&[]), Ok(HostFilter::All)));
        let Ok(HostFilter::Only(hosts)) = HostFilter::from_limit(&["web1,".into()]) else {
            panic!("expected Only")
        };
        assert_eq!(hosts, vec![Hostname::from("web1")]);
    }
}

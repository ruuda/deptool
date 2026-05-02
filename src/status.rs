// Deptool -- A declarative configuration deployment tool.
// Copyright 2026 Ruud van Asseldonk

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// A copy of the License has been included in the root of the repository.

//! The `deptool status` command: per-host deployment status, computed offline.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use git2::{Oid, Time};

use crate::display::{UseColor, format_git_time};
use crate::error::Result;
use crate::plan::{HostFilter, diff_apps};
use crate::prim::Hostname;
use crate::store::{Store, tree_entries};

/// State of one host as known by the local store, without contacting it.
#[derive(Debug)]
pub enum HostState {
    /// Host appears in the config tree but has no tracking ref yet.
    NewHost,
    /// Tracking ref's commit tree matches the config tree for this host.
    UpToDate { commit: Oid, time: Time },
    /// Tracking ref exists but the config tree changes some apps.
    HasChanges {
        commit: Oid,
        time: Time,
        apps: Vec<String>,
    },
}

/// Compute per-host status by comparing the config tree to tracking refs.
///
/// Reads the local store only. `filter` narrows the set of hosts; any limit
/// name not in the tree is reported as an error before comparison.
pub fn compute_status(
    store: &Store,
    dir: &Path,
    filter: &HostFilter,
) -> Result<BTreeMap<Hostname, HostState>> {
    let tree_oid = store.build_tree(dir)?;
    let mut config_hosts = store.host_trees(tree_oid)?;
    filter.apply(&mut config_hosts)?;
    let host_names: Vec<Hostname> = config_hosts.keys().cloned().collect();
    let host_refs = store.host_tracking_refs(&host_names)?;

    let mut result = BTreeMap::new();
    for (host, target_host_tree) in config_hosts {
        let state = match host_refs.get(&host) {
            None => HostState::NewHost,
            Some(host_ref) => {
                let current_apps = tree_entries(&store.repo.find_tree(host_ref.host_tree)?);
                let target_apps = tree_entries(&store.repo.find_tree(target_host_tree)?);
                let app_diffs = diff_apps(&current_apps, &target_apps);
                let commit = store.repo.find_commit(host_ref.commit)?;
                // Committer time tracks when the commit was actually written,
                // which for Deptool is when this host's deploy was attempted.
                let time = commit.committer().when();
                if app_diffs.is_empty() {
                    HostState::UpToDate {
                        commit: host_ref.commit,
                        time,
                    }
                } else {
                    HostState::HasChanges {
                        commit: host_ref.commit,
                        time,
                        apps: app_diffs.into_keys().collect(),
                    }
                }
            }
        };
        result.insert(host, state);
    }
    Ok(result)
}

/// Smallest abbreviation length that's unambiguous in the object database
/// for every commit in `states`. Use to pick a uniform SHA column width.
pub fn min_unambiguous_short_len(
    store: &Store,
    states: &BTreeMap<Hostname, HostState>,
) -> Result<usize> {
    let mut max_len = 0;
    for state in states.values() {
        let oid = match state {
            HostState::UpToDate { commit, .. } | HostState::HasChanges { commit, .. } => *commit,
            HostState::NewHost => continue,
        };
        let buf = store.repo.find_object(oid, None)?.short_id()?;
        max_len = max_len.max(buf.as_str().expect("short_id is utf-8 hex").len());
    }
    Ok(max_len)
}

/// Write per-host status as one line per host, in input (sorted) order.
///
/// `short_len` is the number of hex characters to display for each commit
/// (typically from [`min_unambiguous_short_len`]).
pub fn print_status(
    out: &mut impl Write,
    states: &BTreeMap<Hostname, HostState>,
    short_len: usize,
    color: UseColor,
) -> Result<()> {
    let short = |c: &Oid| {
        let mut s = c.to_string();
        s.truncate(short_len);
        s
    };
    for (host, state) in states {
        let yellow_host = color.yellow(&host.to_string());
        match state {
            HostState::NewHost => {
                writeln!(out, "{yellow_host} {}", color.yellow("new host"))?;
            }
            HostState::UpToDate { commit, time } => {
                writeln!(
                    out,
                    "{yellow_host} {} {}",
                    format_git_time(*time),
                    color.blue(&short(commit)),
                )?;
            }
            HostState::HasChanges { commit, time, apps } => {
                let label = format!("undeployed changes: {}", apps.join(", "));
                writeln!(
                    out,
                    "{yellow_host} {} {} {}",
                    format_git_time(*time),
                    color.blue(&short(commit)),
                    color.red(&label),
                )?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{TestRepo, config_with};

    fn host_state<'a>(states: &'a BTreeMap<Hostname, HostState>, host: &str) -> &'a HostState {
        states
            .get(&Hostname::from(host))
            .expect("host is in the status map")
    }

    fn example_oid() -> Oid {
        Oid::from_str("0a89f71cafafa7b99ead436fef7981583dc040be").expect("valid hex")
    }

    /// 2024-04-27 14:24:43 UTC, recorded in +0200.
    fn example_time() -> Time {
        Time::new(1714227883, 120)
    }

    fn render_status(states: &BTreeMap<Hostname, HostState>, short_len: usize) -> String {
        let mut out = Vec::new();
        print_status(&mut out, states, short_len, UseColor::No).expect("write succeeds");
        String::from_utf8(out).expect("output is utf-8")
    }

    #[test]
    fn compute_status_classifies_each_host_by_its_tracking_ref() -> Result<()> {
        let driver = TestRepo::new();

        // host-a: tracking ref content matches config -> up to date.
        let c_a = driver.commit(&[("a/app/conf", b"v1")]);
        driver.set_host_tracking_ref("a", c_a);

        // host-b: nginx differs, caddy matches -> has changes (nginx only).
        let c_b = driver.commit(&[
            ("b/nginx/conf", b"v1"),
            ("b/caddy/conf", b"v1"),
        ]);
        driver.set_host_tracking_ref("b", c_b);

        // host-c: no tracking ref -> new host.

        let config = config_with(&[
            ("a/app/conf", b"v1"),
            ("b/nginx/conf", b"v2"),
            ("b/caddy/conf", b"v1"),
            ("c/app/conf", b"v1"),
        ]);

        let states = compute_status(&driver.store, config.path(), &HostFilter::All)?;
        assert!(matches!(host_state(&states, "a"), HostState::UpToDate { .. }));
        assert!(matches!(host_state(&states, "c"), HostState::NewHost));
        match host_state(&states, "b") {
            HostState::HasChanges { apps, .. } => {
                assert_eq!(apps, &vec!["nginx".to_string()]);
            }
            other => panic!("expected HasChanges, got {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn print_status_renders_one_line_per_host_with_distinct_states() {
        let mut states = BTreeMap::new();
        states.insert(
            Hostname::from("a.example.com"),
            HostState::UpToDate {
                commit: example_oid(),
                time: example_time(),
            },
        );
        states.insert(
            Hostname::from("b.example.com"),
            HostState::HasChanges {
                commit: example_oid(),
                time: example_time(),
                apps: vec!["nginx".to_string()],
            },
        );
        states.insert(
            Hostname::from("c.example.com"),
            HostState::HasChanges {
                commit: example_oid(),
                time: example_time(),
                apps: vec!["caddy".to_string(), "nginx".to_string()],
            },
        );
        states.insert(Hostname::from("d.example.com"), HostState::NewHost);

        assert_eq!(
            render_status(&states, 7),
            "\
a.example.com 2024-04-27 16:24:43 +0200 0a89f71
b.example.com 2024-04-27 16:24:43 +0200 0a89f71 undeployed changes: nginx
c.example.com 2024-04-27 16:24:43 +0200 0a89f71 undeployed changes: caddy, nginx
d.example.com new host
",
        );
    }
}

// Deptool -- A declarative configuration deployment tool.
// Copyright 2026 Ruud van Asseldonk

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// A copy of the License has been included in the root of the repository.

//! The `deptool sync` command: refresh tracking refs from remote hosts.

use std::collections::BTreeMap;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use git2::Oid;
use parking_lot::Mutex;

use crate::deploy::{self, DeployProgress, HostState, StaleHost};
use crate::error::{HostError, Result};
use crate::plan::HostFilter;
use crate::prim::Hostname;
use crate::protocol::{Message, Request};
use crate::setup::HostConnector;
use crate::store::{RefUpdate, Store};

#[derive(Clone, Debug)]
pub enum SyncMode {
    /// Only sync hosts whose config tree differs from the tracking ref.
    OnlyAffectedHosts,
    /// Sync all hosts in the config tree.
    AllHosts,
}

/// Pick hosts to sync from `dir` and look up each one's expected commit.
///
/// Returns the hosts to contact paired with the commit our tracking refs
/// say they should be at. `None` means we have no tracking ref for the
/// host, so any current commit is "new" from our point of view. `filter`
/// narrows the candidate set; any limit name not in the tree is reported
/// as an error before any host is contacted.
pub fn select_hosts_to_sync(
    store: &Store,
    dir: &Path,
    mode: SyncMode,
    filter: &HostFilter,
) -> Result<BTreeMap<Hostname, Option<Oid>>> {
    let tree_oid = store.build_tree(dir)?;
    let mut config_hosts = store.host_trees(tree_oid)?;
    filter.apply(&mut config_hosts)?;
    let host_names: Vec<Hostname> = config_hosts.keys().cloned().collect();
    let host_refs = store.host_tracking_refs(&host_names)?;

    let mut to_sync = BTreeMap::new();
    for (host, host_tree) in config_hosts {
        let host_ref = host_refs.get(&host);
        let needs_sync = match mode {
            SyncMode::AllHosts => true,
            SyncMode::OnlyAffectedHosts => match host_ref {
                Some(hr) => hr.host_tree != host_tree,
                None => true,
            },
        };
        if needs_sync {
            to_sync.insert(host, host_ref.map(|hr| hr.commit));
        }
    }
    Ok(to_sync)
}

/// Connect to hosts and refresh their tracking refs.
pub fn run_sync(
    store: &Store,
    hosts: &BTreeMap<Hostname, Option<Oid>>,
    connector: &dyn HostConnector,
    progress: &DeployProgress,
) {
    // Connect to all hosts in parallel, checking Hello for staleness.
    // A host is stale whenever its current commit differs from the tracking
    // ref, including the asymmetric cases: a fresh host (`actual = None`)
    // with a stale tracking ref still needs to be reconciled, and that's
    // how we recover from a reprovisioned host.
    let stale: Mutex<Vec<(Hostname, StaleHost)>> = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for (host, expected) in hosts {
            let stale = &stale;
            let expected = *expected;
            s.spawn(move || {
                let conn = match deploy::try_connect(host, connector, progress) {
                    Some(c) => c,
                    None => return,
                };
                let actual = conn.hello().current_commit;
                if actual == expected {
                    progress.update(host, HostState::UpToDate);
                } else {
                    progress.update(host, HostState::Stale);
                    stale.lock().push((
                        host.clone(),
                        StaleHost {
                            expected_commit: expected,
                            actual_commit: actual,
                            connection: conn,
                        },
                    ));
                }
            });
        }
    });

    // Fetch sequentially: the first fetch may provide objects that later
    // hosts also need, avoiding redundant transfers.
    for (host, mut info) in stale.into_inner() {
        match fetch_from_stale_host(store, &host, &mut info) {
            Ok(()) => progress.update(&host, HostState::Updated),
            Err(err) => progress.update(&host, err),
        }
    }
}

/// Reconcile each stale host's tracking ref with its current commit.
///
/// For a host that has a current commit we don't already have, fetches a
/// pack from its still-open session and points the tracking ref at it. For
/// a host that reports no current commit, deletes the tracking ref. Reports
/// per-host errors via progress.
pub fn fetch_stale_objects(
    store: &Store,
    stale: &mut BTreeMap<Hostname, StaleHost>,
    progress: &DeployProgress,
) {
    for (host, info) in stale.iter_mut() {
        match fetch_from_stale_host(store, host, info) {
            Ok(()) => {}
            Err(err) => progress.update(host, err),
        }
    }
}

fn fetch_from_stale_host(
    store: &Store,
    host: &Hostname,
    info: &mut StaleHost,
) -> std::result::Result<(), HostError> {
    let refname = format!("refs/remotes/{host}/current");

    let actual_commit = match info.actual_commit {
        Some(c) => c,
        // Host has no current commit -- a reprovisioned or wiped host. Drop
        // the tracking ref so the next plan treats this host as fresh;
        // without this step every subsequent deploy would abort the same way
        // and never make progress.
        None => {
            store.delete_ref(&refname)?;
            return Ok(());
        }
    };

    // Fetch the pack if we don't already have this commit.
    if store.repo.find_commit(actual_commit).is_err() {
        info.connection.send_request(&Request::RequestObjects {
            have_commit: info.expected_commit,
        })?;

        match info.connection.read_message()? {
            Some(Message::SendPack { pack_data }) => {
                let bytes = BASE64
                    .decode(&pack_data)
                    .expect("SendPack contains valid base64");
                store.write_pack(&bytes)?;
            }
            Some(Message::ErrorPreApply(apply_err)) => {
                return Err(HostError::PreApply(apply_err));
            }
            other => {
                return Err(HostError::ProtocolError(format!(
                    "unexpected response to RequestObjects: {other:?}"
                )));
            }
        }
    }

    store.set_ref(&refname, actual_commit, RefUpdate::FetchStale)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Result;
    use crate::testutil::{TempDir, TestHost, TestRepo, test_connector, test_progress};

    /// Run sync end-to-end and return the progress so tests can inspect state.
    fn sync_affected(
        driver: &TestRepo,
        hosts: &[&TestHost],
        config: &Path,
    ) -> Result<DeployProgress> {
        let to_sync = select_hosts_to_sync(
            &driver.store,
            config,
            SyncMode::OnlyAffectedHosts,
            &HostFilter::All,
        )?;
        let names: Vec<&str> = to_sync.keys().map(|h| h.0.as_str()).collect();
        let progress = test_progress(&names);
        let connector = test_connector(hosts);
        run_sync(&driver.store, &to_sync, &connector, &progress);
        Ok(progress)
    }

    /// Create a temp dir populated with the given files (with parent dirs).
    fn config_with(files: &[(&str, &[u8])]) -> TempDir {
        let dir = TempDir::new("config");
        for (path, content) in files {
            let full = dir.path().join(path);
            let parent = full.parent().expect("path has a parent dir");
            std::fs::create_dir_all(parent).expect("parent dir is created");
            std::fs::write(&full, content).expect("file is written");
        }
        dir
    }

    #[test]
    fn sync_updates_stale_tracking_ref() -> Result<()> {
        // Host has c2 deployed, but the driver only knows about c1.
        let driver = TestRepo::new();
        let c1 = driver.commit(&[("web1/app/conf", b"v1")]);
        let c2 = driver.commit(&[("web1/app/conf", b"v2")]);
        let host = TestHost::at_commit(&driver, "web1", c2);
        driver.set_host_tracking_ref("web1", c1);

        let config = config_with(&[("web1/app/conf", b"v3")]);
        let progress = sync_affected(&driver, &[&host], config.path())?;

        assert_eq!(
            driver.get_host_tracking_ref("web1"),
            Some(c2),
            "tracking ref updated to host's current commit",
        );
        assert!(
            matches!(*progress.state("web1"), HostState::Updated),
            "stale host reaches terminal Updated state after sync",
        );
        Ok(())
    }

    #[test]
    fn sync_marks_host_up_to_date_when_commit_matches_tracking_ref() -> Result<()> {
        // Host's current commit matches the tracking ref, but the config has
        // new content -- so the host is in to_sync, yet there's nothing to
        // fetch. UpToDate (not Updated) reports that the ref didn't move.
        let driver = TestRepo::new();
        let c1 = driver.commit(&[("web1/app/conf", b"v1")]);
        let host = TestHost::at_commit(&driver, "web1", c1);
        driver.set_host_tracking_ref("web1", c1);

        let config = config_with(&[("web1/app/conf", b"v2")]);
        let progress = sync_affected(&driver, &[&host], config.path())?;

        assert!(
            matches!(*progress.state("web1"), HostState::UpToDate),
            "host already at tracking ref commit reaches UpToDate state",
        );
        Ok(())
    }

    #[test]
    fn sync_with_limit_includes_only_listed_hosts() -> Result<()> {
        let driver = TestRepo::new();
        let config = config_with(&[
            ("web1/app/conf", b"a"),
            ("web2/app/conf", b"b"),
            ("web3/app/conf", b"c"),
        ]);
        let filter = HostFilter::Only(vec!["web1".into(), "web3".into()]);
        let to_sync =
            select_hosts_to_sync(&driver.store, config.path(), SyncMode::AllHosts, &filter)?;
        assert_eq!(
            to_sync.keys().cloned().collect::<Vec<_>>(),
            vec![Hostname::from("web1"), Hostname::from("web3")],
            "limit narrows the sync set to exactly the listed hosts",
        );
        Ok(())
    }

    #[test]
    fn sync_clears_tracking_ref_when_host_has_no_current_commit() -> Result<()> {
        // Models a reprovisioned host: the driver still has a tracking ref
        // from before, but the host's `refs/heads/current` is gone.
        let driver = TestRepo::new();
        let c1 = driver.commit(&[("web1/app/conf", b"v1")]);
        driver.set_host_tracking_ref("web1", c1);
        let host = TestHost::new("web1"); // No current commit on the host.

        let config = config_with(&[("web1/app/conf", b"v2")]);
        let progress = sync_affected(&driver, &[&host], config.path())?;

        assert_eq!(
            driver.get_host_tracking_ref("web1"),
            None,
            "tracking ref deleted because host has no current commit",
        );
        assert!(
            matches!(*progress.state("web1"), HostState::Updated),
            "stale host reaches terminal Updated state after deleting ref",
        );
        Ok(())
    }
}

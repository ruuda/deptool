// Deptool -- A declarative configuration deployment tool.
// Copyright 2026 Ruud van Asseldonk

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// A copy of the License has been included in the root of the repository.

//! The `deptool sync` command: refresh tracking refs from remote hosts.

use std::path::Path;

use parking_lot::Mutex;

use std::collections::BTreeMap;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

use crate::deploy::{self, DeployObserver, DeployProgress, HostState, StaleHost};
use crate::error::{HostError, Result};
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

/// Connect to hosts and refresh their tracking refs.
pub fn run_sync(
    store: &Store,
    dir: &Path,
    connector: &dyn HostConnector,
    mode: SyncMode,
    observer: Box<dyn DeployObserver>,
) -> Result<()> {
    let tree_oid = store.build_tree(dir)?;
    let config_hosts = store.host_trees(tree_oid)?;

    let host_names: Vec<Hostname> = config_hosts.keys().cloned().collect();
    let host_refs = store.host_tracking_refs(&host_names)?;

    let hosts_to_sync: Vec<Hostname> = match mode {
        SyncMode::AllHosts => host_names,
        SyncMode::OnlyAffectedHosts => host_names
            .into_iter()
            .filter(|host| match host_refs.get(host) {
                Some(hr) => hr.host_tree != config_hosts[host],
                None => true,
            })
            .collect(),
    };

    if hosts_to_sync.is_empty() {
        eprintln!("All hosts are up to date.");
        return Ok(());
    }
    let progress = &DeployProgress::new(hosts_to_sync.clone(), observer);

    // Connect to all hosts in parallel, checking Hello for staleness.
    // A host is stale whenever its current commit differs from the tracking
    // ref, including the asymmetric cases: a fresh host (`actual = None`)
    // with a stale tracking ref still needs to be reconciled, and that's
    // how we recover from a reprovisioned host.
    let stale: Mutex<Vec<(Hostname, StaleHost)>> = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for host in &hosts_to_sync {
            let stale = &stale;
            let expected = host_refs.get(host).map(|hr| hr.commit);
            s.spawn(move || {
                let conn = match deploy::try_connect(host, connector, progress) {
                    Some(c) => c,
                    None => return,
                };
                let actual = conn.hello().current_commit;
                if actual == expected {
                    progress.update(host, HostState::Done);
                } else {
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
    let mut stale_hosts = stale.into_inner().into_iter().collect();
    fetch_stale_objects(store, &mut stale_hosts, progress);

    Ok(())
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
    use crate::testutil::{NoopObserver, TempDir, TestHost, TestRepo, test_connector};

    #[test]
    fn sync_updates_stale_tracking_ref() -> Result<()> {
        let driver = TestRepo::new();
        let c1 = driver.commit(&[("web1/app/conf", b"v1")]);
        let c2 = driver.commit(&[("web1/app/conf", b"v2")]);

        // Host has c2 deployed, but the driver only knows about c1.
        let host = TestHost::new("web1");
        let pack = driver.store.create_pack(c2, None)?;
        host.session.store.write_pack(&pack)?;
        host.set_current(c2);
        driver.set_host_tracking_ref("web1", c1);

        // Create a config dir with changes so the host looks affected.
        let config = TempDir::new("config");
        std::fs::create_dir_all(config.path().join("web1/app")).unwrap();
        std::fs::write(config.path().join("web1/app/conf"), b"v3").unwrap();

        let connector = test_connector(&[&host]);
        run_sync(
            &driver.store,
            config.path(),
            &connector,
            SyncMode::OnlyAffectedHosts,
            Box::new(NoopObserver),
        )?;

        assert_eq!(
            driver.get_host_tracking_ref("web1"),
            Some(c2),
            "tracking ref updated to host's current commit",
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

        let config = TempDir::new("config");
        std::fs::create_dir_all(config.path().join("web1/app")).unwrap();
        std::fs::write(config.path().join("web1/app/conf"), b"v2").unwrap();

        let connector = test_connector(&[&host]);
        run_sync(
            &driver.store,
            config.path(),
            &connector,
            SyncMode::OnlyAffectedHosts,
            Box::new(NoopObserver),
        )?;

        assert_eq!(
            driver.get_host_tracking_ref("web1"),
            None,
            "tracking ref deleted because host has no current commit",
        );
        Ok(())
    }
}

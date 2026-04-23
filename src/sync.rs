//! The `deptool sync` command: refresh tracking refs from remote hosts.

use std::path::Path;

use git2::Oid;
use parking_lot::Mutex;

use std::collections::BTreeMap;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

use crate::deploy::{self, Connection, DeployProgress, HostState, StaleHost};
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
    let progress = DeployProgress::with_status_printer(hosts_to_sync.clone());

    // Connect to all hosts in parallel, checking Hello for staleness.
    let stale: Mutex<Vec<(Hostname, Oid, Box<dyn Connection>)>> = Mutex::new(Vec::new());
    let progress = &progress;
    std::thread::scope(|s| {
        for host in &hosts_to_sync {
            let stale = &stale;
            let expected = host_refs.get(host).map(|hr| hr.commit);
            s.spawn(move || {
                let conn = match deploy::try_connect(host, connector, progress) {
                    Some(c) => c,
                    None => return,
                };
                match conn.hello().current_commit {
                    Some(actual) if Some(actual) != expected => {
                        stale.lock().push((host.clone(), actual, conn));
                    }
                    _ => progress.update(host, HostState::Done),
                }
            });
        }
    });

    // Fetch sequentially: the first fetch may provide objects that later
    // hosts also need, avoiding redundant transfers.
    let mut stale_hosts = stale
        .into_inner()
        .into_iter()
        .map(|(host, actual, conn)| {
            let expected = host_refs.get(&host).map(|hr| hr.commit);
            (
                host,
                StaleHost {
                    expected_commit: expected,
                    actual_commit: Some(actual),
                    connection: conn,
                },
            )
        })
        .collect();
    fetch_stale_objects(store, &mut stale_hosts, progress);

    Ok(())
}

/// Fetch objects from stale hosts over their still-open sessions.
///
/// For each stale host whose actual commit we don't already have, sends
/// `RequestObjects` and receives a packfile. Updates the local tracking
/// ref for each host. Reports per-host errors via progress.
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
    let actual_commit = match info.actual_commit {
        Some(c) => c,
        None => return Ok(()),
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
            Some(Message::Error(apply_err)) => {
                return Err(HostError::Apply(apply_err));
            }
            other => {
                return Err(HostError::ProtocolError(format!(
                    "unexpected response to RequestObjects: {other:?}"
                )));
            }
        }
    }

    store.set_ref(
        &format!("refs/remotes/{host}/current"),
        actual_commit,
        RefUpdate::FetchStale,
    )?;

    Ok(())
}

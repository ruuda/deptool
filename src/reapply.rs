// Deptool -- A declarative configuration deployment tool.
// Copyright 2026 Ruud van Asseldonk

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// A copy of the License has been included in the root of the repository.

//! The `deptool reapply` command: re-apply each host's deployed commit.
//!
//! A deploy only touches what changed since the previous deploy, so a host
//! that drifted (a hand-edited file, a unit that lost its enablement
//! symlinks) stays broken until the config changes again. Reapply converges
//! every host on the commit its tracking ref records: it re-materializes
//! every app checkout and re-enables and starts every manifest-enabled
//! unit, without disrupting units that are already running.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use git2::Oid;

use crate::deploy::{self, ApplyOutcome, Connection, DeployProgress, HostState};
use crate::error::{Error, HostError, Result};
use crate::filter::HostFilter;
use crate::prim::Hostname;
use crate::protocol::{ApplyMode, Request};
use crate::setup::HostConnector;
use crate::store::{Store, tree_entries};
use crate::sync;

/// What reapply will converge one host on, derived from its tracking ref.
#[derive(Debug)]
pub struct HostReapply {
    /// The deployed commit recorded by the host's tracking ref.
    pub commit: Oid,
    /// Number of apps deployed to the host, for the plan display.
    pub n_apps: usize,
    /// Manifest-enabled units, which reapply enables and starts.
    pub units_enabled: BTreeSet<String>,
}

/// Look up each host's deployed commit from the tracking refs.
///
/// Errors when a host in the config tree has no tracking ref: another
/// operator may have deployed to it, so the fix is `deptool sync`, not
/// assuming the host is fresh. `filter` narrows the candidate set; any
/// limit name not in the tree is reported as an error before any host is
/// contacted.
pub fn select_hosts_to_reapply(
    store: &Store,
    dir: &Path,
    filter: &HostFilter,
) -> Result<BTreeMap<Hostname, HostReapply>> {
    let tree_oid = store.build_tree(dir, filter)?;
    let config_hosts = store.host_trees(tree_oid)?;
    let host_names: Vec<Hostname> = config_hosts.keys().cloned().collect();
    let host_refs = store.host_tracking_refs(&host_names)?;

    let unsynced: Vec<Hostname> = config_hosts
        .keys()
        .filter(|host| !host_refs.contains_key(host))
        .cloned()
        .collect();
    if !unsynced.is_empty() {
        return Err(Error::UnsyncedHosts(unsynced));
    }

    let mut hosts = BTreeMap::new();
    for host in config_hosts.into_keys() {
        let host_ref = &host_refs[&host];
        let apps = tree_entries(&store.repo.find_tree(host_ref.host_tree)?);
        let mut units_enabled = BTreeSet::new();
        for app_tree in apps.values() {
            units_enabled.extend(store.enabled_units(*app_tree)?);
        }
        hosts.insert(
            host,
            HostReapply {
                commit: host_ref.commit,
                n_apps: apps.len(),
                units_enabled,
            },
        );
    }
    Ok(hosts)
}

/// Connect to and lock all hosts, then reapply each host's own commit.
///
/// Mirrors `run_deploy`, minus the steps a reapply does not need: there is
/// no pack to push (the target commit is the host's current commit), no
/// tracking ref to advance, and no rollback on failure.
pub fn run_reapply(
    store: &Store,
    hosts: &BTreeMap<Hostname, HostReapply>,
    operator: &str,
    connector: &dyn HostConnector,
    progress: &DeployProgress,
) -> Result<()> {
    let expected_commits: BTreeMap<Hostname, Option<Oid>> = hosts
        .iter()
        .map(|(host, reapply)| (host.clone(), Some(reapply.commit)))
        .collect();
    let mut lock_result =
        deploy::connect_and_lock(&expected_commits, operator, connector, progress);

    if lock_result.locked.len() < hosts.len() {
        // Same forward-progress rule as in `run_deploy`: fetch from stale
        // hosts so the tracking refs converge with reality, then the next
        // reapply targets the commit the host actually has.
        sync::fetch_stale_objects(store, &mut lock_result.stale, progress);
        let n = progress.num_failed();
        return Err(Error::DeployFailed(format!(
            "failed to lock {n} host(s), aborting",
        )));
    }

    // A host whose target ref diverged from current has an incomplete
    // deploy: it started but did not finish. Reapplying `current` could
    // revert changes that deploy already made, and in a cluster-wide
    // plan that detail is easy to miss, so refuse the whole run.
    // `deptool deploy` still applies to such a host.
    let mut n_incomplete = 0;
    for (host, conn) in &lock_result.locked {
        let hello = conn.hello();
        if hello.target_commit != hello.current_commit {
            progress.update(host, HostError::IncompleteDeploy);
            n_incomplete += 1;
        }
    }
    if n_incomplete > 0 {
        // Dropping the connections releases the host locks.
        return Err(Error::DeployFailed(format!(
            "refusing to reapply, {n_incomplete} host(s) have an incomplete deploy",
        )));
    }

    std::thread::scope(|s| {
        for (host, mut conn) in lock_result.locked {
            let target_commit = hosts[&host].commit;
            s.spawn(move || {
                if let Err(err) = reapply_host(&host, &mut conn, target_commit, progress) {
                    progress.update(&host, err);
                }
            });
        }
    });

    progress.outcome()
}

/// Reapply on a single host.
fn reapply_host(
    host: &Hostname,
    conn: &mut Box<dyn Connection>,
    target_commit: Oid,
    progress: &DeployProgress,
) -> std::result::Result<(), HostError> {
    progress.update(host, HostState::Applying);
    conn.send_request(&Request::Apply {
        target_commit,
        mode: ApplyMode::Reapply,
    })?;
    conn.close();
    let applied_commit = match deploy::read_apply_messages(host, conn, progress)? {
        ApplyOutcome::Applied(oid) => oid,
        ApplyOutcome::RolledBack => unreachable!("agent never rolls back a reapply"),
    };
    assert_eq!(
        applied_commit, target_commit,
        "applied commit matches the reapply target"
    );

    progress.update(host, HostState::Done);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::checkout::oid_prefix;
    use crate::plan::Plan;
    use crate::store::RefUpdate;
    use crate::testutil::{
        TestHost, TestRepo, commit_files, config_with, test_connector, test_progress,
    };

    #[test]
    fn select_reads_apps_and_units_from_the_tracking_ref_commit() -> Result<()> {
        let t = TestRepo::new();
        let manifest = br#"{"systemd":{"units_enabled":["nginx.service"]}}"#;
        let c1 = t.commit(&[
            ("web1/lego/conf", b"v1"),
            ("web1/nginx/conf", b"v1"),
            ("web1/nginx/manifest.json", manifest),
        ]);
        let deployed = t.mark_deployed(c1);

        // The config tree lists fewer apps than the deployed commit: what
        // gets reapplied must come from the tracking ref, not the config.
        let config = config_with(&[("web1/nginx/conf", b"v2")]);
        let hosts = select_hosts_to_reapply(&t.store, config.path(), &HostFilter::All)?;

        let web1 = &hosts[&"web1".into()];
        assert_eq!(web1.commit, deployed.hosts[&"web1".into()].commit);
        assert_eq!(web1.n_apps, 2);
        assert_eq!(
            web1.units_enabled,
            BTreeSet::from(["nginx.service".to_string()]),
        );
        Ok(())
    }

    #[test]
    fn select_errors_when_a_config_host_has_no_tracking_ref() {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/app/conf", b"v1")]);
        t.mark_deployed(c1);

        // web2 is in the config tree, but this store has no record of it.
        // Another operator may have deployed to it, so reapply must not
        // assume the host is fresh.
        let config = config_with(&[("web1/app/conf", b"v1"), ("web2/app/conf", b"v1")]);
        match select_hosts_to_reapply(&t.store, config.path(), &HostFilter::All) {
            Err(Error::UnsyncedHosts(hosts)) => {
                assert_eq!(hosts, vec![Hostname::from("web2")]);
            }
            other => panic!("expected UnsyncedHosts, got {other:?}"),
        }
    }

    #[test]
    fn select_with_limit_includes_only_listed_hosts() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/app/conf", b"a"), ("web2/app/conf", b"b")]);
        t.mark_deployed(c1);

        let config = config_with(&[("web1/app/conf", b"a"), ("web2/app/conf", b"b")]);
        let filter = HostFilter::Only(vec!["web2".into()]);
        let hosts = select_hosts_to_reapply(&t.store, config.path(), &filter)?;

        assert_eq!(
            hosts.keys().cloned().collect::<Vec<_>>(),
            vec![Hostname::from("web2")],
            "limit narrows the reapply set to exactly the listed hosts",
        );
        Ok(())
    }

    /// Deploy `plan` to the targets through the real deploy machinery.
    fn deploy_to(driver: &TestRepo, targets: &[&TestHost], plan: &Plan) -> Result<()> {
        let hostnames: Vec<_> = plan.hosts.keys().map(|h| h.0.as_str()).collect();
        let progress = test_progress(&hostnames);
        let connector = test_connector(targets);
        deploy::run_deploy(
            &driver.store,
            plan,
            "deckard@spinner",
            &connector,
            &progress,
        )
    }

    #[test]
    fn reapply_restores_drifted_checkout_and_keeps_previous_version() -> Result<()> {
        let driver = TestRepo::new();
        let target = TestHost::new("web1");
        // Deploys apply the plan's own deploy commit, so name checkout
        // version dirs after the plan commits, not c1 and c2.
        let c1 = driver.commit(&[("web1/nginx/nginx.conf", b"v1")]);
        let plan_v1 = driver.plan(c1);
        deploy_to(&driver, &[&target], &plan_v1)?;
        let c2 = driver.commit(&[("web1/nginx/nginx.conf", b"v2")]);
        let plan_v2 = driver.plan(c2);
        deploy_to(&driver, &[&target], &plan_v2)?;

        // Drift: the checked-out config file vanished.
        let conf = target.apps_path().join("nginx/current/nginx.conf");
        fs::remove_file(&conf)?;

        let config = config_with(&[("web1/nginx/nginx.conf", b"v2")]);
        let hosts = select_hosts_to_reapply(&driver.store, config.path(), &HostFilter::All)?;
        let progress = test_progress(&["web1"]);
        let connector = test_connector(&[&target]);
        run_reapply(
            &driver.store,
            &hosts,
            "deckard@spinner",
            &connector,
            &progress,
        )?;

        assert!(matches!(*progress.state("web1"), HostState::Done));
        assert_eq!(fs::read(&conf)?, b"v2", "drifted file is re-materialized");
        // `previous` still points at the last distinct version: rotating it
        // here would make the next gc delete the real rollback target.
        assert_eq!(
            fs::read_link(target.apps_path().join("nginx/previous"))?,
            Path::new(&oid_prefix(plan_v1.hosts[&"web1".into()].commit)),
        );
        assert_eq!(
            driver.get_host_tracking_ref("web1"),
            Some(plan_v2.hosts[&"web1".into()].commit)
        );
        Ok(())
    }

    #[test]
    fn reapply_refuses_host_with_incomplete_deploy() -> Result<()> {
        let driver = TestRepo::new();
        let target = TestHost::new("web1");
        let c1 = driver.commit(&[("web1/nginx/nginx.conf", b"v1")]);
        let plan_v1 = driver.plan(c1);
        deploy_to(&driver, &[&target], &plan_v1)?;

        // Simulate a deploy that died between setting the target ref and
        // completing: the host's target ref moved, current did not.
        let host_store = Store::open(target.session.store.path())?;
        let c2 = commit_files(&host_store, &[("nginx/nginx.conf", b"v2")])?;
        host_store.set_ref(
            "refs/heads/target",
            c2,
            RefUpdate::SetTarget {
                operator: "roy@nexus",
            },
        )?;

        let config = config_with(&[("web1/nginx/nginx.conf", b"v1")]);
        let hosts = select_hosts_to_reapply(&driver.store, config.path(), &HostFilter::All)?;
        let progress = test_progress(&["web1"]);
        let connector = test_connector(&[&target]);
        let result = run_reapply(
            &driver.store,
            &hosts,
            "deckard@spinner",
            &connector,
            &progress,
        );

        assert!(result.is_err(), "reapply refuses the incomplete deploy");
        assert!(matches!(
            *progress.state("web1"),
            HostState::Failed(HostError::IncompleteDeploy)
        ));
        // The host was not modified: current still points at the v1 deploy.
        assert_eq!(
            fs::read_link(target.apps_path().join("nginx/current"))?,
            Path::new(&oid_prefix(plan_v1.hosts[&"web1".into()].commit)),
        );
        Ok(())
    }

    #[test]
    fn reapply_aborts_on_stale_host_and_updates_tracking_ref() -> Result<()> {
        let driver = TestRepo::new();
        let c1 = driver.commit(&[("app/conf", b"v1")]);
        let c2 = driver.commit(&[("app/conf", b"v2")]);
        // The host is at c2 (deployed from elsewhere), but our tracking
        // ref still says c1.
        let target = TestHost::at_commit(&driver, "web1", c2);
        driver.set_host_tracking_ref("web1", c1);

        let config = config_with(&[("web1/app/conf", b"v2")]);
        let hosts = select_hosts_to_reapply(&driver.store, config.path(), &HostFilter::All)?;
        let progress = test_progress(&["web1"]);
        let connector = test_connector(&[&target]);
        let result = run_reapply(
            &driver.store,
            &hosts,
            "deckard@spinner",
            &connector,
            &progress,
        );

        assert!(result.is_err(), "reapply aborts on a stale host");
        assert!(matches!(*progress.state("web1"), HostState::Stale));
        // The abort fetched the host's real commit, so the next reapply
        // targets it.
        assert_eq!(driver.get_host_tracking_ref("web1"), Some(c2));
        Ok(())
    }
}

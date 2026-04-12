//! Deployment plan: diff the desired config against each host's current state.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use git2::Oid;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::prim::Hostname;
use crate::store::Store;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AppDiff {
    Add {
        #[serde(with = "crate::prim::ser::oid")]
        new_tree: Oid,
    },
    Remove {
        #[serde(with = "crate::prim::ser::oid")]
        old_tree: Oid,
    },
    Update {
        #[serde(with = "crate::prim::ser::oid")]
        old_tree: Oid,
        #[serde(with = "crate::prim::ser::oid")]
        new_tree: Oid,
    },
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostPlan {
    pub apps: BTreeMap<String, AppDiff>,
    #[serde(with = "crate::prim::ser::oid_option")]
    pub expected_current: Option<Oid>,
    pub is_fast_forward: bool,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub hosts: BTreeMap<Hostname, HostPlan>,
    #[serde(with = "crate::prim::ser::oid")]
    pub commit: Oid,
}

/// Systemd unit lifecycle actions, derived from Git trees only.
///
/// We don't query actual system state; actions are based on comparing the
/// previous and target commits. If the system drifts (e.g. a human disables
/// a unit manually), the operator will see it in `systemctl status` output
/// that we report after applying.
#[derive(Debug)]
pub struct UnitChanges {
    /// Newly enabled units: `systemctl enable --now`.
    pub enable: Vec<String>,
    /// Still enabled, but app content changed: `systemctl restart`.
    pub restart: Vec<String>,
    /// No longer enabled: `systemctl disable --now`.
    pub disable: Vec<String>,
}

impl UnitChanges {
    pub fn is_empty(&self) -> bool {
        self.enable.is_empty() && self.restart.is_empty() && self.disable.is_empty()
    }
}

/// Compute unit lifecycle actions by comparing two enabled unit sets.
///
/// Both sets are pre-filtered to changed apps only, so a unit appearing
/// in both means its app changed while it stayed enabled → restart.
pub fn diff_enabled(prev: &BTreeSet<String>, target: &BTreeSet<String>) -> UnitChanges {
    let mut changes = UnitChanges {
        enable: Vec::new(),
        restart: Vec::new(),
        disable: Vec::new(),
    };
    for name in target {
        if prev.contains(name) {
            changes.restart.push(name.clone());
        } else {
            changes.enable.push(name.clone());
        }
    }
    for name in prev {
        if !target.contains(name) {
            changes.disable.push(name.clone());
        }
    }
    changes
}

/// Manifest symlink actions, derived from comparing two commits' manifests.
#[derive(Debug)]
pub struct SymlinkChanges<T> {
    /// New symlinks to create: (link_path, source_path).
    pub create: Vec<(T, T)>,
    /// Symlinks to remove.
    pub remove: Vec<T>,
    /// Symlinks whose source changed: (link_path, new_source_path).
    pub change: Vec<(T, T)>,
}

impl<T> SymlinkChanges<T> {
    pub fn is_empty(&self) -> bool {
        self.create.is_empty() && self.remove.is_empty() && self.change.is_empty()
    }
}

/// Compute symlink actions by comparing two symlink maps.
pub fn diff_symlinks<T: Clone + Ord>(
    previous: &BTreeMap<T, T>,
    target: &BTreeMap<T, T>,
) -> SymlinkChanges<T> {
    let mut changes = SymlinkChanges {
        create: Vec::new(),
        remove: Vec::new(),
        change: Vec::new(),
    };
    for (link, source) in target {
        match previous.get(link) {
            None => changes.create.push((link.clone(), source.clone())),
            Some(prev_source) if prev_source != source => {
                changes.change.push((link.clone(), source.clone()))
            }
            Some(_) => {}
        }
    }
    for link in previous.keys() {
        if !target.contains_key(link) {
            changes.remove.push(link.clone());
        }
    }
    changes
}

/// All deployment changes for a host, computed from the git state.
#[derive(Debug)]
pub struct Changes {
    pub units: UnitChanges,
    pub symlinks: SymlinkChanges<PathBuf>,
}

/// Diff two sets of app tree oids for a single host.
pub fn diff_apps(
    current: &BTreeMap<String, Oid>,
    target: &BTreeMap<String, Oid>,
) -> BTreeMap<String, AppDiff> {
    let mut changes = BTreeMap::new();

    for (name, target_oid) in target {
        match current.get(name) {
            None => {
                changes.insert(
                    name.clone(),
                    AppDiff::Add {
                        new_tree: *target_oid,
                    },
                );
            }
            Some(cur_oid) if cur_oid != target_oid => {
                changes.insert(
                    name.clone(),
                    AppDiff::Update {
                        old_tree: *cur_oid,
                        new_tree: *target_oid,
                    },
                );
            }
            Some(_) => {}
        }
    }

    for (name, oid) in current {
        if !target.contains_key(name) {
            changes.insert(name.clone(), AppDiff::Remove { old_tree: *oid });
        }
    }

    changes
}

/// Build a deployment plan by comparing main against each host's current ref.
///
/// TODO: Currently this is based only on the repository state, which means we
/// need to fetch the remote refs ahead of time. We should split this into two
/// stages: first eliminate hosts that we definitely do not need to touch based
/// on current refs. Then for hosts that do need touching we refresh their refs,
/// and plan again. We could just use the same plan function for that though.
pub fn make_plan(store: &Store) -> Result<Plan> {
    let main_commit = store
        .repo
        .find_reference("refs/heads/main")?
        .peel_to_commit()?;
    let commit = main_commit.id();
    let main_tree = main_commit.tree()?;

    store.validate(main_tree.id())?;

    let mut hosts = BTreeMap::new();

    for entry in main_tree.iter() {
        let host = Hostname(entry.name().expect("tree entry name is utf-8").to_string());

        let target_apps = store.get_host_apps(&main_tree, &host)?;

        let (expected_current, current_apps) = match store
            .repo
            .find_reference(&format!("refs/remotes/{host}/current"))
        {
            Err(_) => (None, BTreeMap::new()),
            Ok(r) => {
                let c = r.peel_to_commit()?;
                let tree = c.tree()?;
                (Some(c.id()), store.get_host_apps(&tree, &host)?)
            }
        };

        let apps = diff_apps(&current_apps, &target_apps);

        if !apps.is_empty() {
            let is_fast_forward = match &expected_current {
                None => true,
                Some(current) => store.repo.graph_descendant_of(commit, *current)?,
            };

            hosts.insert(
                host,
                HostPlan {
                    apps,
                    expected_current,
                    is_fast_forward,
                },
            );
        }
    }

    Ok(Plan { hosts, commit })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Result;
    use crate::testutil::TestRepo;

    #[test]
    fn plan_shows_all_apps_as_add_for_new_host() -> Result<()> {
        let t = TestRepo::new();
        t.commit(&[("web1/nginx/conf", b"a"), ("web1/rofld/conf", b"b")]);

        let plan = make_plan(&t.store)?;
        assert_eq!(plan.hosts.len(), 1);
        let apps = &plan.hosts[&"web1".into()].apps;
        assert_eq!(apps.len(), 2);
        assert!(matches!(apps["rofld"], AppDiff::Add { .. }));
        assert!(matches!(apps["nginx"], AppDiff::Add { .. }));
        Ok(())
    }

    #[test]
    fn plan_detects_updated_and_unchanged_apps() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/conf", b"v1"), ("web1/rofld/conf", b"v1")]);
        t.set_host_tracking_ref("web1", c1);

        t.commit(&[("web1/nginx/conf", b"v2"), ("web1/rofld/conf", b"v1")]);

        let plan = make_plan(&t.store)?;
        let apps = &plan.hosts[&"web1".into()].apps;
        assert_eq!(apps.len(), 1);
        assert!(matches!(apps["nginx"], AppDiff::Update { .. }));
        Ok(())
    }

    #[test]
    fn plan_detects_removed_apps() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/conf", b"a"), ("web1/rofld/conf", b"b")]);
        t.set_host_tracking_ref("web1", c1);

        t.commit(&[("web1/nginx/conf", b"a")]);

        let plan = make_plan(&t.store)?;
        let apps = &plan.hosts[&"web1".into()].apps;
        assert_eq!(apps.len(), 1);
        assert!(matches!(apps["rofld"], AppDiff::Remove { .. }));
        Ok(())
    }

    #[test]
    fn plan_includes_new_host_alongside_up_to_date_host() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/conf", b"a")]);
        t.set_host_tracking_ref("web1", c1);

        t.commit(&[("web1/nginx/conf", b"a"), ("web2/rofld/conf", b"b")]);

        let plan = make_plan(&t.store)?;
        assert!(!plan.hosts.contains_key(&"web1".into()));
        let apps = &plan.hosts[&"web2".into()].apps;
        assert_eq!(apps.len(), 1);
        assert!(matches!(apps["rofld"], AppDiff::Add { .. }));
        Ok(())
    }

    #[test]
    fn plan_omits_hosts_that_are_up_to_date() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/conf", b"a")]);
        t.set_host_tracking_ref("web1", c1);

        let plan = make_plan(&t.store)?;
        assert!(plan.hosts.is_empty());
        Ok(())
    }

    #[test]
    fn plan_detects_non_fast_forward() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/conf", b"v1")]);
        // Simulate another driver deploying c2 (descendant of c1).
        let c2 = t.commit(&[("web1/nginx/conf", b"v2")]);
        t.set_host_tracking_ref("web1", c2);

        // Reset main back to c1 so the next commit branches from c1,
        // not from c2. This simulates our local repo diverging.
        // There is no correct RefUpdate for this ref update because we
        // construct this situation artificially in the tests. It's not
        // worth adding a RefUpdate that is only used in tests, so we'll
        // abuse FetchStale here.
        t.store
            .set_ref("refs/heads/main", c1, crate::store::RefUpdate::FetchStale)?;
        t.commit(&[("web1/nginx/conf", b"v3")]);

        // The new commit descends from c1, but the host has c2. Not a fast-forward.
        let plan = make_plan(&t.store)?;
        assert!(!plan.hosts[&"web1".into()].is_fast_forward);
        Ok(())
    }
}

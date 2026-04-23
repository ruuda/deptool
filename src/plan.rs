//! Deployment plan: diff the desired config against each host's current state.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use git2::Oid;
use serde::{Deserialize, Serialize};

use crate::error::{Result, StoreError};
use crate::prim::Hostname;
use crate::store::{Store, tree_entries};

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

/// Side effects to apply to the host system for a set of app changes.
///
/// Per-app instances can be combined via `merge` into a host-level aggregate.
#[derive(Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemDiff<T = String> {
    /// Unit lifecycle actions.
    pub units: UnitChanges,
    /// Manifest symlink changes.
    pub symlinks: SymlinkChanges<T>,
}

impl<T> SystemDiff<T> {
    /// Whether this diff can be safely rolled back by re-applying the previous
    /// commit.
    ///
    /// * Creations are unsafe: they may overwrite pre-existing state we can't
    ///   restore. This is a consequence of enabling gradual adoption.
    /// * Removals and restarts are safe: we created that state, so if we
    ///   already deleted it, we can just recreate it.
    ///
    /// IMPORTANT: every field of every struct must be destructured here, so
    /// that adding a field anywhere is a compile error that forces revisiting
    /// this safety check. Do NOT use field access (e.g. `self.symlinks.create`)
    /// -- it silently ignores new fields.
    pub fn is_rollback_safe(&self) -> bool {
        let SystemDiff { units, symlinks } = self;
        let UnitChanges {
            enable,
            restart: _,
            disable: _,
            link,
            unlink: _,
        } = units;
        let SymlinkChanges {
            create,
            remove: _,
            change: _,
        } = symlinks;
        enable.is_empty() && link.is_empty() && create.is_empty()
    }

    /// Move all entries from `other` into `self`, leaving `other` empty.
    pub fn append(&mut self, other: &mut Self) {
        self.units.enable.append(&mut other.units.enable);
        self.units.restart.append(&mut other.units.restart);
        self.units.disable.append(&mut other.units.disable);
        self.units.link.append(&mut other.units.link);
        self.units.unlink.append(&mut other.units.unlink);
        self.symlinks.create.append(&mut other.symlinks.create);
        self.symlinks.remove.append(&mut other.symlinks.remove);
        self.symlinks.change.append(&mut other.symlinks.change);
    }
}

impl SystemDiff {
    /// Resolve manifest symlink paths for a specific app on the host.
    ///
    /// Converts relative source paths to absolute paths under
    /// `apps_dir/<app>/current/`.
    pub fn resolve_symlinks(self, app: &str, apps_dir: &Path) -> SystemDiff<PathBuf> {
        let current_dir = apps_dir.join(app).join("current");
        SystemDiff {
            units: self.units,
            symlinks: self.symlinks.map(PathBuf::from, |s| current_dir.join(s)),
        }
    }
}

/// Per-app diff and precomputed actions, produced during planning.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppPlan {
    pub diff: AppDiff,
    pub system: SystemDiff,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostPlan {
    pub apps: BTreeMap<String, AppPlan>,
    #[serde(with = "crate::prim::ser::oid_option")]
    pub expected_current: Option<Oid>,
    /// Whether the host can be automatically rolled back on deploy failure.
    ///
    /// True when every app's `SystemDiff` is rollback-safe. See
    /// [`SystemDiff::is_rollback_safe`] for what that means.
    pub is_rollback_safe: bool,
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
///
/// The `link`/`unlink` and `enable`/`disable`/`restart` groups can overlap:
/// a unit file may be both linked and enabled in the same deploy.
#[derive(Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitChanges {
    /// Newly enabled units: `systemctl enable --now`.
    pub enable: Vec<String>,
    /// Still enabled, but app content changed: `systemctl restart`.
    pub restart: Vec<String>,
    /// No longer enabled: `systemctl disable --now`.
    pub disable: Vec<String>,
    /// Unit files newly provided by this app.
    pub link: Vec<String>,
    /// Unit files no longer provided by this app.
    pub unlink: Vec<String>,
}

impl UnitChanges {
    pub fn is_empty(&self) -> bool {
        self.enable.is_empty()
            && self.restart.is_empty()
            && self.disable.is_empty()
            && self.link.is_empty()
            && self.unlink.is_empty()
    }
}

/// Compute unit lifecycle actions by comparing two enabled unit sets.
///
/// Both sets are pre-filtered to changed apps only, so a unit appearing
/// in both means its app changed while it stayed enabled → restart.
pub fn diff_enabled(prev: &BTreeSet<String>, target: &BTreeSet<String>) -> UnitChanges {
    let mut changes = UnitChanges::default();
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
#[derive(Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

    /// Transform link and source paths.
    pub fn map<U>(
        self,
        mut link: impl FnMut(T) -> U,
        mut source: impl FnMut(T) -> U,
    ) -> SymlinkChanges<U> {
        SymlinkChanges {
            create: self
                .create
                .into_iter()
                .map(|(l, s)| (link(l), source(s)))
                .collect(),
            remove: self.remove.into_iter().map(&mut link).collect(),
            change: self
                .change
                .into_iter()
                .map(|(l, s)| (link(l), source(s)))
                .collect(),
        }
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

/// Compute the system-level side effects of a single app change.
pub fn compute_system_diff(
    store: &Store,
    diff: &AppDiff,
) -> std::result::Result<SystemDiff, StoreError> {
    let result = match diff {
        AppDiff::Add { new_tree } => {
            let enabled = store.enabled_units(*new_tree)?;
            let mut units = diff_enabled(&BTreeSet::new(), &enabled);
            units.link = store.app_units(*new_tree)?.into_iter().collect();
            let manifest = store.read_manifest(*new_tree)?;
            let symlinks = diff_symlinks(&BTreeMap::new(), &manifest.symlinks);
            SystemDiff { units, symlinks }
        }
        AppDiff::Remove { old_tree } => {
            let enabled = store.enabled_units(*old_tree)?;
            let mut units = diff_enabled(&enabled, &BTreeSet::new());
            units.unlink = store.app_units(*old_tree)?.into_iter().collect();
            let manifest = store.read_manifest(*old_tree)?;
            let symlinks = diff_symlinks(&manifest.symlinks, &BTreeMap::new());
            SystemDiff { units, symlinks }
        }
        AppDiff::Update { old_tree, new_tree } => {
            let old_all = store.app_units(*old_tree)?;
            let new_all = store.app_units(*new_tree)?;
            let old_enabled = store.enabled_units(*old_tree)?;
            let new_enabled = store.enabled_units(*new_tree)?;
            let mut units = diff_enabled(&old_enabled, &new_enabled);
            units.link = new_all.difference(&old_all).cloned().collect();
            units.unlink = old_all.difference(&new_all).cloned().collect();
            let old_manifest = store.read_manifest(*old_tree)?;
            let new_manifest = store.read_manifest(*new_tree)?;
            let symlinks = diff_symlinks(&old_manifest.symlinks, &new_manifest.symlinks);
            SystemDiff { units, symlinks }
        }
    };
    Ok(result)
}

/// Compute per-app diff and system actions for the plan.
pub fn compute_app_plan(store: &Store, diff: AppDiff) -> Result<AppPlan> {
    let system = compute_system_diff(store, &diff)?;
    Ok(AppPlan { diff, system })
}

/// Map from unit filename to the absolute symlink target path.
pub type DesiredUnits = BTreeMap<String, PathBuf>;

/// Compute per-app diffs and the aggregate system diff for a host deploy.
///
/// Either commit can be None to represent an empty host (no apps).
pub fn diff_host(
    store: &Store,
    host: &Hostname,
    apps_dir: &Path,
    current_commit: Option<Oid>,
    target_commit: Option<Oid>,
) -> std::result::Result<(BTreeMap<String, AppDiff>, SystemDiff<PathBuf>), StoreError> {
    let get_apps = |oid| -> std::result::Result<BTreeMap<String, Oid>, StoreError> {
        let tree = store.get_commit_tree(oid)?;
        store.get_host_apps(&tree, host)
    };
    let current_apps = current_commit
        .map(get_apps)
        .transpose()?
        .unwrap_or_default();
    let target_apps = target_commit.map(get_apps).transpose()?.unwrap_or_default();
    let app_diffs = diff_apps(&current_apps, &target_apps);

    let mut system = SystemDiff::<PathBuf>::default();
    for (app, change) in &app_diffs {
        let mut resolved = compute_system_diff(store, change)?.resolve_symlinks(app, apps_dir);
        system.append(&mut resolved);
    }

    Ok((app_diffs, system))
}

/// Diff a config tree against the deployed state and build a deploy plan.
///
/// Returns `None` if no hosts need changes. Creates a commit with the
/// frontier of affected hosts' tracking refs as parents.
pub fn make_plan(store: &Store, tree_oid: Oid) -> Result<Option<Plan>> {
    // Validate first so we don't do diff work or create commits from
    // invalid config.
    store.validate(tree_oid)?;

    let config_hosts = store.host_trees(tree_oid)?;
    let host_names: Vec<Hostname> = config_hosts.keys().cloned().collect();
    let host_refs = store.host_tracking_refs(&host_names)?;

    // Diff each host's target against its current state, collecting
    // parent commits and per-host plans in a single pass.
    let mut parent_commits: Vec<Oid> = Vec::new();
    let mut hosts = BTreeMap::new();

    for (host, &target_tree_oid) in &config_hosts {
        let (expected_current, current_apps) = match host_refs.get(host) {
            Some(hr) if hr.host_tree == target_tree_oid => continue,
            Some(hr) => {
                parent_commits.push(hr.commit);
                let apps = tree_entries(&store.repo.find_tree(hr.host_tree)?);
                (Some(hr.commit), apps)
            }
            None => (None, BTreeMap::new()),
        };

        let target_apps = tree_entries(&store.repo.find_tree(target_tree_oid)?);
        let diffs = diff_apps(&current_apps, &target_apps);

        let mut apps = BTreeMap::new();
        let mut is_rollback_safe = true;
        for (name, diff) in diffs {
            let plan = compute_app_plan(store, diff)?;
            is_rollback_safe &= plan.system.is_rollback_safe();
            apps.insert(name, plan);
        }

        hosts.insert(
            host.clone(),
            HostPlan {
                apps,
                expected_current,
                is_rollback_safe,
            },
        );
    }

    if hosts.is_empty() {
        return Ok(None);
    }

    let parents = store.frontier(&parent_commits)?;
    let commit = store.commit_tree(tree_oid, &parents)?;
    Ok(Some(Plan { hosts, commit }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Result;
    use crate::store::RefUpdate;
    use crate::testutil::TestRepo;

    #[test]
    fn plan_shows_all_apps_as_add_for_new_host() -> Result<()> {
        let t = TestRepo::new();
        let c = t.commit(&[("web1/nginx/conf", b"a"), ("web1/rofld/conf", b"b")]);

        let plan = t.plan(c);
        assert_eq!(plan.hosts.len(), 1);
        let apps = &plan.hosts[&"web1".into()].apps;
        assert_eq!(apps.len(), 2);
        assert!(matches!(apps["rofld"].diff, AppDiff::Add { .. }));
        assert!(matches!(apps["nginx"].diff, AppDiff::Add { .. }));
        Ok(())
    }

    #[test]
    fn plan_detects_updated_and_unchanged_apps() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/conf", b"v1"), ("web1/rofld/conf", b"v1")]);
        t.set_host_tracking_ref("web1", c1);

        let c2 = t.commit(&[("web1/nginx/conf", b"v2"), ("web1/rofld/conf", b"v1")]);

        let plan = t.plan(c2);
        let apps = &plan.hosts[&"web1".into()].apps;
        assert_eq!(apps.len(), 1);
        assert!(matches!(apps["nginx"].diff, AppDiff::Update { .. }));
        Ok(())
    }

    #[test]
    fn plan_detects_removed_apps() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/conf", b"a"), ("web1/rofld/conf", b"b")]);
        t.set_host_tracking_ref("web1", c1);

        let c2 = t.commit(&[("web1/nginx/conf", b"a")]);

        let plan = t.plan(c2);
        let apps = &plan.hosts[&"web1".into()].apps;
        assert_eq!(apps.len(), 1);
        assert!(matches!(apps["rofld"].diff, AppDiff::Remove { .. }));
        Ok(())
    }

    #[test]
    fn plan_includes_new_host_alongside_up_to_date_host() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/conf", b"a")]);
        t.set_host_tracking_ref("web1", c1);

        let c2 = t.commit(&[("web1/nginx/conf", b"a"), ("web2/rofld/conf", b"b")]);

        let plan = t.plan(c2);
        assert!(!plan.hosts.contains_key(&"web1".into()));
        let apps = &plan.hosts[&"web2".into()].apps;
        assert_eq!(apps.len(), 1);
        assert!(matches!(apps["rofld"].diff, AppDiff::Add { .. }));
        Ok(())
    }

    #[test]
    fn plan_returns_none_when_up_to_date() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/conf", b"a")]);
        t.set_host_tracking_ref("web1", c1);

        let tree_oid = t.get_commit_tree_oid(c1);
        assert!(make_plan(&t.store, tree_oid)?.is_none());
        Ok(())
    }

    /// Plan a single-host update and return the resulting SystemDiff.
    fn diff_update(before: &[(&str, &[u8])], after: &[(&str, &[u8])]) -> Result<SystemDiff> {
        let t = TestRepo::new();
        let c1 = t.commit(before);
        t.set_host_tracking_ref("web1", c1);
        let c2 = t.commit(after);
        let plan = t.plan(c2);
        let host = plan.hosts.into_values().next().expect("plan has one host");
        let app = host.apps.into_values().next().expect("host has one app");
        Ok(app.system)
    }

    #[test]
    fn rollback_safe_for_content_only_update() -> Result<()> {
        let d = diff_update(
            &[("web1/nginx/nginx.conf", b"v1")],
            &[("web1/nginx/nginx.conf", b"v2")],
        )?;
        assert!(d.is_rollback_safe());
        Ok(())
    }

    #[test]
    fn rollback_unsafe_when_units_enabled() -> Result<()> {
        let d = diff_update(
            &[("web1/nginx/nginx.conf", b"v1")],
            &[
                ("web1/nginx/nginx.conf", b"v2"),
                (
                    "web1/nginx/manifest.json",
                    br#"{"systemd":{"units_enabled":["nginx.service"]}}"#,
                ),
            ],
        )?;
        assert!(!d.is_rollback_safe());
        Ok(())
    }

    #[test]
    fn rollback_unsafe_when_symlinks_added() -> Result<()> {
        let d = diff_update(
            &[("web1/nginx/nginx.conf", b"v1")],
            &[
                ("web1/nginx/nginx.conf", b"v2"),
                (
                    "web1/nginx/manifest.json",
                    br#"{"symlinks":{"/etc/nginx.conf":"nginx.conf"}}"#,
                ),
            ],
        )?;
        assert!(!d.is_rollback_safe());
        Ok(())
    }

    #[test]
    fn rollback_safe_when_symlinks_changed_or_removed() -> Result<()> {
        let d = diff_update(
            &[
                ("web1/nginx/nginx.conf", b"v1"),
                (
                    "web1/nginx/manifest.json",
                    br#"{"symlinks":{"/etc/a":"nginx.conf","/etc/b":"nginx.conf"}}"#,
                ),
            ],
            &[
                ("web1/nginx/nginx.conf", b"v2"),
                ("web1/nginx/alt.conf", b"v2"),
                (
                    "web1/nginx/manifest.json",
                    br#"{"symlinks":{"/etc/a":"alt.conf"}}"#,
                ),
            ],
        )?;
        assert!(d.is_rollback_safe());
        Ok(())
    }

    #[test]
    fn rollback_unsafe_when_unit_files_added() -> Result<()> {
        let d = diff_update(
            &[("web1/nginx/nginx.conf", b"v1")],
            &[
                ("web1/nginx/nginx.conf", b"v2"),
                ("web1/nginx/systemd/nginx.service", b"[Service]"),
            ],
        )?;
        assert!(!d.is_rollback_safe());
        Ok(())
    }

    #[test]
    fn rollback_safe_when_unit_files_removed() -> Result<()> {
        let d = diff_update(
            &[
                ("web1/nginx/nginx.conf", b"v1"),
                ("web1/nginx/systemd/nginx.service", b"[Service]"),
                (
                    "web1/nginx/manifest.json",
                    br#"{"systemd":{"units_enabled":["nginx.service"]}}"#,
                ),
            ],
            &[("web1/nginx/nginx.conf", b"v2")],
        )?;
        assert!(d.is_rollback_safe());
        Ok(())
    }

    #[test]
    fn rollback_safe_when_unit_stays_enabled_across_update() -> Result<()> {
        let manifest = br#"{"systemd":{"units_enabled":["nginx.service"]}}"#;
        let d = diff_update(
            &[
                ("web1/nginx/nginx.conf", b"v1"),
                ("web1/nginx/manifest.json", manifest),
            ],
            &[
                ("web1/nginx/nginx.conf", b"v2"),
                ("web1/nginx/manifest.json", manifest),
            ],
        )?;
        assert!(d.is_rollback_safe());
        Ok(())
    }

    #[test]
    fn host_rollback_safe_for_content_only_update() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/nginx.conf", b"v1")]);
        t.set_host_tracking_ref("web1", c1);
        let c2 = t.commit(&[("web1/nginx/nginx.conf", b"v2")]);

        let plan = t.plan(c2);
        assert!(plan.hosts[&"web1".into()].is_rollback_safe);
        Ok(())
    }

    #[test]
    fn host_rollback_safe_when_app_added_without_system_effects() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/conf", b"v1")]);
        t.set_host_tracking_ref("web1", c1);
        let c2 = t.commit(&[("web1/nginx/conf", b"v1"), ("web1/rofld/conf", b"v1")]);

        let plan = t.plan(c2);
        assert!(plan.hosts[&"web1".into()].is_rollback_safe);
        Ok(())
    }

    #[test]
    fn host_rollback_unsafe_when_any_app_is_unsafe() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/conf", b"v1")]);
        t.set_host_tracking_ref("web1", c1);
        let c2 = t.commit(&[
            ("web1/nginx/conf", b"v2"),
            (
                "web1/nginx/manifest.json",
                br#"{"systemd":{"units_enabled":["nginx.service"]}}"#,
            ),
            ("web1/rofld/conf", b"v1"),
        ]);

        let plan = t.plan(c2);
        assert!(!plan.hosts[&"web1".into()].is_rollback_safe);
        Ok(())
    }

    #[test]
    fn diverged_hosts_produce_multi_parent_commit() -> Result<()> {
        let t = TestRepo::new();
        let base = t.commit(&[("web1/app/conf", b"v1"), ("web2/app/conf", b"v1")]);
        t.set_host_tracking_ref("web1", base);
        t.set_host_tracking_ref("web2", base);

        // Simulate operator A deploying to web1 only.
        let c_a = t.commit(&[("web1/app/conf", b"v2"), ("web2/app/conf", b"v1")]);
        t.set_host_tracking_ref("web1", c_a);

        // Simulate operator B deploying to web2 only, branching from base.
        t.store
            .set_ref("refs/heads/main", base, RefUpdate::FetchStale)?;
        let c_b = t.commit(&[("web1/app/conf", b"v1"), ("web2/app/conf", b"v2")]);
        t.set_host_tracking_ref("web2", c_b);

        // Now deploy a new config that touches both hosts.
        let c_new = t.commit(&[("web1/app/conf", b"v3"), ("web2/app/conf", b"v3")]);
        let plan = t.plan(c_new);
        assert!(plan.hosts.contains_key(&"web1".into()));
        assert!(plan.hosts.contains_key(&"web2".into()));

        // The commit should descend from both diverged tracking refs.
        let commit = t.store.repo.find_commit(plan.commit)?;
        assert_eq!(commit.parent_count(), 2, "commit has two parents");
        let parents: Vec<Oid> = (0..commit.parent_count())
            .map(|i| commit.parent_id(i).expect("parent exists"))
            .collect();
        assert!(parents.contains(&c_a), "commit descends from web1's ref");
        assert!(parents.contains(&c_b), "commit descends from web2's ref");
        Ok(())
    }
}

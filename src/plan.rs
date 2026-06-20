// Deptool -- A declarative configuration deployment tool.
// Copyright 2026 Ruud van Asseldonk

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// A copy of the License has been included in the root of the repository.

//! Deployment plan: diff the desired config against each host's current state.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use git2::Oid;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, StoreError};
use crate::prim::Hostname;
use crate::store::{Store, empty_tree_oid, tree_entries};

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

impl AppDiff {
    /// The app's tree before this change, the empty tree when the app is
    /// being added, so lookups against it return nothing.
    fn old_tree(&self) -> Oid {
        match self {
            AppDiff::Add { .. } => empty_tree_oid(),
            AppDiff::Remove { old_tree } | AppDiff::Update { old_tree, .. } => *old_tree,
        }
    }

    /// The app's tree after this change, the empty tree when the app is
    /// being removed.
    fn new_tree(&self) -> Oid {
        match self {
            AppDiff::Remove { .. } => empty_tree_oid(),
            AppDiff::Add { new_tree } | AppDiff::Update { new_tree, .. } => *new_tree,
        }
    }
}

/// Side effects to apply to the host system for a set of app changes.
///
/// Per-app instances can be combined via `merge` into a host-level aggregate.
#[derive(Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemDiff<T = String> {
    /// Quadlet file changes.
    pub quadlets: SubdirChanges,
    /// Manifest symlink changes.
    pub symlinks: SymlinkChanges<T>,
    /// Sysusers config file changes.
    pub sysusers: SubdirChanges,
    /// Unit file changes in the `systemd/` directory.
    pub units: SubdirChanges,
    /// Unit actions: enable, restart, disable.
    pub unit_actions: UnitActions,
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
    /// IMPORTANT: every component must be destructured here, so that adding a
    /// field to `SystemDiff` is a compile error that forces revisiting this
    /// check. Each component's own `is_rollback_safe` likewise destructures its
    /// fields, so the exhaustiveness guarantee holds at every level.
    pub fn is_rollback_safe(&self) -> bool {
        let SystemDiff {
            quadlets,
            symlinks,
            sysusers,
            units,
            unit_actions,
        } = self;
        quadlets.is_rollback_safe()
            && symlinks.is_rollback_safe()
            && sysusers.is_rollback_safe()
            && units.is_rollback_safe()
            && unit_actions.is_rollback_safe()
    }

    /// Move all entries from `other` into `self`, leaving `other` empty.
    pub fn append(&mut self, other: &mut Self) {
        self.quadlets.append(&mut other.quadlets);
        self.symlinks.append(&mut other.symlinks);
        self.sysusers.append(&mut other.sysusers);
        self.units.append(&mut other.units);
        self.unit_actions.append(&mut other.unit_actions);
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
            quadlets: self.quadlets,
            symlinks: self.symlinks.map(PathBuf::from, |s| current_dir.join(s)),
            sysusers: self.sysusers,
            units: self.units,
            unit_actions: self.unit_actions,
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

/// A plan that has not yet been committed to the store.
///
/// Carries everything `finalize` needs to create the commit: the host data,
/// the tree the commit will point at, and the parent oids (frontier of the
/// affected hosts' tracking refs).
#[derive(Debug)]
pub struct DraftPlan {
    pub hosts: BTreeMap<Hostname, HostPlan>,
    pub tree_oid: Oid,
    pub parents: Vec<Oid>,
}

/// Changes to a managed subdirectory between two commits, derived from Git
/// trees only.
///
/// The `systemd/`, `sysusers/`, and `quadlets/` directories are each symlinked
/// into a system location; this captures which files appeared, disappeared,
/// and whether the directory's content changed at all. We don't query the live
/// system -- changes come from comparing the previous and target trees.
#[derive(Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubdirChanges {
    /// Files newly provided by this app.
    pub link: Vec<String>,
    /// Files no longer provided by this app.
    pub unlink: Vec<String>,
    /// True iff the subtree differs between old and new tree: a file added,
    /// removed, or edited. Triggers the directory's systemd tool
    /// (`daemon-reload`, `systemd-sysusers`); subsumes link and unlink, since
    /// any file added or removed also changes the subtree oid.
    pub content_changed: bool,
}

impl SubdirChanges {
    pub fn is_empty(&self) -> bool {
        self.link.is_empty() && self.unlink.is_empty() && !self.content_changed
    }

    /// Re-applying the previous commit restores removed files and edited
    /// content, but can't un-create a file we added over pre-existing state.
    fn is_rollback_safe(&self) -> bool {
        let SubdirChanges {
            link,
            unlink: _,
            content_changed: _,
        } = self;
        link.is_empty()
    }

    fn append(&mut self, other: &mut Self) {
        self.link.append(&mut other.link);
        self.unlink.append(&mut other.unlink);
        self.content_changed |= other.content_changed;
    }
}

/// Systemd unit actions (enable, restart, disable), derived from the
/// manifest's `units_enabled`.
///
/// Distinct from the `systemd/` directory's file changes (a `SubdirChanges`):
/// this says which units to enable, restart, or disable, not which unit files
/// exist. We don't query live system state; a manual change (e.g. a human
/// disabling a unit) surfaces in the `systemctl status` output we report after
/// applying.
#[derive(Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitActions {
    /// Newly enabled units: `systemctl enable --now`.
    pub enable: Vec<String>,
    /// Still enabled, but app content changed: `systemctl restart`.
    pub restart: Vec<String>,
    /// No longer enabled: `systemctl disable --now`.
    pub disable: Vec<String>,
}

impl UnitActions {
    pub fn is_empty(&self) -> bool {
        self.enable.is_empty() && self.restart.is_empty() && self.disable.is_empty()
    }

    /// Enabling a unit may start something that wasn't running before, which a
    /// rollback can't cleanly undo; restart and disable are safe.
    fn is_rollback_safe(&self) -> bool {
        let UnitActions {
            enable,
            restart: _,
            disable: _,
        } = self;
        enable.is_empty()
    }

    fn append(&mut self, other: &mut Self) {
        self.enable.append(&mut other.enable);
        self.restart.append(&mut other.restart);
        self.disable.append(&mut other.disable);
    }
}

/// Desired managed-symlink state for a host deploy.
///
/// Maps filenames to absolute symlink target paths for each managed
/// directory (systemd units, sysusers configs, and quadlets).
#[derive(Default)]
pub struct DesiredState {
    /// Quadlet files to symlink in the quadlets directory.
    pub quadlets: BTreeMap<String, PathBuf>,
    /// Sysuser config files to symlink in the sysusers directory.
    pub sysusers: BTreeMap<String, PathBuf>,
    /// Unit files to symlink in the unit directory.
    pub units: BTreeMap<String, PathBuf>,
}

/// Compute unit actions by comparing two enabled unit sets.
///
/// Both sets are pre-filtered to changed apps only, so a unit appearing
/// in both means its app changed while it stayed enabled → restart.
pub fn diff_enabled(prev: &BTreeSet<String>, target: &BTreeSet<String>) -> UnitActions {
    let mut changes = UnitActions::default();
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

    /// Re-applying the previous commit restores removed and changed symlinks,
    /// but can't un-create one we added over a pre-existing path.
    fn is_rollback_safe(&self) -> bool {
        let SymlinkChanges {
            create,
            remove: _,
            change: _,
        } = self;
        create.is_empty()
    }

    fn append(&mut self, other: &mut Self) {
        self.create.append(&mut other.create);
        self.remove.append(&mut other.remove);
        self.change.append(&mut other.change);
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

/// Diff a managed subdirectory (units, sysusers, quadlets) between two app
/// trees into the files added, the files removed, and whether its content
/// changed at all.
fn subdir_changes(
    store: &Store,
    old: Oid,
    new: Oid,
    subdir: &str,
) -> std::result::Result<SubdirChanges, StoreError> {
    let old_files = store.subdir_entries(old, subdir)?;
    let new_files = store.subdir_entries(new, subdir)?;
    Ok(SubdirChanges {
        link: new_files.difference(&old_files).cloned().collect(),
        unlink: old_files.difference(&new_files).cloned().collect(),
        content_changed: store.subtree_oid(old, subdir)? != store.subtree_oid(new, subdir)?,
    })
}

/// Compute the system-level side effects of a single app change.
///
/// The add, remove, and update cases are one path: the absent side of an add
/// or remove is the empty tree, so a set difference gives "all new" on add and
/// "all old" on remove without special-casing.
pub fn compute_system_diff(
    store: &Store,
    diff: &AppDiff,
) -> std::result::Result<SystemDiff, StoreError> {
    let old = diff.old_tree();
    let new = diff.new_tree();

    Ok(SystemDiff {
        quadlets: subdir_changes(store, old, new, "quadlets")?,
        symlinks: diff_symlinks(
            &store.read_manifest(old)?.symlinks,
            &store.read_manifest(new)?.symlinks,
        ),
        sysusers: subdir_changes(store, old, new, "sysusers")?,
        units: subdir_changes(store, old, new, "systemd")?,
        unit_actions: diff_enabled(&store.enabled_units(old)?, &store.enabled_units(new)?),
    })
}

/// Compute per-app diff and system actions for the plan.
pub fn compute_app_plan(store: &Store, diff: AppDiff) -> Result<AppPlan> {
    let system = compute_system_diff(store, &diff)?;
    Ok(AppPlan { diff, system })
}

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
    /// `HostFilter::All`.
    pub fn from_limit(args: &[String]) -> Self {
        let hosts: Vec<Hostname> = args
            .iter()
            .flat_map(|s| s.split(','))
            .filter(|s| !s.is_empty())
            .map(Hostname::from)
            .collect();
        if hosts.is_empty() {
            HostFilter::All
        } else {
            HostFilter::Only(hosts)
        }
    }

    /// Drop entries from `hosts` that the filter excludes.
    ///
    /// Returns an error listing every name in the limit that does not
    /// appear in `hosts`, so the operator can fix typos in one round trip
    /// rather than rerun-and-discover.
    pub fn apply<T>(&self, hosts: &mut BTreeMap<Hostname, T>) -> Result<()> {
        let limit = match self {
            HostFilter::All => return Ok(()),
            HostFilter::Only(limit) => limit,
        };
        let unknown: Vec<Hostname> = limit
            .iter()
            .filter(|h| !hosts.contains_key(h))
            .cloned()
            .collect();
        if !unknown.is_empty() {
            return Err(Error::UnknownHosts(unknown));
        }
        hosts.retain(|h, _| limit.contains(h));
        Ok(())
    }
}

/// Diff a config tree against the deployed state and build a deploy plan.
///
/// Returns `None` if no hosts need changes. The returned `DraftPlan` is not
/// yet committed -- the caller renders a commit message and calls `finalize`
/// to create the commit. `filter` narrows the set of hosts considered; any
/// limit name not in the tree is reported as an error before the plan is
/// built.
pub fn make_plan(store: &Store, tree_oid: Oid, filter: &HostFilter) -> Result<Option<DraftPlan>> {
    // Validate first so we don't do diff work or create commits from
    // invalid config.
    store.validate(tree_oid)?;

    let mut config_hosts = store.host_trees(tree_oid)?;
    filter.apply(&mut config_hosts)?;
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
    Ok(Some(DraftPlan {
        hosts,
        tree_oid,
        parents,
    }))
}

/// Maximum width for a deploy commit's subject line.
///
/// 52 is the conventional Git commit subject budget that lets `git log
/// --oneline` and most code-hosting UIs show the subject without truncation.
const SUBJECT_BUDGET: usize = 52;

/// Join names with commas and a final "and": `["a","b","c"]` → `"a, b, and c"`.
fn oxford_join(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [a] => a.to_string(),
        [a, b] => format!("{a} and {b}"),
        [front @ .., last] => format!("{}, and {last}", front.join(", ")),
    }
}

impl DraftPlan {
    /// The subject line for the deploy commit this draft will produce.
    ///
    /// Tries progressively terser candidates and returns the first that fits
    /// `SUBJECT_BUDGET`. Apps are kept in full longer than hosts -- given the
    /// app you usually know which hosts run it, but not the other way around.
    pub fn subject(&self) -> String {
        let unique_apps: BTreeSet<&str> = self
            .hosts
            .values()
            .flat_map(|h| h.apps.keys().map(String::as_str))
            .collect();
        let app_names: Vec<&str> = unique_apps.iter().copied().collect();
        let host_names: Vec<&str> = self.hosts.keys().map(|h| h.0.as_str()).collect();
        let n_apps = app_names.len();
        let n_hosts = host_names.len();

        let full_apps = oxford_join(&app_names);
        let full_hosts = oxford_join(&host_names);
        let count_apps = format!("{n_apps} {}", if n_apps == 1 { "app" } else { "apps" });
        let count_hosts = format!("{n_hosts} {}", if n_hosts == 1 { "host" } else { "hosts" });

        // "Update" only when every change is an update; otherwise the deploy
        // also adds or removes apps, and "Deploy" covers all three.
        let mut verb = "Update";
        for app in self.hosts.values().flat_map(|h| h.apps.values()) {
            if !matches!(app.diff, AppDiff::Update { .. }) {
                verb = "Deploy";
                break;
            }
        }

        [
            format!("{verb} {full_apps} on {full_hosts}"),
            format!("{verb} {full_apps} on {count_hosts}"),
            format!("{verb} {count_apps} on {count_hosts}"),
        ]
        .into_iter()
        .find(|s| s.len() <= SUBJECT_BUDGET)
        .expect("count-only candidate always fits the budget")
    }

    /// The full commit message for the deploy commit this draft will produce.
    ///
    /// The subject is a short summary produced by [`Self::subject`]. The body
    /// lists each affected host with its previous deployed commit (or "new
    /// host"), and one line per app change (`add`, `update`, or `remove`).
    pub fn commit_message(&self) -> String {
        use std::fmt::Write;

        let mut out = self.subject();
        out.push('\n');
        for (host, host_plan) in &self.hosts {
            match host_plan.expected_current {
                Some(oid) => writeln!(out, "\n{host} (changed from {oid})"),
                None => writeln!(out, "\n{host} (new host)"),
            }
            .expect("writes to String are infallible");
            for (app, app_plan) in &host_plan.apps {
                let action = match &app_plan.diff {
                    AppDiff::Add { .. } => "add",
                    AppDiff::Update { .. } => "update",
                    AppDiff::Remove { .. } => "remove",
                };
                writeln!(out, "    {action} {app}").expect("writes to String are infallible");
            }
        }
        out
    }

    /// Create the deploy commit and produce a finalized `Plan`.
    pub fn finalize(self, store: &Store) -> Result<Plan> {
        let message = self.commit_message();
        let commit = store.commit_tree(self.tree_oid, &self.parents, &message)?;
        Ok(Plan {
            hosts: self.hosts,
            commit,
        })
    }
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
        assert!(make_plan(&t.store, tree_oid, &HostFilter::All)?.is_none());
        Ok(())
    }

    #[test]
    fn plan_with_limit_includes_only_listed_hosts() -> Result<()> {
        let t = TestRepo::new();
        let c = t.commit(&[
            ("web1/app/conf", b"a"),
            ("web2/app/conf", b"b"),
            ("web3/app/conf", b"c"),
        ]);
        let tree_oid = t.get_commit_tree_oid(c);
        let filter = HostFilter::Only(vec!["web1".into(), "web3".into()]);
        let plan = make_plan(&t.store, tree_oid, &filter)?.expect("plan has changes");
        assert_eq!(
            plan.hosts.keys().cloned().collect::<Vec<_>>(),
            vec![Hostname::from("web1"), Hostname::from("web3")],
            "limit narrows the plan to exactly the listed hosts",
        );
        Ok(())
    }

    #[test]
    fn plan_errors_when_limit_contains_unknown_host() {
        let t = TestRepo::new();
        let c = t.commit(&[("web1/app/conf", b"a")]);
        let tree_oid = t.get_commit_tree_oid(c);
        let filter = HostFilter::Only(vec!["nope".into(), "also_nope".into()]);
        let result = make_plan(&t.store, tree_oid, &filter);
        match result {
            Err(Error::UnknownHosts(hosts)) => assert_eq!(
                hosts,
                vec![Hostname::from("nope"), Hostname::from("also_nope")],
                "all unknown hosts are reported",
            ),
            other => panic!("expected UnknownHosts, got {other:?}"),
        }
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
    fn unit_content_change_is_flagged_even_when_not_enabled() -> Result<()> {
        // A unit that is not enabled: editing its content changes nothing in
        // link/unlink or enable/restart, but the deploy must still flag it so it
        // issues a daemon-reload for systemd to pick it up.
        let d = diff_update(
            &[("web1/nginx/systemd/nginx.service", b"v1")],
            &[("web1/nginx/systemd/nginx.service", b"v2")],
        )?;
        assert!(d.units.content_changed, "content change is flagged");
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
    fn rollback_unsafe_when_quadlet_files_added() -> Result<()> {
        let d = diff_update(
            &[("web1/nginx/nginx.conf", b"v1")],
            &[
                ("web1/nginx/nginx.conf", b"v2"),
                ("web1/nginx/quadlets/nginx.container", b"[Container]"),
            ],
        )?;
        assert!(!d.is_rollback_safe());
        Ok(())
    }

    #[test]
    fn rollback_safe_when_quadlet_files_removed() -> Result<()> {
        let d = diff_update(
            &[
                ("web1/nginx/nginx.conf", b"v1"),
                ("web1/nginx/quadlets/nginx.container", b"[Container]"),
            ],
            &[("web1/nginx/nginx.conf", b"v2")],
        )?;
        assert!(d.is_rollback_safe());
        Ok(())
    }

    /// Content-only quadlet changes are rollback-safe: re-applying the
    /// previous commit restores the old quadlet file, which `daemon-reload`
    /// then re-renders to the prior generated unit.
    #[test]
    fn rollback_safe_when_quadlet_content_changes_without_add() -> Result<()> {
        let d = diff_update(
            &[
                ("web1/nginx/nginx.conf", b"v1"),
                ("web1/nginx/quadlets/nginx.container", b"Image=v1"),
            ],
            &[
                ("web1/nginx/nginx.conf", b"v1"),
                ("web1/nginx/quadlets/nginx.container", b"Image=v2"),
            ],
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

    fn draft(t: &TestRepo, commit: Oid) -> DraftPlan {
        let tree_oid = t.get_commit_tree_oid(commit);
        make_plan(&t.store, tree_oid, &HostFilter::All)
            .expect("plan succeeds")
            .expect("plan has changes")
    }

    fn subject_of(message: &str) -> &str {
        message.lines().next().expect("message has a subject line")
    }

    #[test]
    fn commit_subject_renders_one_app_one_host_descriptively() {
        let t = TestRepo::new();
        let c = t.commit(&[("web1/nginx/conf", b"v1")]);
        let message = draft(&t, c).commit_message();
        assert_eq!(subject_of(&message), "Deploy nginx on web1");
    }

    #[test]
    fn commit_subject_oxford_joins_apps_and_hosts_when_descriptive_form_fits() {
        let t = TestRepo::new();
        let c = t.commit(&[("web1/nginx/conf", b"v1"), ("web2/lego/conf", b"v1")]);
        let message = draft(&t, c).commit_message();
        assert_eq!(
            subject_of(&message),
            "Deploy lego and nginx on web1 and web2"
        );
    }

    #[test]
    fn commit_subject_uses_update_verb_when_all_changes_are_updates() {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/conf", b"v1")]);
        t.set_host_tracking_ref("web1", c1);
        let c2 = t.commit(&[("web1/nginx/conf", b"v2")]);
        let message = draft(&t, c2).commit_message();
        assert_eq!(subject_of(&message), "Update nginx on web1");
    }

    #[test]
    fn commit_subject_collapses_hosts_first_when_descriptive_form_too_long() {
        let t = TestRepo::new();
        // Three long hostnames overflow the 52-col budget when listed in
        // full, but the apps part is short, so apps stay full and hosts
        // collapse to a count.
        let c = t.commit(&[
            ("verylongname01/nginx/conf", b"v1"),
            ("verylongname02/nginx/conf", b"v1"),
            ("verylongname03/nginx/conf", b"v1"),
        ]);
        let message = draft(&t, c).commit_message();
        assert_eq!(subject_of(&message), "Deploy nginx on 3 hosts");
    }

    #[test]
    fn commit_subject_falls_back_to_count_form_when_apps_too_long() {
        let t = TestRepo::new();
        // Many long-named apps push past the budget even with a single
        // host, so apps collapse too.
        let c = t.commit(&[
            ("web1/superlongappname01/conf", b"v1"),
            ("web1/superlongappname02/conf", b"v1"),
            ("web1/superlongappname03/conf", b"v1"),
            ("web1/superlongappname04/conf", b"v1"),
        ]);
        let message = draft(&t, c).commit_message();
        assert_eq!(subject_of(&message), "Deploy 4 apps on 1 host");
    }

    #[test]
    fn commit_body_shows_previous_oid_when_host_was_already_deployed() {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/conf", b"v1")]);
        t.set_host_tracking_ref("web1", c1);
        let c2 = t.commit(&[("web1/nginx/conf", b"v2")]);
        let message = draft(&t, c2).commit_message();
        let expected = format!("web1 (changed from {c1})");
        assert!(
            message.contains(&expected),
            "message must contain {expected:?}, got:\n{message}",
        );
    }

    #[test]
    fn commit_body_marks_new_host_when_no_previous_deploy() {
        let t = TestRepo::new();
        let c = t.commit(&[("web1/nginx/conf", b"v1")]);
        let message = draft(&t, c).commit_message();
        assert!(
            message.contains("web1 (new host)"),
            "message must mark web1 as new host, got:\n{message}",
        );
    }

    #[test]
    fn commit_body_uses_distinct_verbs_for_add_update_remove() {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/conf", b"v1"), ("web1/lego/conf", b"v1")]);
        t.set_host_tracking_ref("web1", c1);
        let c2 = t.commit(&[("web1/nginx/conf", b"v2"), ("web1/foo/conf", b"v1")]);
        let message = draft(&t, c2).commit_message();
        assert!(message.contains("    add foo"), "got:\n{message}");
        assert!(message.contains("    update nginx"), "got:\n{message}");
        assert!(message.contains("    remove lego"), "got:\n{message}");
    }

    #[test]
    fn commit_message_renders_as_expected_for_mixed_scenario() {
        let t = TestRepo::new();
        // web1 was already deployed; web2 is brand new.
        let c1 = t.commit(&[("web1/nginx/conf", b"v1"), ("web1/lego/conf", b"v1")]);
        t.set_host_tracking_ref("web1", c1);
        // Target: web1 updates nginx, removes lego, adds foo;
        // web2 is a new host with one app.
        let c2 = t.commit(&[
            ("web1/nginx/conf", b"v2"),
            ("web1/foo/conf", b"v1"),
            ("web2/lego/conf", b"v1"),
        ]);
        // Commit oids depend on timestamps and are not stable across test
        // runs. Substitute a fixed oid so the assertion is deterministic and
        // the reader sees a realistic message.
        let message = draft(&t, c2)
            .commit_message()
            .replace(&c1.to_string(), "b8a4c3df2a1e6f5b9d8c0a7e1b3f4d2c8e5a6b7f");
        assert_eq!(
            message,
            "\
Deploy foo, lego, and nginx on web1 and web2

web1 (changed from b8a4c3df2a1e6f5b9d8c0a7e1b3f4d2c8e5a6b7f)
    add foo
    remove lego
    update nginx

web2 (new host)
    add lego
",
        );
    }
}

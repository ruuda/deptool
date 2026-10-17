// Deptool -- A declarative configuration deployment tool.
// Copyright 2026 Ruud van Asseldonk

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// A copy of the License has been included in the root of the repository.

//! Deployment plan: diff the desired config against each host's current state.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use git2::Oid;
use serde::{Deserialize, Serialize};

use crate::error::{Result, StoreError};
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
    /// Networkd config file changes.
    pub networkd: SubdirChanges,
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
            networkd,
            quadlets,
            symlinks,
            sysusers,
            units,
            unit_actions,
        } = self;
        networkd.is_rollback_safe()
            && quadlets.is_rollback_safe()
            && symlinks.is_rollback_safe()
            && sysusers.is_rollback_safe()
            && units.is_rollback_safe()
            && unit_actions.is_rollback_safe()
    }

    /// Move all entries from `other` into `self`, leaving `other` empty.
    pub fn append(&mut self, other: &mut Self) {
        self.networkd.append(&mut other.networkd);
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
            networkd: self.networkd,
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
pub struct CommittedHostPlan {
    pub apps: BTreeMap<String, AppPlan>,
    /// The commit to deploy to this host: its tree is the host's config
    /// tree, its parent the commit at `expected_current`.
    #[serde(with = "crate::prim::ser::oid")]
    pub commit: Oid,
    #[serde(with = "crate::prim::ser::oid_option")]
    pub expected_current: Option<Oid>,
    /// See [`HostPlan::is_rollback_safe`].
    pub is_rollback_safe: bool,
}

/// A plan whose deploy commits exist in the store, ready to deploy.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommittedPlan {
    pub hosts: BTreeMap<Hostname, CommittedHostPlan>,
}

impl CommittedPlan {
    /// The commit each host is expected to be at before this plan applies.
    pub fn expected_commits(&self) -> BTreeMap<Hostname, Option<Oid>> {
        self.hosts
            .iter()
            .map(|(host, host_plan)| (host.clone(), host_plan.expected_current))
            .collect()
    }
}

/// A plan that has not yet been committed to the store.
#[derive(Debug)]
pub struct Plan {
    pub hosts: BTreeMap<Hostname, HostPlan>,
}

/// One host's part of a `Plan`; `commit` turns it into a `CommittedHostPlan`.
#[derive(Debug)]
pub struct HostPlan {
    pub apps: BTreeMap<String, AppPlan>,
    pub expected_current: Option<Oid>,
    /// The host's config tree, which becomes the tree of the deploy commit.
    pub tree: Oid,
    /// Whether the host can be automatically rolled back on deploy failure.
    ///
    /// True when every app's `SystemDiff` is rollback-safe. See
    /// [`SystemDiff::is_rollback_safe`] for what that means.
    pub is_rollback_safe: bool,
}

/// Changes to a managed subdirectory between two commits, derived from Git
/// trees only.
///
/// The `networkd/`, `quadlets/`, `sysusers/`, and `systemd/` directories are
/// each symlinked into a system location; this captures which files appeared,
/// disappeared, and whether the directory's content changed at all. We don't
/// query the live system -- changes come from comparing the previous and
/// target trees.
#[derive(Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubdirChanges {
    /// Files newly provided by this app.
    pub link: Vec<String>,
    /// Files no longer provided by this app.
    pub unlink: Vec<String>,
    /// True iff the subtree differs between old and new tree: a file added,
    /// removed, or edited. Triggers the directory's systemd tool
    /// (`daemon-reload`, `systemd-sysusers`, `networkctl reload`); subsumes
    /// link and unlink, since any file added or removed also changes the
    /// subtree oid.
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
    /// Newly enabled units: `systemctl enable`, then `systemctl restart`.
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
/// directory (networkd configs, quadlets, sysusers configs, and systemd
/// units).
#[derive(Default)]
pub struct DesiredState {
    /// Networkd config files to symlink in the networkd directory.
    pub networkd: BTreeMap<String, PathBuf>,
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

/// Diff a managed subdirectory (networkd, quadlets, sysusers, units) between
/// two app trees into the files added, the files removed, and whether its
/// content changed at all.
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
        networkd: subdir_changes(store, old, new, "networkd")?,
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
    apps_dir: &Path,
    current_commit: Option<Oid>,
    target_commit: Option<Oid>,
) -> std::result::Result<(BTreeMap<String, AppDiff>, SystemDiff<PathBuf>), StoreError> {
    let current_apps = match current_commit {
        Some(oid) => store.commit_apps(oid)?,
        None => BTreeMap::new(),
    };
    let target_apps = match target_commit {
        Some(oid) => store.commit_apps(oid)?,
        None => BTreeMap::new(),
    };
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
/// Returns `None` if no hosts need changes. The returned `Plan` is not
/// yet committed -- the caller calls `commit` to create the deploy
/// commits.
pub fn make_plan(store: &Store, tree_oid: Oid) -> Result<Option<Plan>> {
    // Validate first so we don't do diff work or create commits from
    // invalid config.
    store.validate(tree_oid)?;

    let config_hosts = store.host_trees(tree_oid)?;
    let host_names: Vec<Hostname> = config_hosts.keys().cloned().collect();
    let host_refs = store.host_tracking_refs(&host_names)?;

    let mut hosts = BTreeMap::new();
    for (host, &tree) in &config_hosts {
        let (expected_current, current_apps) = match host_refs.get(host) {
            Some(hr) if hr.host_tree == tree => continue,
            Some(hr) => {
                let apps = tree_entries(&store.repo.find_tree(hr.host_tree)?);
                (Some(hr.commit), apps)
            }
            None => (None, BTreeMap::new()),
        };

        let target_apps = tree_entries(&store.repo.find_tree(tree)?);
        let mut apps = BTreeMap::new();
        for (name, diff) in diff_apps(&current_apps, &target_apps) {
            apps.insert(name, compute_app_plan(store, diff)?);
        }

        let is_rollback_safe = apps.values().all(|app| app.system.is_rollback_safe());
        hosts.insert(
            host.clone(),
            HostPlan {
                apps,
                expected_current,
                tree,
                is_rollback_safe,
            },
        );
    }

    if hosts.is_empty() {
        return Ok(None);
    }
    Ok(Some(Plan { hosts }))
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

/// The subject line for a host's deploy commit.
///
/// Lists the apps in full when that fits `SUBJECT_BUDGET`, otherwise counts
/// them. The count form is used even when it overflows, for a long
/// hostname.
fn subject(host: &Hostname, plan: &HostPlan) -> String {
    let app_names: Vec<&str> = plan.apps.keys().map(String::as_str).collect();
    let n_apps = app_names.len();

    // A plan can have no app changes at all: a fresh host with no apps
    // yet, or a tracking ref that recorded stray files from before
    // Deptool excluded them from the tree. Name only the host then,
    // rather than render an empty app list.
    if app_names.is_empty() {
        return format!("Deploy {host}");
    }

    // "Update" only when every change is an update; otherwise the deploy
    // also adds or removes apps, and "Deploy" covers all three.
    let mut verb = "Update";
    for app in plan.apps.values() {
        if !matches!(app.diff, AppDiff::Update { .. }) {
            verb = "Deploy";
            break;
        }
    }

    let descriptive = format!("{verb} {} on {host}", oxford_join(&app_names));
    if descriptive.len() <= SUBJECT_BUDGET {
        return descriptive;
    }
    let noun = if n_apps == 1 { "app" } else { "apps" };
    format!("{verb} {n_apps} {noun} on {host}")
}

/// A random 128-bit token, hex encoded, that identifies one deploy run.
///
/// Every commit of the run records it in a `Deploy-Run` trailer, so that
/// `git log --all --grep` finds them together. It is random rather than
/// derived from the content, because two runs may deploy the same trees.
fn deploy_run_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// The commit message for `host`'s deploy commit: its own subject line,
/// then the body that every commit of the run shares.
fn commit_message(host: &Hostname, plan: &HostPlan, body: &str) -> String {
    format!("{}\n\n{body}", subject(host, plan))
}

impl Plan {
    /// The body that every commit of one deploy run shares: the changes
    /// intended for every host, so a reader of one host's history sees the
    /// run's scope. It does not record whether the other hosts succeeded.
    fn run_body(&self, run: &str) -> String {
        use std::fmt::Write;

        let mut out = "Changes intended to be deployed in the same run:\n".to_string();
        for (host, host_plan) in &self.hosts {
            writeln!(out, "\n{host}").expect("writes to String are infallible");
            for (app, app_plan) in &host_plan.apps {
                let action = match &app_plan.diff {
                    AppDiff::Add { .. } => "add",
                    AppDiff::Update { .. } => "update",
                    AppDiff::Remove { .. } => "remove",
                };
                writeln!(out, "    {action} {app}").expect("writes to String are infallible");
            }
        }
        writeln!(out, "\nDeploy-Run: {run}").expect("writes to String are infallible");
        out
    }

    /// Create each host's deploy commit in the store.
    ///
    /// The commits of one run share a signature and a `Deploy-Run` token.
    pub fn commit(self, store: &Store) -> Result<CommittedPlan> {
        let sig = store.signature()?;
        let body = self.run_body(&deploy_run_token()?);
        let mut hosts = BTreeMap::new();
        for (host, plan) in self.hosts {
            let message = commit_message(&host, &plan, &body);
            let commit = store.commit_tree(&sig, plan.tree, plan.expected_current, &message)?;
            let committed = CommittedHostPlan {
                apps: plan.apps,
                commit,
                expected_current: plan.expected_current,
                is_rollback_safe: plan.is_rollback_safe,
            };
            hosts.insert(host, committed);
        }
        Ok(CommittedPlan { hosts })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Result;
    use crate::filter::HostFilter;
    use crate::testutil::{TempDir, TestRepo};

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
        t.mark_deployed(c1);

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
        t.mark_deployed(c1);

        let c2 = t.commit(&[("web1/nginx/conf", b"a")]);

        let plan = t.plan(c2);
        let apps = &plan.hosts[&"web1".into()].apps;
        assert_eq!(apps.len(), 1);
        assert!(matches!(apps["rofld"].diff, AppDiff::Remove { .. }));
        Ok(())
    }

    #[test]
    fn plan_removes_apps_of_emptied_host_and_initializes_fresh_empty_host() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/conf", b"v1")]);
        t.mark_deployed(c1);

        // web1 was deployed to and its last app deleted; web2 is an empty
        // host dir that was never deployed to.
        let config = TempDir::new("config");
        std::fs::create_dir_all(config.path().join("web1"))?;
        std::fs::create_dir_all(config.path().join("web2"))?;
        let tree_oid = t.store.build_tree(config.path(), &HostFilter::All)?;

        let plan = make_plan(&t.store, tree_oid)?.expect("plan has changes");
        let apps = &plan.hosts[&"web1".into()].apps;
        assert_eq!(apps.len(), 1);
        assert!(matches!(apps["nginx"].diff, AppDiff::Remove { .. }));

        // The fresh host deploys with nothing on it, which records its
        // tracking ref, so status, sync, and deploy agree afterwards.
        let web2 = &plan.hosts[&"web2".into()];
        assert!(web2.apps.is_empty());
        assert!(web2.expected_current.is_none());
        Ok(())
    }

    #[test]
    fn plan_includes_new_host_alongside_up_to_date_host() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/conf", b"a")]);
        t.mark_deployed(c1);

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
        t.mark_deployed(c1);

        let tree_oid = t.get_commit_tree_oid(c1);
        assert!(make_plan(&t.store, tree_oid)?.is_none());
        Ok(())
    }

    /// Plan a single-host update and return the resulting SystemDiff.
    fn diff_update(before: &[(&str, &[u8])], after: &[(&str, &[u8])]) -> Result<SystemDiff> {
        let t = TestRepo::new();
        let c1 = t.commit(before);
        t.mark_deployed(c1);
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
    fn rollback_unsafe_when_networkd_files_added() -> Result<()> {
        let d = diff_update(
            &[("web1/nginx/nginx.conf", b"v1")],
            &[
                ("web1/nginx/nginx.conf", b"v2"),
                ("web1/nginx/networkd/10-lan.network", b"[Match]"),
            ],
        )?;
        assert!(!d.is_rollback_safe());
        Ok(())
    }

    #[test]
    fn rollback_safe_when_networkd_files_removed() -> Result<()> {
        let d = diff_update(
            &[
                ("web1/nginx/nginx.conf", b"v1"),
                ("web1/nginx/networkd/10-lan.network", b"[Match]"),
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
        t.mark_deployed(c1);
        let c2 = t.commit(&[("web1/nginx/nginx.conf", b"v2")]);

        let plan = t.plan(c2);
        assert!(plan.hosts[&"web1".into()].is_rollback_safe);
        Ok(())
    }

    #[test]
    fn host_rollback_safe_when_app_added_without_system_effects() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/conf", b"v1")]);
        t.mark_deployed(c1);
        let c2 = t.commit(&[("web1/nginx/conf", b"v1"), ("web1/rofld/conf", b"v1")]);

        let plan = t.plan(c2);
        assert!(plan.hosts[&"web1".into()].is_rollback_safe);
        Ok(())
    }

    #[test]
    fn host_rollback_unsafe_when_any_app_is_unsafe() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/conf", b"v1")]);
        t.mark_deployed(c1);
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
    fn commit_parents_each_host_commit_on_its_tracking_ref() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/app/conf", b"v1"), ("web2/app/conf", b"v1")]);
        let deployed = t.mark_deployed(c1);
        // web3 is new, so its commit has no parent.
        let c2 = t.commit(&[
            ("web1/app/conf", b"v2"),
            ("web2/app/conf", b"v2"),
            ("web3/app/conf", b"v1"),
        ]);

        let plan = t.plan(c2);

        let config_tree = t.store.get_commit_tree(c2)?;
        for (host, host_plan) in &plan.hosts {
            let commit = t.store.repo.find_commit(host_plan.commit)?;
            let host_tree = config_tree.get_name(&host.0).expect("config has the host");
            assert_eq!(commit.tree_id(), host_tree.id(), "{host}");
            let parent = deployed.hosts.get(host).map(|d| d.commit);
            assert_eq!(commit.parent_ids().next(), parent, "{host}");
        }
        Ok(())
    }

    #[test]
    fn commit_stamps_the_commits_of_a_run_with_one_token() -> Result<()> {
        let t = TestRepo::new();
        let c = t.commit(&[("web1/app/conf", b"v1"), ("web2/app/conf", b"v1")]);
        let plan = t.plan(c);

        let trailer_of = |host: &str| -> String {
            let commit = t
                .store
                .repo
                .find_commit(plan.hosts[&host.into()].commit)
                .expect("commit exists");
            let message = commit.message().expect("message is utf-8");
            message
                .lines()
                .last()
                .expect("message has lines")
                .to_string()
        };
        let trailer = trailer_of("web1");
        assert_eq!(trailer, trailer_of("web2"));
        let token = trailer
            .strip_prefix("Deploy-Run: ")
            .expect("trailer records the run");
        assert_eq!(token.len(), 32);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()), "{token}");
        Ok(())
    }

    const RUN: &str = "3f0a9c2e7d1b4e6a8c5f0d2b9e7a1c4d";

    /// The commit message for `host` when deploying `commit`'s tree.
    fn message_for(t: &TestRepo, commit: Oid, host: &str) -> String {
        let tree_oid = t.get_commit_tree_oid(commit);
        let plan = make_plan(&t.store, tree_oid)
            .expect("plan succeeds")
            .expect("plan has changes");
        let host = Hostname::from(host);
        commit_message(&host, &plan.hosts[&host], &plan.run_body(RUN))
    }

    fn subject_of(message: &str) -> &str {
        message.lines().next().expect("message has a subject line")
    }

    #[test]
    fn commit_subject_renders_one_app_one_host_descriptively() {
        let t = TestRepo::new();
        let c = t.commit(&[("web1/nginx/conf", b"v1")]);
        assert_eq!(
            subject_of(&message_for(&t, c, "web1")),
            "Deploy nginx on web1"
        );
    }

    #[test]
    fn commit_subject_lists_the_apps_of_its_own_host() {
        let t = TestRepo::new();
        let c = t.commit(&[
            ("web1/lego/conf", b"v1"),
            ("web1/nginx/conf", b"v1"),
            ("web2/lego/conf", b"v1"),
        ]);
        assert_eq!(
            subject_of(&message_for(&t, c, "web1")),
            "Deploy lego and nginx on web1"
        );
        assert_eq!(
            subject_of(&message_for(&t, c, "web2")),
            "Deploy lego on web2"
        );
    }

    #[test]
    fn commit_subject_uses_update_verb_when_all_changes_are_updates() {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/conf", b"v1")]);
        t.mark_deployed(c1);
        let c2 = t.commit(&[("web1/nginx/conf", b"v2")]);
        assert_eq!(
            subject_of(&message_for(&t, c2, "web1")),
            "Update nginx on web1"
        );
    }

    #[test]
    fn commit_subject_counts_apps_when_listing_them_overflows_the_budget() {
        let t = TestRepo::new();
        let c = t.commit(&[
            ("web1/superlongappname01/conf", b"v1"),
            ("web1/superlongappname02/conf", b"v1"),
            ("web1/superlongappname03/conf", b"v1"),
        ]);
        assert_eq!(
            subject_of(&message_for(&t, c, "web1")),
            "Deploy 3 apps on web1"
        );
    }

    #[test]
    fn commit_subject_keeps_the_hostname_when_even_the_count_form_overflows() {
        let t = TestRepo::new();
        let host = "a-hostname-longer-than-the-subject-budget.example.com";
        let path = format!("{host}/nginx/conf");
        let c = t.commit(&[(path.as_str(), b"v1")]);
        assert_eq!(
            subject_of(&message_for(&t, c, host)),
            format!("Deploy 1 app on {host}")
        );
    }

    #[test]
    fn commit_subject_names_host_when_plan_has_no_app_changes() -> Result<()> {
        let t = TestRepo::new();
        // A fresh empty host dir plans a deploy with no app changes at all.
        let config = TempDir::new("config");
        std::fs::create_dir_all(config.path().join("web2"))?;
        let tree_oid = t.store.build_tree(config.path(), &HostFilter::All)?;

        let plan = make_plan(&t.store, tree_oid)?.expect("plan has changes");
        let web2 = Hostname::from("web2");
        let message = commit_message(&web2, &plan.hosts[&web2], &plan.run_body(RUN));
        assert_eq!(subject_of(&message), "Deploy web2");
        Ok(())
    }

    #[test]
    fn commit_message_renders_as_expected_for_mixed_scenario() {
        let t = TestRepo::new();
        // web1 was already deployed; web2 is brand new.
        let c1 = t.commit(&[("web1/nginx/conf", b"v1"), ("web1/lego/conf", b"v1")]);
        t.mark_deployed(c1);
        // Target: web1 updates nginx, removes lego, adds foo;
        // web2 is a new host with one app.
        let c2 = t.commit(&[
            ("web1/nginx/conf", b"v2"),
            ("web1/foo/conf", b"v1"),
            ("web2/lego/conf", b"v1"),
        ]);
        // Each host's commit has its own subject; the body is the run's.
        let body = "\
Changes intended to be deployed in the same run:

web1
    add foo
    remove lego
    update nginx

web2
    add lego

Deploy-Run: 3f0a9c2e7d1b4e6a8c5f0d2b9e7a1c4d
";
        assert_eq!(
            message_for(&t, c2, "web1"),
            format!("Deploy foo, lego, and nginx on web1\n\n{body}"),
        );
        assert_eq!(
            message_for(&t, c2, "web2"),
            format!("Deploy lego on web2\n\n{body}"),
        );
    }
}

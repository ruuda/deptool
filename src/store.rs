// Deptool -- A declarative configuration deployment tool.
// Copyright 2026 Ruud van Asseldonk

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// A copy of the License has been included in the root of the repository.

//! Git store operations: commit, checkout, and ref management.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use git2::{Oid, Repository, Signature, Tree};

use serde::Deserialize;

use crate::error::StoreError;
use crate::filter::HostFilter;
use crate::prim::Hostname;

type Result<T> = std::result::Result<T, StoreError>;

/// The version of the store format this Deptool reads and writes.
///
/// Every host store records the version it was last written with, so a
/// Deptool that opens a store written in a newer format refuses it instead
/// of misinterpreting its contents.
pub const STORE_VERSION: u32 = 2;

/// The format of Deptool 1.2 and earlier: every commit's tree holds the
/// config of the whole cluster, one entry per host. The agent converts a
/// store in this format to the current one when it takes the deploy lock.
pub const STORE_VERSION_CLUSTER_TREE: u32 = 1;

/// Body of the commit the agent creates when converting a cluster-tree
/// commit, below a subject that includes the hostname.
const CONVERSION_MESSAGE_BODY: &str = "\
Deptool 1.3 and later record one commit per host. This tree is the
entry for this host in the parent's cluster-wide tree, unchanged.
";

/// Per-app manifest declaring runtime state that deptool should manage.
///
/// Unknown fields are rejected so typos and stale keys are caught early.
/// Use a separate file for custom metadata.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    #[serde(default)]
    pub systemd: SystemdConfig,
    /// Symlinks to create on the host: absolute target path → relative source
    /// path within the app tree.
    #[serde(default)]
    pub symlinks: BTreeMap<String, String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemdConfig {
    #[serde(default)]
    pub units_enabled: Vec<String>,
}

/// A host's tracking state as known by the driver.
pub struct HostRef {
    /// The commit deployed to this host (`refs/remotes/{host}`).
    pub commit: Oid,
    /// The host's tree in that commit, see [`Store::deployed_host_tree`].
    pub host_tree: Oid,
}

/// A bare Git repository used as the config store.
pub struct Store {
    pub repo: Repository,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Err(StoreError::NotFound(path.to_path_buf()));
        }
        Ok(Store {
            repo: Repository::open(path)?,
        })
    }

    /// Open or init a bare repo with reflogs enabled and owner-only perms.
    ///
    /// The store carries the full cluster config and history -- an
    /// unprivileged user that escaped a sandbox on a host must not be
    /// able to read it.
    pub fn open_or_init(path: &Path) -> Result<Self> {
        let repo = match Repository::open(path) {
            Ok(r) => r,
            Err(_) => {
                let repo = Repository::init_bare(path)?;
                // Bare repos don't create reflogs by default.
                repo.config()?.set_bool("core.logAllRefUpdates", true)?;
                repo
            }
        };
        // Tightens 0o755 dirs from older installs.
        // TODO: Move inside the init branch once every host has run
        // this binary at least once.
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        Ok(Store { repo })
    }

    /// The on-disk path to the bare repo directory.
    pub fn path(&self) -> &Path {
        self.repo.path()
    }

    /// Read a string from the repo's git config, `None` when unset.
    fn get_config_string(&self, key: &str) -> Result<Option<String>> {
        let config = self.repo.config()?;
        match config.get_string(key) {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(e) => Err(StoreError::Git(e)),
        }
    }

    fn set_config_string(&self, key: &str, value: &str) -> Result<()> {
        let mut config = self.repo.config()?;
        config.set_str(key, value)?;
        Ok(())
    }

    /// The config tree directory recorded by the most recent deploy or sync.
    pub fn get_default_cluster(&self) -> Result<Option<PathBuf>> {
        Ok(self
            .get_config_string("deptool.cluster")?
            .map(PathBuf::from))
    }

    /// Remember `path` as the default config tree for subsequent runs.
    pub fn set_default_cluster(&self, path: &Path) -> Result<()> {
        let s = path.to_str().expect("cluster path is valid UTF-8");
        self.set_config_string("deptool.cluster", s)
    }

    /// The hostname recorded by the most recent apply of Deptool 1.1 or 1.2.
    ///
    /// Those versions found a host's config in the cluster tree by name,
    /// so they refused to deploy after a rename. Converting such a store
    /// looks the host up by name one last time, and checks this record
    /// against the current `/etc/hostname` first. `None` on hosts deployed
    /// before recording existed, and on hosts provisioned by 1.3 or later.
    pub fn get_deployed_hostname(&self) -> Result<Option<Hostname>> {
        Ok(self.get_config_string("deptool.hostname")?.map(Hostname))
    }

    /// Record `hostname` as the name this host's state was deployed under,
    /// as the apply of Deptool 1.1 and 1.2 did. Only tests write it now.
    pub fn set_deployed_hostname(&self, hostname: &Hostname) -> Result<()> {
        self.set_config_string("deptool.hostname", &hostname.0)
    }

    /// The store format version recorded on this host, `None` for stores
    /// last written before versions were recorded.
    pub fn get_store_version(&self) -> Result<Option<u32>> {
        Ok(self
            .get_config_string("deptool.storeVersion")?
            .map(|s| s.parse().expect("store version is an integer")))
    }

    pub fn set_store_version(&self, version: u32) -> Result<()> {
        self.set_config_string("deptool.storeVersion", &version.to_string())
    }

    /// Read the host-local `refs/heads/current` commit, if it exists.
    pub fn current_commit(&self) -> Option<Oid> {
        self.ref_commit("refs/heads/current")
    }

    /// Read the host-local `refs/heads/target` commit, if it exists.
    ///
    /// Diverges from `current` when an apply started but did not complete.
    pub fn target_commit(&self) -> Option<Oid> {
        self.ref_commit("refs/heads/target")
    }

    fn ref_commit(&self, refname: &str) -> Option<Oid> {
        self.repo.find_reference(refname).ok().map(|r| {
            r.peel_to_commit()
                .expect("deptool refs point to commits")
                .id()
        })
    }

    /// Get the tree for a commit.
    pub fn get_commit_tree(&self, commit_oid: Oid) -> Result<Tree<'_>> {
        Ok(self.repo.find_commit(commit_oid)?.tree()?)
    }

    /// Recursively build a Git tree from the host dirs that `filter` selects.
    ///
    /// Excluded host dirs are never read, so a broken config for one host does
    /// not block commands limited to others. Skipping them is safe because
    /// validation has no rules across hosts. An entirely empty config directory
    /// is valid (no hosts configured) and yields the empty tree.
    pub fn build_tree(&self, dir: &Path, filter: &HostFilter) -> Result<Oid> {
        let depth_root = 0;
        build_tree_recursive(&self.repo, filter, dir, depth_root)
    }

    /// The identity to commit as: the repo's configured user, or Deptool's
    /// own when none is configured, as on a host.
    pub fn signature(&self) -> Result<Signature<'static>> {
        match self.repo.signature() {
            Ok(sig) => Ok(sig),
            Err(..) => Ok(Signature::now("deptool", "bot@deptool")?),
        }
    }

    /// Create a commit with the given tree, parent, and message.
    ///
    /// Does not update any ref -- the caller decides where to point. The
    /// signature is a parameter so that the commits of one deploy run all
    /// record the same time.
    pub fn commit_tree(
        &self,
        sig: &Signature,
        tree_oid: Oid,
        parent: Option<Oid>,
        message: &str,
    ) -> Result<Oid> {
        let tree = self.repo.find_tree(tree_oid)?;
        let parent = match parent {
            Some(oid) => Some(self.repo.find_commit(oid)?),
            None => None,
        };
        let parents: Vec<_> = parent.iter().collect();
        Ok(self.repo.commit(None, sig, sig, message, &tree, &parents)?)
    }

    /// The host's tree in a commit deployed to it: the commit's tree, or for
    /// commits of Deptool 1.2 and older, which hold the whole cluster, the
    /// entry named after the host. TODO: Delete the cluster-tree case once
    /// every host has been deployed to by 1.3 or later; until then an app
    /// named after its host is mistaken for the old format.
    pub fn deployed_host_tree(&self, commit_oid: Oid, host: &Hostname) -> Result<Oid> {
        let tree = self.get_commit_tree(commit_oid)?;
        match tree.get_name(&host.0) {
            Some(entry) => Ok(entry.id()),
            None => Ok(tree.id()),
        }
    }

    /// A per-host commit with the host's entry of a cluster-tree commit,
    /// with that commit as parent so its history stays reachable.
    ///
    /// `None` when the tree has no entry for the host: it was renamed after
    /// the deploy. A conversion converts as its parent, so a lock that was
    /// interrupted after moving one ref but before recording the version
    /// brings every ref to one commit on the next attempt.
    pub fn convert_cluster_commit(
        &self,
        sig: &Signature,
        host: &Hostname,
        commit_oid: Oid,
    ) -> Result<Option<Oid>> {
        let mut cluster = self.repo.find_commit(commit_oid)?;
        if let Ok(parent) = cluster.parent(0) {
            let converted_from = parent.tree()?.get_name(&host.0).map(|e| e.id());
            if converted_from == Some(cluster.tree_id()) {
                cluster = parent;
            }
        }
        let host_tree = match cluster.tree()?.get_name(&host.0) {
            Some(entry) => entry.id(),
            None => return Ok(None),
        };
        let subject = format!("Convert {host} history to the per-host format");
        let message = format!("{subject}\n\n{CONVERSION_MESSAGE_BODY}");
        let parent = Some(cluster.id());
        Ok(Some(self.commit_tree(sig, host_tree, parent, &message)?))
    }

    /// Map hostnames to their subtree OIDs from a config tree.
    pub fn host_trees(&self, tree_oid: Oid) -> Result<BTreeMap<Hostname, Oid>> {
        let tree = self.repo.find_tree(tree_oid)?;
        Ok(tree_entries(&tree)
            .into_iter()
            .map(|(name, oid)| (Hostname(name), oid))
            .collect())
    }

    /// Look up tracking refs for the given hosts.
    ///
    /// For each host that has a `refs/remotes/{host}` ref, returns the
    /// deployed commit and the host's tree in it.
    pub fn host_tracking_refs(&self, hosts: &[Hostname]) -> Result<BTreeMap<Hostname, HostRef>> {
        let mut result = BTreeMap::new();
        for host in hosts {
            let refname = format!("refs/remotes/{host}");
            let commit = match self.repo.find_reference(&refname) {
                Ok(r) => r.peel_to_commit()?.id(),
                Err(_) => continue,
            };
            let host_tree = self.deployed_host_tree(commit, host)?;
            result.insert(host.clone(), HostRef { commit, host_tree });
        }
        Ok(result)
    }

    /// Check out an app's tree from a deployed commit into a target directory.
    pub fn checkout_app(&self, commit_oid: Oid, app: &str, target: &Path) -> Result<()> {
        let tree = self.get_commit_tree(commit_oid)?;
        let entry = tree.get_path(Path::new(app))?;
        let subtree = self.repo.find_tree(entry.id())?;
        let mut cb = git2::build::CheckoutBuilder::new();
        cb.target_dir(target).force();
        self.repo
            .checkout_tree(subtree.as_object(), Some(&mut cb))?;
        make_files_readonly(target)?;
        Ok(())
    }

    /// Delete a reference if it exists. Idempotent: a missing ref is not an error.
    pub fn delete_ref(&self, refname: &str) -> Result<()> {
        match self.repo.find_reference(refname) {
            Ok(mut r) => {
                r.delete()?;
                Ok(())
            }
            Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(()),
            Err(e) => Err(StoreError::Git(e)),
        }
    }

    pub fn set_ref(&self, refname: &str, oid: Oid, reason: RefUpdate) -> Result<()> {
        let reflog_msg = match reason {
            RefUpdate::SetTarget { operator } => {
                format!("deploy by {operator}: set target")
            }
            RefUpdate::SetCurrent { operator } => {
                format!("deploy by {operator}: set current")
            }
            RefUpdate::Rollback { operator } => {
                format!("deploy by {operator}: rollback after failed apply")
            }
            RefUpdate::Convert { operator } => {
                format!("deploy by {operator}: convert to per-host format")
            }
            RefUpdate::ApplyComplete => "deploy: host applied, update tracking ref".to_string(),
            RefUpdate::FetchStale => "deploy: fetched stale commit from host".to_string(),
        };
        let force = true;
        self.repo.reference(refname, oid, force, &reflog_msg)?;
        Ok(())
    }

    /// Build a packfile of objects reachable from a commit.
    ///
    /// If `have_commit` is provided, objects reachable from it are excluded,
    /// so only the delta between the two commits is packed.
    pub fn create_pack(&self, want_commit: Oid, have_commit: Option<Oid>) -> Result<Vec<u8>> {
        let mut walk = self.repo.revwalk()?;
        walk.push(want_commit)?;
        if let Some(have) = have_commit {
            walk.hide(have)?;
        }
        let mut builder = self.repo.packbuilder()?;
        builder.insert_walk(&mut walk)?;
        let mut buf = git2::Buf::new();
        builder.write_buf(&mut buf)?;
        Ok(buf.to_vec())
    }

    /// Write raw packfile bytes into the repository's object database.
    pub fn write_pack(&self, data: &[u8]) -> Result<()> {
        let odb = self.repo.odb()?;
        let mut writer = odb.packwriter()?;
        std::io::Write::write_all(&mut writer, data)?;
        writer.commit()?;
        Ok(())
    }

    /// Path to the deploy lock file in the repo directory.
    pub fn get_lock_file_path(&self) -> PathBuf {
        self.path().join("deptool.lock")
    }

    /// Relative paths of all files (blobs) in an app tree's named
    /// subdirectory, recursively.
    ///
    /// A nested directory (e.g. a systemd `<unit>.d` drop-in directory)
    /// appears only as a path prefix; the directory itself is never an entry.
    /// Returns an empty set if the subdirectory doesn't exist.
    pub fn subdir_entries(&self, app_tree_oid: Oid, subdir: &str) -> Result<BTreeSet<String>> {
        let tree = self.repo.find_tree(app_tree_oid)?;
        let entry = match tree.get_name(subdir) {
            Some(entry) => entry,
            None => return Ok(BTreeSet::new()),
        };
        let subtree = self.repo.find_tree(entry.id())?;
        let mut names = BTreeSet::new();
        self.collect_blobs(&subtree, &mut String::new(), &mut names)?;
        Ok(names)
    }

    /// Collect the path of every blob under `tree`, prefixing each with
    /// `prefix` (the slash-terminated path from the subdirectory to `tree`).
    ///
    /// `prefix` is a reused buffer: each level appends its name and truncates
    /// back on the way out, so recursion doesn't allocate a string per level.
    fn collect_blobs(
        &self,
        tree: &Tree,
        prefix: &mut String,
        names: &mut BTreeSet<String>,
    ) -> Result<()> {
        for entry in tree.iter() {
            let name = entry.name().ok_or(StoreError::NonUtf8FileName)?;
            match entry.kind() {
                Some(git2::ObjectType::Tree) => {
                    let subtree = self.repo.find_tree(entry.id())?;
                    let len = prefix.len();
                    prefix.push_str(name);
                    prefix.push('/');
                    self.collect_blobs(&subtree, prefix, names)?;
                    prefix.truncate(len);
                }
                Some(git2::ObjectType::Blob) => {
                    names.insert(format!("{prefix}{name}"));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The app trees of a deployed commit, by app name.
    pub fn commit_apps(&self, commit_oid: Oid) -> Result<BTreeMap<String, Oid>> {
        Ok(tree_entries(&self.get_commit_tree(commit_oid)?))
    }

    /// Collect desired symlinks for an app subdirectory across all apps.
    ///
    /// Maps each filename in `subdir` to the absolute symlink target path
    /// under `apps_dir/<app>/current/<subdir>/`.
    fn desired_subdir_symlinks(
        &self,
        commit_oid: Oid,
        apps_dir: &Path,
        subdir: &str,
    ) -> Result<BTreeMap<String, PathBuf>> {
        let mut result = BTreeMap::new();
        for (app, app_tree_oid) in &self.commit_apps(commit_oid)? {
            for name in self.subdir_entries(*app_tree_oid, subdir)? {
                // `name` may be a slash-separated relative path (a drop-in file
                // inside a `<unit>.d` directory); `join` extends the path.
                let target = apps_dir.join(app).join("current").join(subdir).join(&name);
                result.insert(name, target);
            }
        }
        Ok(result)
    }

    /// Oid of an app tree's named subtree, or `None` if there is no such
    /// subdirectory. Differs between two trees whenever any file under it was
    /// added, removed, or edited.
    pub fn subtree_oid(&self, app_tree_oid: Oid, subdir: &str) -> Result<Option<Oid>> {
        let tree = self.repo.find_tree(app_tree_oid)?;
        Ok(tree.get_name(subdir).map(|e| e.id()))
    }

    /// Collect desired unit symlinks for a host.
    pub fn desired_units(
        &self,
        commit_oid: Oid,
        apps_dir: &Path,
    ) -> Result<BTreeMap<String, PathBuf>> {
        self.desired_subdir_symlinks(commit_oid, apps_dir, "systemd")
    }

    /// List all unit files in an app tree's `systemd/` directory.
    pub fn app_units(&self, app_tree_oid: Oid) -> Result<BTreeSet<String>> {
        self.subdir_entries(app_tree_oid, "systemd")
    }

    /// List all sysuser config files in an app tree's `sysusers/` directory.
    pub fn app_sysusers(&self, app_tree_oid: Oid) -> Result<BTreeSet<String>> {
        self.subdir_entries(app_tree_oid, "sysusers")
    }

    /// Collect desired sysusers symlinks for a host.
    pub fn desired_sysusers(
        &self,
        commit_oid: Oid,
        apps_dir: &Path,
    ) -> Result<BTreeMap<String, PathBuf>> {
        self.desired_subdir_symlinks(commit_oid, apps_dir, "sysusers")
    }

    /// List all quadlet files in an app tree's `quadlets/` directory.
    pub fn app_quadlets(&self, app_tree_oid: Oid) -> Result<BTreeSet<String>> {
        self.subdir_entries(app_tree_oid, "quadlets")
    }

    /// Collect desired quadlet symlinks for a host.
    pub fn desired_quadlets(
        &self,
        commit_oid: Oid,
        apps_dir: &Path,
    ) -> Result<BTreeMap<String, PathBuf>> {
        self.desired_subdir_symlinks(commit_oid, apps_dir, "quadlets")
    }

    /// List all networkd config files in an app tree's `networkd/` directory.
    pub fn app_networkd(&self, app_tree_oid: Oid) -> Result<BTreeSet<String>> {
        self.subdir_entries(app_tree_oid, "networkd")
    }

    /// Collect desired networkd symlinks for a host.
    pub fn desired_networkd(
        &self,
        commit_oid: Oid,
        apps_dir: &Path,
    ) -> Result<BTreeMap<String, PathBuf>> {
        self.desired_subdir_symlinks(commit_oid, apps_dir, "networkd")
    }

    /// Read the enabled units from an app's manifest as a set.
    pub fn enabled_units(&self, app_tree_oid: Oid) -> Result<BTreeSet<String>> {
        let manifest = self.read_manifest(app_tree_oid)?;
        Ok(manifest.systemd.units_enabled.into_iter().collect())
    }

    /// Collect manifest symlinks for a host, resolved to absolute paths.
    ///
    /// Maps each absolute link path to the absolute source path under
    /// `apps_dir/<app>/current/`.
    pub fn desired_symlinks(
        &self,
        commit_oid: Oid,
        apps_dir: &Path,
    ) -> Result<BTreeMap<PathBuf, PathBuf>> {
        let mut result = BTreeMap::new();
        for (app, app_tree_oid) in &self.commit_apps(commit_oid)? {
            let manifest = self.read_manifest(*app_tree_oid)?;
            for (link, source) in &manifest.symlinks {
                let source_path = apps_dir.join(app).join("current").join(source);
                result.insert(link.into(), source_path);
            }
        }
        Ok(result)
    }

    /// Validate all hosts in a config tree.
    pub fn validate(&self, tree_oid: Oid) -> Result<()> {
        let tree = self.repo.find_tree(tree_oid)?;
        for host_tree_oid in tree_entries(&tree).into_values() {
            self.validate_app_manifests(host_tree_oid)?;
        }
        Ok(())
    }

    /// Validate manifests for all apps on a single host.
    fn validate_app_manifests(&self, host_tree_oid: Oid) -> Result<()> {
        let apps = tree_entries(&self.repo.find_tree(host_tree_oid)?);

        // Track networkd config, quadlet, sysuser, unit, and symlink-target
        // conflicts across apps. Subdir-based dedup checks run first
        // (canonical order: networkd, quadlets, sysusers, units), then the
        // manifest-symlink loop.
        let mut networkd_owners: BTreeMap<String, String> = BTreeMap::new();
        let mut quadlet_owners: BTreeMap<String, String> = BTreeMap::new();
        let mut symlink_owners: BTreeMap<String, String> = BTreeMap::new();
        let mut sysuser_owners: BTreeMap<String, String> = BTreeMap::new();
        let mut unit_owners: BTreeMap<String, String> = BTreeMap::new();

        for (app, app_tree_oid) in &apps {
            let manifest = self.read_manifest(*app_tree_oid)?;
            let app_tree = self.repo.find_tree(*app_tree_oid)?;

            // Check for duplicate networkd config files across apps.
            for name in self.app_networkd(*app_tree_oid)? {
                if let Some(other) = networkd_owners.insert(name.clone(), app.clone()) {
                    return Err(StoreError::InvalidConfig(format!(
                        "networkd config {name} provided by both {other} and {app}",
                    )));
                }
            }

            // Check for duplicate quadlet files across apps.
            for name in self.app_quadlets(*app_tree_oid)? {
                if let Some(other) = quadlet_owners.insert(name.clone(), app.clone()) {
                    return Err(StoreError::InvalidConfig(format!(
                        "quadlet {name} provided by both {other} and {app}",
                    )));
                }
            }

            // Check for duplicate sysuser config files across apps.
            for name in self.app_sysusers(*app_tree_oid)? {
                if let Some(other) = sysuser_owners.insert(name.clone(), app.clone()) {
                    return Err(StoreError::InvalidConfig(format!(
                        "sysuser config {name} provided by both {other} and {app}",
                    )));
                }
            }

            // Check for duplicate unit files across apps.
            for name in self.app_units(*app_tree_oid)? {
                if let Some(other) = unit_owners.insert(name.clone(), app.clone()) {
                    return Err(StoreError::InvalidConfig(format!(
                        "unit {name} provided by both {other} and {app}",
                    )));
                }
            }

            for (target, source) in &manifest.symlinks {
                if !target.starts_with('/') {
                    return Err(StoreError::InvalidConfig(format!(
                        "app {app} symlink target {target} is not an absolute path",
                    )));
                }
                // Manifest symlinks must not target directories that are
                // managed by reconcile_managed_symlinks, because that
                // function removes any symlink pointing into apps_dir
                // that isn't in its desired set -- it can't distinguish
                // manifest symlinks from managed ones.
                for managed_dir in [
                    "/etc/systemd/network",
                    "/etc/containers/systemd",
                    "/etc/sysusers.d",
                    "/etc/systemd/system",
                ] {
                    if target.starts_with(&format!("{managed_dir}/")) {
                        return Err(StoreError::InvalidConfig(format!(
                            "app {app} symlink targets {managed_dir}/ which is \
                             managed automatically; use the networkd/, quadlets/, \
                             sysusers/, or systemd/ app directory instead",
                        )));
                    }
                }
                if app_tree.get_path(Path::new(source)).is_err() {
                    return Err(StoreError::InvalidConfig(format!(
                        "app {app} symlink source {source} does not exist in the app tree",
                    )));
                }
                // Check for duplicate symlink targets across apps.
                if let Some(other) = symlink_owners.insert(target.clone(), app.clone()) {
                    return Err(StoreError::InvalidConfig(format!(
                        "symlink {target} provided by both {other} and {app}",
                    )));
                }
            }
        }
        Ok(())
    }

    /// Read the manifest from an app tree's `manifest.json`.
    ///
    /// Returns a default manifest if the app has no `manifest.json`.
    pub fn read_manifest(&self, app_tree_oid: Oid) -> Result<Manifest> {
        let tree = self.repo.find_tree(app_tree_oid)?;
        let entry = match tree.get_name("manifest.json") {
            Some(entry) => entry,
            None => return Ok(Manifest::default()),
        };
        let blob = self.repo.find_blob(entry.id())?;
        Ok(serde_json::from_slice(blob.content())?)
    }
}

/// Drop write bits from every regular file under `dir`, recursively.
///
/// Checked-out files are not meant to be edited in place; users should deploy
/// a new version instead. Directories stay writable so that `remove_dir_all`
/// (used by gc and by re-checkout) can still unlink the files inside.
fn make_files_readonly(dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_dir() {
            make_files_readonly(&entry.path())?;
        } else if ft.is_file() {
            let mut perms = entry.metadata()?.permissions();
            perms.set_mode(perms.mode() & !0o222);
            fs::set_permissions(entry.path(), perms)?;
        }
    }
    Ok(())
}

pub enum RefUpdate<'a> {
    SetTarget { operator: &'a str },
    SetCurrent { operator: &'a str },
    Rollback { operator: &'a str },
    Convert { operator: &'a str },
    ApplyComplete,
    FetchStale,
}

/// Git's well-known empty tree oid.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// The empty tree oid, which `find_tree` resolves to a tree with no entries.
///
/// Used to stand in for the absent side of a diff: looking up units, sysusers,
/// manifests, etc. against it yields nothing, so an added or removed app needs
/// no special-casing.
pub fn empty_tree_oid() -> Oid {
    Oid::from_str(EMPTY_TREE).expect("empty tree oid is a hardcoded valid hex string")
}

/// Get the subtree entries (name -> oid) one level deep.
///
/// Only includes entries that are trees, not blobs. A config tree is
/// structured as `{host}/{app}/{files}` and a deployed commit's tree as
/// `{app}/{files}`, so above the files every entry is a directory.
pub fn tree_entries(tree: &Tree) -> BTreeMap<String, Oid> {
    let mut entries = BTreeMap::new();
    for entry in tree.iter() {
        if entry.kind() != Some(git2::ObjectType::Tree) {
            continue;
        }
        if let Some(name) = entry.name() {
            entries.insert(name.to_string(), entry.id());
        }
    }
    entries
}

/// Build a git tree from a directory.
///
/// `depth` counts directory levels below the config tree root: hosts live
/// at depth 0, apps at depth 1. Only files inside apps get deployed;
/// anything else at those two levels (a stray readme) is excluded, because
/// committing it would change a host's subtree without changing any app,
/// making the host show up in every plan with nothing to do.
fn build_tree_recursive(
    repo: &Repository,
    filter: &HostFilter,
    dir: &Path,
    depth: u32,
) -> Result<Oid> {
    let mut tb = repo.treebuilder(None)?;

    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<std::result::Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let name = entry.file_name();
        let name = name.to_str().ok_or(StoreError::NonUtf8FileName)?;

        let file_type = entry.file_type()?;
        match file_type {
            ft if ft.is_dir() && depth == 0 && !filter.includes(&Hostname::from(name)) => {}
            ft if ft.is_dir() => {
                let oid = build_tree_recursive(repo, filter, &entry.path(), depth + 1)?;
                // Prune subtrees with no files below the host level:
                // committing an empty app tree would make removing the
                // app's files diff as an update to nothing instead of a
                // removal. An empty *host* tree stays: the dir declares
                // the host managed, with zero apps, so deleting a host's
                // last app deploys that removal.
                if depth == 0 || !repo.find_tree(oid)?.is_empty() {
                    tb.insert(name, oid, 0o040000)?;
                }
            }
            // Regular files above the app level (a per-host or per-cluster
            // readme, say) belong to no app, so they are not committed.
            ft if ft.is_file() && depth < 2 => {}
            ft if ft.is_file() => {
                let contents = fs::read(entry.path())?;
                let oid = repo.blob(&contents)?;
                // Git only tracks the user-executable bit.
                let mode = match entry.metadata()?.permissions().mode() & 0o100 {
                    0 => 0o100644,
                    _ => 0o100755,
                };
                tb.insert(name, oid, mode)?;
            }
            // A symlink or special file has no representation in the config
            // tree we build, so reject it with its path rather than skip it:
            // skipping a symlinked app directory would make a deployed app
            // look absent, which the plan then turns into a removal.
            _ => {
                let kind = if file_type.is_symlink() {
                    "a symlink"
                } else {
                    "a special file"
                };
                return Err(StoreError::InvalidConfig(format!(
                    "'{}' is {kind}; a config tree may contain only regular files and directories",
                    entry.path().display(),
                )));
            }
        }
    }

    Ok(tb.write()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::StoreError;
    use crate::testutil::{TestRepo, config_with};
    use std::os::unix::fs::symlink;

    #[test]
    fn commit_tree_creates_commit_with_given_parent() {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/app/config", b"v1")]);
        let c2 = t.commit(&[("web1/app/config", b"v2")]);

        let tree_oid = t.get_commit_tree_oid(c2);
        let sig = t.store.signature().expect("signature is available");
        let c3 = t
            .store
            .commit_tree(&sig, tree_oid, Some(c1), "test commit")
            .expect("commit succeeds");

        let commit = t.store.repo.find_commit(c3).expect("commit exists");
        assert_eq!(commit.parent_count(), 1);
        assert_eq!(commit.parent_id(0).expect("parent 0"), c1);
    }

    #[test]
    fn build_tree_rejects_a_symlinked_app_directory() {
        // A symlinked app directory was silently dropped, so replacing a real
        // app with a symlink would plan as removing the app. (A symlink within
        // an app hits the same arm, but at a depth that used to panic.)
        let t = TestRepo::new();
        let config = config_with(&[("web1/nginx/nginx.conf", b"v1")]);
        symlink("nginx", config.path().join("web1/caddy")).expect("symlink is created");

        let err = t
            .store
            .build_tree(config.path(), &HostFilter::All)
            .unwrap_err();
        let StoreError::InvalidConfig(msg) = err else {
            panic!("expected InvalidConfig, got {err}")
        };
        assert!(msg.contains("caddy"), "{msg}");
    }

    #[test]
    fn validate_rejects_unknown_manifest_key() {
        let t = TestRepo::new();
        let manifest = br#"{"flavor": "spicy"}"#;
        let oid = t.commit(&[("host/nginx/manifest.json", manifest)]);

        let err = t.store.validate(t.get_commit_tree_oid(oid)).unwrap_err();
        assert!(matches!(err, StoreError::Json(_)));
    }

    #[test]
    fn validate_accepts_enabling_unit_not_shipped_by_app() -> Result<()> {
        let t = TestRepo::new();
        // A distro-provided unit can be listed to enable it and to ensure
        // it gets restarted when the app's config changes.
        let manifest = br#"{"systemd": {"units_enabled": ["ntpd.service"]}}"#;
        let oid = t.commit(&[("host/ntp/manifest.json", manifest)]);
        t.store.validate(t.get_commit_tree_oid(oid))
    }

    #[test]
    fn validate_rejects_duplicate_unit_file_across_apps() {
        let t = TestRepo::new();
        let oid = t.commit(&[
            ("host/app1/systemd/shared.service", b"[Unit]"),
            ("host/app2/systemd/shared.service", b"[Unit]"),
        ]);

        let err = t.store.validate(t.get_commit_tree_oid(oid)).unwrap_err();
        let StoreError::InvalidConfig(msg) = err else {
            panic!("expected InvalidConfig, got {err}")
        };
        assert!(msg.contains("shared.service"), "{msg}");
    }

    #[test]
    fn validate_rejects_duplicate_symlink_target_across_apps() {
        let t = TestRepo::new();
        let m1 = br#"{"symlinks": {"/etc/foo.conf": "foo.conf"}}"#;
        let m2 = br#"{"symlinks": {"/etc/foo.conf": "foo.conf"}}"#;
        let oid = t.commit(&[
            ("host/app1/manifest.json", m1),
            ("host/app1/foo.conf", b"v1"),
            ("host/app2/manifest.json", m2),
            ("host/app2/foo.conf", b"v2"),
        ]);

        let err = t.store.validate(t.get_commit_tree_oid(oid)).unwrap_err();
        let StoreError::InvalidConfig(msg) = err else {
            panic!("expected InvalidConfig, got {err}")
        };
        assert!(msg.contains("/etc/foo.conf"), "{msg}");
    }

    #[test]
    fn validate_rejects_duplicate_sysuser_file_across_apps() {
        let t = TestRepo::new();
        let oid = t.commit(&[
            ("host/app1/sysusers/myuser.conf", b"u myuser -"),
            ("host/app2/sysusers/myuser.conf", b"u myuser -"),
        ]);

        let err = t.store.validate(t.get_commit_tree_oid(oid)).unwrap_err();
        let StoreError::InvalidConfig(msg) = err else {
            panic!("expected InvalidConfig, got {err}")
        };
        assert!(msg.contains("myuser.conf"), "{msg}");
    }

    #[test]
    fn validate_rejects_duplicate_networkd_config_across_apps() {
        let t = TestRepo::new();
        let oid = t.commit(&[
            ("host/app1/networkd/10-lan.network", b"[Match]"),
            ("host/app2/networkd/10-lan.network", b"[Match]"),
        ]);

        let err = t.store.validate(t.get_commit_tree_oid(oid)).unwrap_err();
        let StoreError::InvalidConfig(msg) = err else {
            panic!("expected InvalidConfig, got {err}")
        };
        assert!(msg.contains("10-lan.network"), "{msg}");
    }

    #[test]
    fn validate_rejects_duplicate_quadlet_across_apps() {
        let t = TestRepo::new();
        let oid = t.commit(&[
            ("host/app1/quadlets/web.container", b"[Container]"),
            ("host/app2/quadlets/web.container", b"[Container]"),
        ]);

        let err = t.store.validate(t.get_commit_tree_oid(oid)).unwrap_err();
        let StoreError::InvalidConfig(msg) = err else {
            panic!("expected InvalidConfig, got {err}")
        };
        assert!(msg.contains("web.container"), "{msg}");
    }

    #[test]
    fn app_units_includes_dropin_files_by_relative_path() -> Result<()> {
        let t = TestRepo::new();
        let oid = t.commit(&[
            ("postgres/systemd/postgresql.service", b"[Unit]"),
            (
                "postgres/systemd/postgresql.service.d/override.conf",
                b"[Service]",
            ),
        ]);
        let apps = t.store.commit_apps(oid)?;

        let units = t.store.app_units(apps["postgres"])?;

        assert_eq!(
            units,
            BTreeSet::from([
                "postgresql.service".to_string(),
                "postgresql.service.d/override.conf".to_string(),
            ]),
        );
        Ok(())
    }

    #[test]
    fn validate_rejects_same_dropin_file_across_apps() {
        let t = TestRepo::new();
        let oid = t.commit(&[
            ("host/app1/systemd/pg.service.d/override.conf", b"[Service]"),
            ("host/app2/systemd/pg.service.d/override.conf", b"[Service]"),
        ]);

        let err = t.store.validate(t.get_commit_tree_oid(oid)).unwrap_err();
        let StoreError::InvalidConfig(msg) = err else {
            panic!("expected InvalidConfig, got {err}")
        };
        assert!(msg.contains("pg.service.d/override.conf"), "{msg}");
    }

    #[test]
    fn validate_allows_distinct_dropins_in_shared_unit_dir_across_apps() -> Result<()> {
        let t = TestRepo::new();
        // Both apps drop files into the same unit's .d directory, but
        // different files -- they coexist on the host, so this is allowed.
        let oid = t.commit(&[
            ("host/app1/systemd/pg.service.d/a.conf", b"[Service]"),
            ("host/app2/systemd/pg.service.d/b.conf", b"[Service]"),
        ]);

        t.store.validate(t.get_commit_tree_oid(oid))
    }

    #[test]
    fn validate_rejects_symlink_into_networkd_dir() {
        let t = TestRepo::new();
        let manifest =
            br#"{"symlinks": {"/etc/systemd/network/10-lan.network": "10-lan.network"}}"#;
        let oid = t.commit(&[
            ("host/web/manifest.json", manifest),
            ("host/web/10-lan.network", b"[Match]"),
        ]);

        let err = t.store.validate(t.get_commit_tree_oid(oid)).unwrap_err();
        let StoreError::InvalidConfig(msg) = err else {
            panic!("expected InvalidConfig, got {err}")
        };
        assert!(msg.contains("/etc/systemd/network/"), "{msg}");
    }

    #[test]
    fn validate_rejects_symlink_into_quadlet_dir() {
        let t = TestRepo::new();
        let manifest =
            br#"{"symlinks": {"/etc/containers/systemd/web.container": "web.container"}}"#;
        let oid = t.commit(&[
            ("host/web/manifest.json", manifest),
            ("host/web/web.container", b"[Container]"),
        ]);

        let err = t.store.validate(t.get_commit_tree_oid(oid)).unwrap_err();
        let StoreError::InvalidConfig(msg) = err else {
            panic!("expected InvalidConfig, got {err}")
        };
        assert!(msg.contains("/etc/containers/systemd/"), "{msg}");
    }

    #[test]
    fn validate_rejects_relative_symlink_target() {
        let t = TestRepo::new();
        let manifest = br#"{"symlinks": {"etc/nginx.conf": "nginx.conf"}}"#;
        let oid = t.commit(&[
            ("host/nginx/manifest.json", manifest),
            ("host/nginx/nginx.conf", b"server {}"),
        ]);

        let err = t.store.validate(t.get_commit_tree_oid(oid)).unwrap_err();
        let StoreError::InvalidConfig(msg) = err else {
            panic!("expected InvalidConfig, got {err}")
        };
        assert!(msg.contains("not an absolute path"), "{msg}");
    }

    #[test]
    fn validate_rejects_symlink_into_unit_dir() {
        let t = TestRepo::new();
        let manifest = br#"{"symlinks": {"/etc/systemd/system/nginx.service": "nginx.service"}}"#;
        let oid = t.commit(&[
            ("host/nginx/manifest.json", manifest),
            ("host/nginx/nginx.service", b"[Service]"),
        ]);

        let err = t.store.validate(t.get_commit_tree_oid(oid)).unwrap_err();
        let StoreError::InvalidConfig(msg) = err else {
            panic!("expected InvalidConfig, got {err}")
        };
        assert!(msg.contains("/etc/systemd/system/"), "{msg}");
    }

    #[test]
    fn validate_rejects_symlink_into_sysusers_dir() {
        let t = TestRepo::new();
        let manifest = br#"{"symlinks": {"/etc/sysusers.d/myapp.conf": "myapp.conf"}}"#;
        let oid = t.commit(&[
            ("host/myapp/manifest.json", manifest),
            ("host/myapp/myapp.conf", b"u myapp -"),
        ]);

        let err = t.store.validate(t.get_commit_tree_oid(oid)).unwrap_err();
        let StoreError::InvalidConfig(msg) = err else {
            panic!("expected InvalidConfig, got {err}")
        };
        assert!(msg.contains("/etc/sysusers.d/"), "{msg}");
    }

    #[test]
    fn validate_rejects_symlink_with_missing_source() {
        let t = TestRepo::new();
        let manifest = br#"{"symlinks": {"/etc/nginx.conf": "missing.conf"}}"#;
        let oid = t.commit(&[("host/nginx/manifest.json", manifest)]);

        let err = t.store.validate(t.get_commit_tree_oid(oid)).unwrap_err();
        let StoreError::InvalidConfig(msg) = err else {
            panic!("expected InvalidConfig, got {err}")
        };
        assert!(msg.contains("missing.conf"), "{msg}");
    }

    #[test]
    fn validate_accepts_valid_config() -> Result<()> {
        let t = TestRepo::new();
        let manifest =
            br#"{"systemd": {"units_enabled": ["nginx.service"]}, "symlinks": {"/etc/nginx.conf": "nginx.conf"}}"#;
        let oid = t.commit(&[
            ("host/nginx/manifest.json", manifest),
            ("host/nginx/nginx.conf", b"server {}"),
        ]);

        t.store.validate(t.get_commit_tree_oid(oid))
    }

    #[test]
    fn build_tree_preserves_executable_bit() -> Result<()> {
        let dir = crate::testutil::TempDir::new("config");
        let app_dir = dir.path().join("host/app");
        fs::create_dir_all(&app_dir)?;
        fs::write(app_dir.join("hook.sh"), b"#!/bin/sh\n")?;
        fs::write(app_dir.join("plain.conf"), b"key=value\n")?;
        fs::set_permissions(app_dir.join("hook.sh"), fs::Permissions::from_mode(0o755))?;

        let t = TestRepo::new();
        let oid = t.store.build_tree(dir.path(), &HostFilter::All)?;
        let root = t.store.repo.find_tree(oid)?;
        assert_eq!(
            root.get_path(Path::new("host/app/hook.sh"))?.filemode(),
            0o100755,
            "executable input file gets executable git mode",
        );
        assert_eq!(
            root.get_path(Path::new("host/app/plain.conf"))?.filemode(),
            0o100644,
            "non-executable input file gets regular git mode",
        );
        Ok(())
    }

    #[test]
    fn build_tree_prunes_directories_with_no_files() -> Result<()> {
        let dir = crate::testutil::TempDir::new("config");
        let host_dir = dir.path().join("host");

        // nsd/ contains only empty subdirectories -- the entire subtree
        // should be pruned, not just the leaf directories.
        fs::create_dir_all(host_dir.join("nsd/systemd"))?;
        fs::create_dir_all(host_dir.join("nsd/zones"))?;
        fs::create_dir_all(host_dir.join("app"))?;
        fs::write(host_dir.join("app/kept.conf"), b"data")?;

        let t = TestRepo::new();
        let oid = t.store.build_tree(dir.path(), &HostFilter::All)?;
        let tree = t.store.repo.find_tree(oid)?;
        let host_tree = t
            .store
            .repo
            .find_tree(tree.get_name("host").expect("host entry exists").id())?;

        assert!(
            host_tree.get_name("nsd").is_none(),
            "directory with only empty subdirectories should be pruned",
        );
        assert!(
            host_tree.get_name("app").is_some(),
            "non-empty entries should be kept",
        );
        Ok(())
    }

    #[test]
    fn build_tree_keeps_empty_host_dirs() -> Result<()> {
        let dir = crate::testutil::TempDir::new("config");
        fs::create_dir_all(dir.path().join("web1"))?;

        let t = TestRepo::new();
        let oid = t.store.build_tree(dir.path(), &HostFilter::All)?;
        let tree = t.store.repo.find_tree(oid)?;
        let host = tree
            .get_name("web1")
            .expect("an empty host dir stays in the tree");
        assert!(
            t.store.repo.find_tree(host.id())?.is_empty(),
            "the kept host subtree is empty",
        );
        Ok(())
    }

    #[test]
    fn build_tree_excludes_files_outside_app_dirs() -> Result<()> {
        let t = TestRepo::new();
        let clean = config_with(&[("web1/nginx/nginx.conf", b"server {}")]);
        let clean_oid = t.store.build_tree(clean.path(), &HostFilter::All)?;

        let stray = config_with(&[
            ("web1/nginx/nginx.conf", b"server {}"),
            ("readme.md", b"file next to the host dirs"),
            ("web1/foo.txt", b"file next to the app dirs"),
        ]);
        let stray_oid = t.store.build_tree(stray.path(), &HostFilter::All)?;

        assert_eq!(
            stray_oid, clean_oid,
            "files outside app dirs do not affect the tree",
        );
        Ok(())
    }

    #[test]
    fn tree_entries_skips_blobs() {
        let t = TestRepo::new();
        // Create a tree with a blob at the app level alongside a real app.
        let c = t.commit(&[("web1/nginx/conf", b"v1"), ("web1/stray.txt", b"oops")]);
        let tree = t.store.get_commit_tree(c).expect("commit has a tree");
        let web1_tree = t
            .store
            .repo
            .find_tree(tree.get_name("web1").expect("web1 exists").id())
            .expect("web1 is a tree");

        let entries = tree_entries(&web1_tree);
        assert!(
            entries.contains_key("nginx"),
            "subtree entry should be kept"
        );
        assert!(
            !entries.contains_key("stray.txt"),
            "blob entry should be skipped",
        );
    }

    /// The oid of a cluster commit's entry for `host`.
    fn cluster_entry(t: &TestRepo, cluster: Oid, host: &str) -> Oid {
        t.store
            .get_commit_tree(cluster)
            .expect("commit has a tree")
            .get_name(host)
            .expect("cluster tree has an entry for the host")
            .id()
    }

    #[test]
    fn host_tracking_refs_reads_the_host_tree_from_per_host_and_cluster_commits() -> Result<()> {
        let t = TestRepo::new();
        // web1 was deployed by this version: the commit's tree is its own.
        let per_host = t.commit(&[("nginx/conf", b"v1")]);
        t.set_host_tracking_ref("web1", per_host);
        // web2 was deployed by Deptool 1.2: the commit's tree holds the
        // whole cluster, and web2's tree is its entry in it.
        let cluster = t.commit(&[("web1/nginx/conf", b"v1"), ("web2/app/conf", b"v1")]);
        t.set_host_tracking_ref("web2", cluster);

        let refs = t
            .store
            .host_tracking_refs(&["web1".into(), "web2".into()])?;
        assert_eq!(refs[&"web1".into()].commit, per_host);
        assert_eq!(
            refs[&"web1".into()].host_tree,
            t.get_commit_tree_oid(per_host)
        );
        assert_eq!(refs[&"web2".into()].commit, cluster);
        assert_eq!(
            refs[&"web2".into()].host_tree,
            cluster_entry(&t, cluster, "web2")
        );
        Ok(())
    }

    #[test]
    fn convert_cluster_commit_takes_the_host_entry_with_the_cluster_commit_as_parent() -> Result<()>
    {
        let t = TestRepo::new();
        let cluster = t.commit(&[("web1/nginx/conf", b"v1"), ("web2/app/conf", b"v1")]);
        let sig = t.store.signature()?;

        let converted = t
            .store
            .convert_cluster_commit(&sig, &"web1".into(), cluster)?
            .expect("the cluster tree has an entry for web1");

        let commit = t.store.repo.find_commit(converted)?;
        assert_eq!(commit.parent_count(), 1);
        assert_eq!(commit.parent_id(0)?, cluster);
        assert_eq!(commit.tree_id(), cluster_entry(&t, cluster, "web1"));
        assert_eq!(
            commit.summary(),
            Some("Convert web1 history to the per-host format")
        );
        Ok(())
    }

    #[test]
    fn open_or_init_locks_store_dir_to_owner() {
        let parent = crate::testutil::TempDir::new("store_perms");
        let store_path = parent.path().join("store");

        // Init creates the dir; widening between two opens checks
        // that the chmod runs on the reopen path too, not just init.
        let _ = Store::open_or_init(&store_path).expect("init succeeds");
        fs::set_permissions(&store_path, fs::Permissions::from_mode(0o755))
            .expect("widen succeeds");
        let _ = Store::open_or_init(&store_path).expect("reopen succeeds");
        let mode = fs::metadata(&store_path)
            .expect("dir exists")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "dir is owner-only after reopen");
    }
}

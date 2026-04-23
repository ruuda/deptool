//! Git store operations: commit, checkout, and ref management.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use git2::{Commit, Oid, Repository, Tree};

use serde::Deserialize;

use crate::error::StoreError;
use crate::prim::Hostname;

type Result<T> = std::result::Result<T, StoreError>;

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
    /// The commit deployed to this host (`refs/remotes/{host}/current`).
    pub commit: Oid,
    /// The host's own subtree within that commit's tree.
    pub host_tree: Oid,
}

/// A bare Git repository used as the config store.
pub struct Store {
    pub repo: Repository,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Store {
            repo: Repository::open(path)?,
        })
    }

    /// Open an existing bare repo, or create one with reflogs enabled.
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
        Ok(Store { repo })
    }

    /// The on-disk path to the bare repo directory.
    pub fn path(&self) -> &Path {
        self.repo.path()
    }

    /// Read the host-local `refs/heads/current` commit, if it exists.
    pub fn current_commit(&self) -> Option<Oid> {
        self.repo
            .find_reference("refs/heads/current")
            .ok()
            .map(|r| {
                r.peel_to_commit()
                    .expect("current ref points to a commit")
                    .id()
            })
    }

    /// Get the tree for a commit.
    pub fn get_commit_tree(&self, commit_oid: Oid) -> Result<Tree<'_>> {
        Ok(self.repo.find_commit(commit_oid)?.tree()?)
    }

    /// Recursively build a Git tree from a directory on disk.
    pub fn build_tree(&self, dir: &Path) -> Result<Oid> {
        // An entirely empty config directory is valid (no hosts configured),
        // so fall back to writing an empty tree.
        match build_tree_recursive(&self.repo, dir)? {
            Some(oid) => Ok(oid),
            None => Ok(self.repo.treebuilder(None)?.write()?),
        }
    }

    /// Create a commit with the given tree and parent commits.
    ///
    /// Does not update any ref -- the caller decides where to point.
    pub fn commit_tree(&self, tree_oid: Oid, parent_oids: &[Oid]) -> Result<Oid> {
        let tree = self.repo.find_tree(tree_oid)?;
        let author_sig = match self.repo.signature() {
            Ok(sig) => sig,
            Err(..) => git2::Signature::now("deptool", "bot@deptool")?,
        };
        let parents: Vec<Commit> = parent_oids
            .iter()
            .map(|&oid| self.repo.find_commit(oid))
            .collect::<std::result::Result<_, _>>()?;
        let parent_refs: Vec<&Commit> = parents.iter().collect();

        Ok(self.repo.commit(
            None,
            &author_sig,
            &author_sig,
            "Update config",
            &tree,
            &parent_refs,
        )?)
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
    /// For each host that has a `refs/remotes/{host}/current` ref, returns
    /// the deployed commit and the host's own subtree within it.
    pub fn host_tracking_refs(&self, hosts: &[Hostname]) -> Result<BTreeMap<Hostname, HostRef>> {
        let mut result = BTreeMap::new();
        for host in hosts {
            let refname = format!("refs/remotes/{host}/current");
            let commit = match self.repo.find_reference(&refname) {
                Ok(r) => r.peel_to_commit()?,
                Err(_) => continue,
            };
            let tree = commit.tree()?;
            let host_tree = match tree.get_name(&host.0) {
                Some(entry) => entry.id(),
                None => continue,
            };
            result.insert(
                host.clone(),
                HostRef {
                    commit: commit.id(),
                    host_tree,
                },
            );
        }
        Ok(result)
    }

    /// Reduce a set of commits to the frontier -- the minimal subset where
    /// no commit is an ancestor of another.
    pub fn frontier(&self, commits: &[Oid]) -> Result<Vec<Oid>> {
        let unique: BTreeSet<Oid> = commits.iter().copied().collect();
        let mut frontier = Vec::new();
        for &oid in &unique {
            let dominated = unique.iter().any(|&other| {
                other != oid && self.repo.graph_descendant_of(other, oid).unwrap_or(false)
            });
            if !dominated {
                frontier.push(oid);
            }
        }
        // Ensure the output is deterministic regardless of input order.
        frontier.sort();
        Ok(frontier)
    }

    /// Check out a subtree (host/app) from a commit into a target directory.
    pub fn checkout_app(
        &self,
        commit_oid: Oid,
        host: &Hostname,
        app: &str,
        target: &Path,
    ) -> Result<()> {
        let commit = self.repo.find_commit(commit_oid)?;
        let tree = commit.tree()?;
        let entry = tree.get_path(Path::new(&host.0).join(app).as_ref())?;
        let subtree = self.repo.find_tree(entry.id())?;
        let mut cb = git2::build::CheckoutBuilder::new();
        cb.target_dir(target).force();
        self.repo
            .checkout_tree(subtree.as_object(), Some(&mut cb))?;
        Ok(())
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
            RefUpdate::ApplyComplete => "deploy: host applied, update tracking ref".to_string(),
            RefUpdate::FetchStale => "deploy: fetched stale commit from host".to_string(),
            RefUpdate::SetMain => "deploy: lock acquired, deploying this commit".to_string(),
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

    /// List filenames in an app tree's named subdirectory.
    ///
    /// Returns an empty set if the subdirectory doesn't exist.
    fn subdir_entries(&self, app_tree_oid: Oid, subdir: &str) -> Result<BTreeSet<String>> {
        let tree = self.repo.find_tree(app_tree_oid)?;
        let entry = match tree.get_name(subdir) {
            Some(entry) => entry,
            None => return Ok(BTreeSet::new()),
        };
        let subtree = self.repo.find_tree(entry.id())?;
        let mut names = BTreeSet::new();
        for entry in subtree.iter() {
            if let Some(name) = entry.name() {
                names.insert(name.to_string());
            }
        }
        Ok(names)
    }

    /// Collect desired symlinks for an app subdirectory across all apps.
    ///
    /// Maps each filename in `subdir` to the absolute symlink target path
    /// under `apps_dir/<app>/current/<subdir>/`.
    fn desired_subdir_symlinks(
        &self,
        commit_oid: Oid,
        host: &Hostname,
        apps_dir: &Path,
        subdir: &str,
    ) -> Result<BTreeMap<String, PathBuf>> {
        let tree = self.get_commit_tree(commit_oid)?;
        let apps = self.get_host_apps(&tree, host)?;
        let mut result = BTreeMap::new();
        for (app, app_tree_oid) in &apps {
            for name in self.subdir_entries(*app_tree_oid, subdir)? {
                let target = apps_dir.join(app).join("current").join(subdir).join(&name);
                result.insert(name, target);
            }
        }
        Ok(result)
    }

    /// Collect desired unit symlinks for a host.
    pub fn desired_units(
        &self,
        commit_oid: Oid,
        host: &Hostname,
        apps_dir: &Path,
    ) -> Result<crate::plan::DesiredUnits> {
        self.desired_subdir_symlinks(commit_oid, host, apps_dir, "systemd")
    }

    /// List all unit files in an app tree's `systemd/` directory.
    pub fn app_units(&self, app_tree_oid: Oid) -> Result<BTreeSet<String>> {
        self.subdir_entries(app_tree_oid, "systemd")
    }

    /// List all sysuser config files in an app tree's `sysusers/` directory.
    pub fn app_sysusers(&self, app_tree_oid: Oid) -> Result<BTreeSet<String>> {
        self.subdir_entries(app_tree_oid, "sysusers")
    }

    /// Get the tree oid of the `sysusers/` subtree, for content comparison.
    pub fn sysusers_tree_oid(&self, app_tree_oid: Oid) -> Result<Option<Oid>> {
        let tree = self.repo.find_tree(app_tree_oid)?;
        Ok(tree.get_name("sysusers").map(|e| e.id()))
    }

    /// Collect desired sysusers symlinks for a host.
    pub fn desired_sysusers(
        &self,
        commit_oid: Oid,
        host: &Hostname,
        apps_dir: &Path,
    ) -> Result<crate::plan::DesiredSysusers> {
        self.desired_subdir_symlinks(commit_oid, host, apps_dir, "sysusers")
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
        host: &Hostname,
        apps_dir: &Path,
    ) -> Result<BTreeMap<PathBuf, PathBuf>> {
        let tree = self.get_commit_tree(commit_oid)?;
        let apps = self.get_host_apps(&tree, host)?;
        let mut result = BTreeMap::new();
        for (app, app_tree_oid) in &apps {
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
        for entry in tree.iter() {
            let host = Hostname(entry.name().expect("tree entry name is utf-8").to_string());
            self.validate_app_manifests(&tree, &host)?;
        }
        Ok(())
    }

    /// Validate manifests for all apps on a single host.
    fn validate_app_manifests(&self, config_tree: &Tree, host: &Hostname) -> Result<()> {
        let apps = self.get_host_apps(config_tree, host)?;

        // Track unit files, sysuser configs, and symlink targets across apps
        // to detect conflicts.
        let mut unit_owners: BTreeMap<String, String> = BTreeMap::new();
        let mut sysuser_owners: BTreeMap<String, String> = BTreeMap::new();
        let mut symlink_owners: BTreeMap<String, String> = BTreeMap::new();

        for (app, app_tree_oid) in &apps {
            let manifest = self.read_manifest(*app_tree_oid)?;
            let app_tree = self.repo.find_tree(*app_tree_oid)?;

            // Check for duplicate unit files across apps.
            for name in self.app_units(*app_tree_oid)? {
                if let Some(other) = unit_owners.insert(name.clone(), app.clone()) {
                    return Err(StoreError::InvalidConfig(format!(
                        "unit {name} provided by both {other} and {app}",
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

            for (target, source) in &manifest.symlinks {
                if !target.starts_with('/') {
                    return Err(StoreError::InvalidConfig(format!(
                        "app {app} symlink target {target} is not an absolute path",
                    )));
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

    /// Get the app tree oids for a host from a config tree.
    pub fn get_host_apps(
        &self,
        config_tree: &Tree,
        host: &Hostname,
    ) -> Result<BTreeMap<String, Oid>> {
        match config_tree.get_name(&host.0) {
            Some(e) => Ok(tree_entries(&self.repo.find_tree(e.id())?)),
            None => Ok(BTreeMap::new()),
        }
    }
}

pub enum RefUpdate<'a> {
    SetTarget { operator: &'a str },
    SetCurrent { operator: &'a str },
    Rollback { operator: &'a str },
    ApplyComplete,
    FetchStale,
    SetMain,
}

/// Get the subtree entries (name -> oid) one level deep.
///
/// Only includes entries that are trees, not blobs. The config tree is
/// structured as `{host}/{app}/{files}`, so the first two levels are
/// always directories.
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

/// Build a git tree from a directory. Returns `None` when the subtree
/// contains no files (only empty directories), because empty trees have
/// no meaning in git and would suppress Remove diffs in deptool.
fn build_tree_recursive(repo: &Repository, dir: &Path) -> Result<Option<Oid>> {
    let mut tb = repo.treebuilder(None)?;

    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<std::result::Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let name = entry.file_name();
        let name = name.to_str().ok_or(StoreError::NonUtf8FileName)?;

        match entry.file_type()? {
            ft if ft.is_dir() => {
                if let Some(oid) = build_tree_recursive(repo, &entry.path())? {
                    tb.insert(name, oid, 0o040000)?;
                }
            }
            ft if ft.is_file() => {
                let contents = fs::read(entry.path())?;
                let oid = repo.blob(&contents)?;
                tb.insert(name, oid, 0o100644)?;
            }
            _ => panic!("Unsupported directory entry: {name}"),
        }
    }

    let oid = tb.write()?;
    let tree = repo.find_tree(oid)?;
    if tree.is_empty() {
        Ok(None)
    } else {
        Ok(Some(oid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::StoreError;
    use crate::testutil::TestRepo;

    #[test]
    fn commit_appends_to_main_branch() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/app/config", b"v1")]);
        let c2 = t.commit(&[("web1/app/config", b"v2")]);

        let commit = t.store.repo.find_commit(c2)?;
        assert_eq!(commit.parent_count(), 1);
        assert_eq!(commit.parent_id(0)?, c1);
        Ok(())
    }

    #[test]
    fn commit_tree_creates_commit_with_given_parents() {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/app/config", b"v1")]);
        let c2 = t.commit(&[("web1/app/config", b"v2")]);

        let tree_oid = t.get_commit_tree_oid(c2);
        let c3 = t
            .store
            .commit_tree(tree_oid, &[c1, c2])
            .expect("commit succeeds");

        let commit = t.store.repo.find_commit(c3).expect("commit exists");
        assert_eq!(commit.parent_count(), 2);
        assert_eq!(commit.parent_id(0).expect("parent 0"), c1);
        assert_eq!(commit.parent_id(1).expect("parent 1"), c2);
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
    fn build_tree_prunes_directories_with_no_files() -> Result<()> {
        let dir = crate::testutil::TempDir::new("config");
        let host_dir = dir.path().join("host");

        // nsd/ contains only empty subdirectories -- the entire subtree
        // should be pruned, not just the leaf directories.
        fs::create_dir_all(host_dir.join("nsd/systemd"))?;
        fs::create_dir_all(host_dir.join("nsd/zones"))?;
        fs::write(host_dir.join("kept.conf"), b"data")?;

        let t = TestRepo::new();
        let oid = t.store.build_tree(dir.path())?;
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
            host_tree.get_name("kept.conf").is_some(),
            "non-empty entries should be kept",
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

    #[test]
    fn frontier_keeps_only_tips() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("h/app/f", b"v1")]);
        let c2 = t.commit(&[("h/app/f", b"v2")]);
        let c3 = t.commit(&[("h/app/f", b"v3")]);

        // Linear chain: only the tip survives.
        assert_eq!(t.store.frontier(&[c1, c2, c3])?, vec![c3]);
        assert_eq!(t.store.frontier(&[c3, c1])?, vec![c3]);

        // Single commit.
        assert_eq!(t.store.frontier(&[c2])?, vec![c2]);

        // Empty input.
        assert_eq!(t.store.frontier(&[])?, vec![]);

        // Duplicates are deduplicated.
        assert_eq!(t.store.frontier(&[c3, c3])?, vec![c3]);

        Ok(())
    }

    #[test]
    fn frontier_keeps_diverged_branches() -> Result<()> {
        let t = TestRepo::new();
        let base = t.commit(&[("h/app/f", b"base")]);
        let c_a = t.commit(&[("h/app/f", b"branch-a")]);

        // Branch from base so c_a and c_b are diverged.
        t.store
            .set_ref("refs/heads/main", base, RefUpdate::FetchStale)?;
        let c_b = t.commit(&[("h/app/f", b"branch-b")]);

        let result = t.store.frontier(&[c_a, c_b])?;
        let mut expected = vec![c_a, c_b];
        expected.sort();
        assert_eq!(result, expected);

        // Base is dominated by both, should be removed.
        assert_eq!(t.store.frontier(&[base, c_a, c_b])?, expected);

        Ok(())
    }

    #[test]
    fn host_tracking_refs_collects_per_host_state() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/conf", b"v1"), ("web2/app/conf", b"v1")]);
        t.set_host_tracking_ref("web1", c1);
        t.set_host_tracking_ref("web2", c1);

        let hosts = vec!["web1".into(), "web2".into()];
        let refs = t.store.host_tracking_refs(&hosts)?;
        assert_eq!(refs.len(), 2);

        // Both hosts point to the same commit.
        assert_eq!(refs[&"web1".into()].commit, c1);
        assert_eq!(refs[&"web2".into()].commit, c1);

        // But their host subtree OIDs differ (different app content).
        assert_ne!(
            refs[&"web1".into()].host_tree,
            refs[&"web2".into()].host_tree,
        );

        Ok(())
    }
}

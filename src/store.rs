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

    /// Get the tree for a commit.
    pub fn get_commit_tree(&self, commit_oid: Oid) -> Result<Tree<'_>> {
        Ok(self.repo.find_commit(commit_oid)?.tree()?)
    }

    /// Recursively build a Git tree from a directory on disk.
    pub fn build_tree(&self, dir: &Path) -> Result<Oid> {
        build_tree_recursive(&self.repo, dir)
    }

    /// Commit a tree to `refs/heads/main`.
    ///
    /// Returns `None` if the tree is identical to the current head.
    pub fn commit_tree(&self, tree_oid: Oid) -> Result<Option<Oid>> {
        let tree = self.repo.find_tree(tree_oid)?;

        // Use the ambient Git author metadata if configured, fall back to
        // hard-coded credentials otherwise. This is mostly relevant in tests when
        // they run in e.g. an isolated Nix build environment.
        let author_sig = match self.repo.signature() {
            Ok(sig) => sig,
            Err(..) => git2::Signature::now("deptool", "bot@deptool")?,
        };

        let parent = self
            .repo
            .find_reference("refs/heads/main")
            .ok()
            .map(|r| r.peel_to_commit())
            .transpose()?;

        if parent.as_ref().is_some_and(|c| c.tree_id() == tree_oid) {
            return Ok(None);
        }

        let parents: Vec<&Commit> = parent.iter().collect();

        Ok(Some(self.repo.commit(
            Some("refs/heads/main"),
            &author_sig,
            &author_sig,
            "Update config",
            &tree,
            &parents,
        )?))
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

    /// Collect desired unit symlinks for a host.
    ///
    /// Maps each unit filename to the absolute symlink target path under
    /// `apps_dir/<app>/current/systemd/`.
    pub fn desired_units(
        &self,
        commit_oid: Oid,
        host: &Hostname,
        apps_dir: &Path,
    ) -> Result<crate::apply::DesiredUnits> {
        let tree = self.get_commit_tree(commit_oid)?;
        let apps = self.get_host_apps(&tree, host)?;
        let mut units = BTreeMap::new();
        for (app, app_tree_oid) in &apps {
            for name in self.app_units(*app_tree_oid)? {
                let target = apps_dir
                    .join(app)
                    .join("current")
                    .join("systemd")
                    .join(&name);
                units.insert(name, target);
            }
        }
        Ok(units)
    }

    /// List all unit files in an app tree's `systemd/` directory.
    ///
    /// Returns an empty set if the app has no `systemd/` subtree.
    pub fn app_units(&self, app_tree_oid: Oid) -> Result<BTreeSet<String>> {
        let tree = self.repo.find_tree(app_tree_oid)?;
        let systemd_entry = match tree.get_name("systemd") {
            Some(entry) => entry,
            None => return Ok(BTreeSet::new()),
        };
        let systemd_tree = self.repo.find_tree(systemd_entry.id())?;
        let mut units = BTreeSet::new();
        for entry in systemd_tree.iter() {
            if let Some(name) = entry.name() {
                units.insert(name.to_string());
            }
        }
        Ok(units)
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

        // Track unit files and symlink targets across apps to detect conflicts.
        let mut unit_owners: BTreeMap<String, String> = BTreeMap::new();
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
}

/// Get the tree entries (name -> oid) one level deep.
pub fn tree_entries(tree: &Tree) -> BTreeMap<String, Oid> {
    let mut entries = BTreeMap::new();
    for entry in tree.iter() {
        if let Some(name) = entry.name() {
            entries.insert(name.to_string(), entry.id());
        }
    }
    entries
}

fn build_tree_recursive(repo: &Repository, dir: &Path) -> Result<Oid> {
    let mut tb = repo.treebuilder(None)?;

    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<std::result::Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let name = entry.file_name();
        let name = name.to_str().ok_or(StoreError::NonUtf8FileName)?;

        match entry.file_type()? {
            ft if ft.is_dir() => {
                let oid = build_tree_recursive(repo, &entry.path())?;
                tb.insert(name, oid, 0o040000)?;
            }
            ft if ft.is_file() => {
                let contents = fs::read(entry.path())?;
                let oid = repo.blob(&contents)?;
                tb.insert(name, oid, 0o100644)?;
            }
            _ => panic!("Unsupported directory entry: {name}"),
        }
    }

    Ok(tb.write()?)
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
    fn commit_tree_returns_none_when_unchanged() {
        let t = TestRepo::new();
        let oid = t.commit(&[("web1/app/config", b"v1")]);
        let tree_oid = t.get_commit_tree_oid(oid);

        assert!(t.store.commit_tree(tree_oid).unwrap().is_none());
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
}

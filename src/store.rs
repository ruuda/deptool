//! Git store operations: commit, checkout, and ref management.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use git2::{Commit, Oid, Repository, Tree};

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::prim::Hostname;

/// Per-app manifest declaring runtime state that deptool should manage.
#[derive(Default, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub systemd: SystemdConfig,
}

#[derive(Default, Deserialize)]
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

    pub fn commit_tree(&self, tree_oid: Oid) -> Result<Oid> {
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
        let parents: Vec<&Commit> = parent.iter().collect();

        Ok(self.repo.commit(
            Some("refs/heads/main"),
            &author_sig,
            &author_sig,
            "Update config",
            &tree,
            &parents,
        )?)
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
    pub fn get_lock_file_path(&self) -> std::path::PathBuf {
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
        apps_dir: &std::path::Path,
    ) -> Result<crate::apply::DesiredUnits> {
        let tree = self.get_commit_tree(commit_oid)?;
        let apps = self.get_host_apps(&tree, host)?;
        let mut units = std::collections::BTreeMap::new();
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
    pub fn app_units(&self, app_tree_oid: Oid) -> Result<std::collections::BTreeSet<String>> {
        let tree = self.repo.find_tree(app_tree_oid)?;
        let systemd_entry = match tree.get_name("systemd") {
            Some(entry) => entry,
            None => return Ok(std::collections::BTreeSet::new()),
        };
        let systemd_tree = self.repo.find_tree(systemd_entry.id())?;
        let mut units = std::collections::BTreeSet::new();
        for entry in systemd_tree.iter() {
            if let Some(name) = entry.name() {
                units.insert(name.to_string());
            }
        }
        Ok(units)
    }

    /// Read the enabled units from an app's manifest as a set.
    pub fn enabled_units(&self, app_tree_oid: Oid) -> Result<std::collections::BTreeSet<String>> {
        let manifest = self.read_manifest(app_tree_oid)?;
        Ok(manifest.systemd.units_enabled.into_iter().collect())
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
        let name = name.to_str().ok_or(Error::NonUtf8FileName)?;

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
    use crate::error::Result;
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
}

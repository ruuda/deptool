//! Git store operations: commit, checkout, and apply configs.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use git2::Repository;

use crate::error::{Error, Result};
use crate::plan::AppDiff;

/// Recursively build a Git tree from a directory on disk.
pub fn build_tree(repo: &Repository, dir: &Path) -> Result<git2::Oid> {
    let mut tb = repo.treebuilder(None)?;

    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<std::result::Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let name = entry.file_name();
        let name = name.to_str().ok_or(Error::NonUtf8FileName)?;

        match entry.file_type()? {
            ft if ft.is_dir() => {
                let oid = build_tree(repo, &entry.path())?;
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

pub fn commit_tree(repo: &Repository, tree_oid: git2::Oid) -> Result<git2::Oid> {
    let tree = repo.find_tree(tree_oid)?;
    let sig = repo.signature()?;

    let parent = repo
        .find_reference("refs/heads/main")
        .ok()
        .map(|r| r.peel_to_commit())
        .transpose()?;
    let parents: Vec<&git2::Commit> = parent.iter().collect();

    Ok(repo.commit(
        Some("refs/heads/main"),
        &sig,
        &sig,
        "Update config",
        &tree,
        &parents,
    )?)
}

/// Check out a subtree (host/app) from a commit into a target directory.
pub fn checkout_app(
    repo: &Repository,
    commit_oid: git2::Oid,
    host: &str,
    app: &str,
    target: &Path,
) -> Result<()> {
    let commit = repo.find_commit(commit_oid)?;
    let tree = commit.tree()?;
    let entry = tree.get_path(Path::new(host).join(app).as_ref())?;
    let subtree = repo.find_tree(entry.id())?;
    let mut cb = git2::build::CheckoutBuilder::new();
    cb.target_dir(target).force();
    repo.checkout_tree(subtree.as_object(), Some(&mut cb))?;
    Ok(())
}

pub enum RefUpdate {
    SetTarget,
    SetCurrent,
}

pub fn set_ref(repo: &Repository, refname: &str, oid: git2::Oid, reason: RefUpdate) -> Result<()> {
    let reflog_msg = match reason {
        RefUpdate::SetTarget => "apply: begin deployment, set target",
        RefUpdate::SetCurrent => "apply: conclude deployment, set current",
    };
    let force = true;
    repo.reference(refname, oid, force, reflog_msg)?;
    Ok(())
}

/// Get the tree entries (name -> oid) one level deep.
pub fn tree_entries(tree: &git2::Tree) -> BTreeMap<String, git2::Oid> {
    let mut entries = BTreeMap::new();
    for entry in tree.iter() {
        if let Some(name) = entry.name() {
            entries.insert(name.to_string(), entry.id());
        }
    }
    entries
}

/// Get the app tree oids for a host from a config tree.
pub fn get_host_apps(
    repo: &Repository,
    config_tree: &git2::Tree,
    host: &str,
) -> Result<BTreeMap<String, git2::Oid>> {
    match config_tree.get_name(host) {
        Some(e) => Ok(tree_entries(&repo.find_tree(e.id())?)),
        None => Ok(BTreeMap::new()),
    }
}

/// Apply a deployment: set target ref, check out changed apps, set current ref.
///
/// This runs on the target host. It compares `refs/heads/current` against
/// `expected_current` (None for first deploy) to ensure the plan is not stale,
/// then diffs current against the target commit and applies the changes.
pub fn apply(
    repo: &Repository,
    commit_oid: git2::Oid,
    expected_current: Option<git2::Oid>,
    host: &str,
    target_dir: &Path,
) -> Result<()> {
    let actual_current = repo
        .find_reference("refs/heads/current")
        .ok()
        .map(|r| r.peel_to_commit().map(|c| c.id()))
        .transpose()?;

    assert_eq!(
        actual_current, expected_current,
        "Stale plan: expected current ref {expected_current:?} but found {actual_current:?}",
    );

    set_ref(repo, "refs/heads/target", commit_oid, RefUpdate::SetTarget)?;

    let target_tree = repo.find_commit(commit_oid)?.tree()?;
    let target_apps = get_host_apps(repo, &target_tree, host)?;

    let current_apps = match actual_current {
        None => BTreeMap::new(),
        Some(oid) => {
            let tree = repo.find_commit(oid)?.tree()?;
            get_host_apps(repo, &tree, host)?
        }
    };

    let diff = crate::plan::diff_apps(&current_apps, &target_apps);

    for (app, change) in &diff {
        match change {
            AppDiff::Add { .. } | AppDiff::Update { .. } => {
                checkout_app(repo, commit_oid, host, app, target_dir)?;
            }
            AppDiff::Remove => {
                // TODO: Remove the app directory.
            }
        }
    }

    set_ref(
        repo,
        "refs/heads/current",
        commit_oid,
        RefUpdate::SetCurrent,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use git2::Repository;

    use super::*;
    use crate::testutil::{TempDir, commit_files};

    fn read_dir_recursive(root: &Path, dir: &Path) -> Result<Vec<(String, Vec<u8>)>> {
        let mut result = Vec::new();
        let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<std::result::Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let ft = entry.file_type()?;
            if ft.is_dir() {
                result.extend(read_dir_recursive(root, &entry.path())?);
            } else if ft.is_file() {
                let rel = entry
                    .path()
                    .strip_prefix(root)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string();
                let contents = fs::read(entry.path())?;
                result.push((rel, contents));
            }
        }
        Ok(result)
    }

    #[test]
    fn checkout_after_commit_reproduces_the_app_files() -> Result<()> {
        let input = TempDir::new("input");
        fs::create_dir_all(input.path().join("web1/nginx/etc/nginx"))?;
        fs::write(
            input.path().join("web1/nginx/etc/nginx/nginx.conf"),
            "server {}",
        )?;
        fs::create_dir_all(input.path().join("web1/rofld/etc/rofld"))?;
        fs::write(
            input.path().join("web1/rofld/etc/rofld/config.toml"),
            "[server]\nport = 8080\n",
        )?;

        let store = TempDir::new("store");
        let repo = Repository::init_bare(store.path())?;

        let tree_oid = build_tree(&repo, input.path())?;
        let commit_oid = commit_tree(&repo, tree_oid)?;

        let output = TempDir::new("output");
        checkout_app(&repo, commit_oid, "web1", "nginx", output.path())?;

        let out = output.path();
        let expected = input.path().join("web1/nginx");
        assert_eq!(
            read_dir_recursive(out, out)?,
            read_dir_recursive(&expected, &expected)?,
        );
        Ok(())
    }

    #[test]
    fn commit_appends_to_main_branch() -> Result<()> {
        let store = TempDir::new("store");
        let repo = Repository::init_bare(store.path())?;

        let c1 = commit_files(&repo, &[("web1/app/config", b"v1")])?;
        let c2 = commit_files(&repo, &[("web1/app/config", b"v2")])?;

        let commit = repo.find_commit(c2)?;
        assert_eq!(commit.parent_count(), 1);
        assert_eq!(commit.parent_id(0)?, c1);
        Ok(())
    }

    #[test]
    fn apply_sets_target_and_current_refs() -> Result<()> {
        let store = TempDir::new("store");
        let repo = Repository::init_bare(store.path())?;
        let c1 = commit_files(&repo, &[("web1/nginx/conf", b"v1")])?;

        let output = TempDir::new("output");
        apply(&repo, c1, None, "web1", output.path())?;

        let current = repo
            .find_reference("refs/heads/current")?
            .peel_to_commit()?
            .id();
        let target = repo
            .find_reference("refs/heads/target")?
            .peel_to_commit()?
            .id();
        assert_eq!(current, c1);
        assert_eq!(target, c1);
        Ok(())
    }

    #[test]
    #[should_panic(expected = "Stale plan")]
    fn apply_panics_on_stale_expected_current() {
        let store = TempDir::new("store");
        let repo = Repository::init_bare(store.path()).unwrap();
        let c1 = commit_files(&repo, &[("web1/nginx/conf", b"v1")]).unwrap();

        let output = TempDir::new("output");
        apply(&repo, c1, None, "web1", output.path()).unwrap();

        let c2 = commit_files(&repo, &[("web1/nginx/conf", b"v2")]).unwrap();

        let stale_oid = git2::Oid::from_str("0000000000000000000000000000000000000000").unwrap();
        apply(&repo, c2, Some(stale_oid), "web1", output.path()).unwrap();
    }
}

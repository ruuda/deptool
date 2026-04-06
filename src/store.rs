//! Git store operations: commit, checkout, and ref management.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use git2::Repository;

use crate::error::{Error, Result};
use crate::prim::Hostname;

/// Open an existing bare repo, or create one with reflogs enabled.
pub fn open_or_init(path: &Path) -> Result<Repository> {
    match Repository::open(path) {
        Ok(r) => Ok(r),
        Err(_) => {
            let repo = Repository::init_bare(path)?;
            // Bare repos don't create reflogs by default.
            repo.config()?.set_bool("core.logAllRefUpdates", true)?;
            Ok(repo)
        }
    }
}

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

    // Use the ambient Git author metadata if configured, fall back to
    // hard-coded credentials otherwise. This is mostly relevant in tests when
    // they run in e.g. an isolated Nix build environment.
    let author_sig = match repo.signature() {
        Ok(sig) => sig,
        Err(..) => git2::Signature::now("deptool", "bot@deptool")?,
    };

    let parent = repo
        .find_reference("refs/heads/main")
        .ok()
        .map(|r| r.peel_to_commit())
        .transpose()?;
    let parents: Vec<&git2::Commit> = parent.iter().collect();

    Ok(repo.commit(
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
    repo: &Repository,
    commit_oid: git2::Oid,
    host: &Hostname,
    app: &str,
    target: &Path,
) -> Result<()> {
    let commit = repo.find_commit(commit_oid)?;
    let tree = commit.tree()?;
    let entry = tree.get_path(Path::new(&host.0).join(app).as_ref())?;
    let subtree = repo.find_tree(entry.id())?;
    let mut cb = git2::build::CheckoutBuilder::new();
    cb.target_dir(target).force();
    repo.checkout_tree(subtree.as_object(), Some(&mut cb))?;
    Ok(())
}

pub enum RefUpdate {
    SetTarget,
    SetCurrent,
    ApplyComplete,
    FetchStale,
}

pub fn set_ref(repo: &Repository, refname: &str, oid: git2::Oid, reason: RefUpdate) -> Result<()> {
    let reflog_msg = match reason {
        RefUpdate::SetTarget => "apply: begin deployment, set target",
        RefUpdate::SetCurrent => "apply: conclude deployment, set current",
        RefUpdate::ApplyComplete => "deploy: host applied, update tracking ref",
        RefUpdate::FetchStale => "deploy: fetched stale commit from host",
    };
    let force = true;
    repo.reference(refname, oid, force, reflog_msg)?;
    Ok(())
}

/// Build a packfile of objects reachable from a commit.
///
/// If `have_commit` is provided, objects reachable from it are excluded,
/// so only the delta between the two commits is packed.
pub fn create_pack(
    repo: &Repository,
    want_commit: git2::Oid,
    have_commit: Option<git2::Oid>,
) -> Result<Vec<u8>> {
    let mut walk = repo.revwalk()?;
    walk.push(want_commit)?;
    if let Some(have) = have_commit {
        walk.hide(have)?;
    }
    let mut builder = repo.packbuilder()?;
    builder.insert_walk(&mut walk)?;
    let mut buf = git2::Buf::new();
    builder.write_buf(&mut buf)?;
    Ok(buf.to_vec())
}

/// Write raw packfile bytes into a repository's object database.
pub fn write_pack(repo: &Repository, data: &[u8]) -> Result<()> {
    let odb = repo.odb()?;
    let mut writer = odb.packwriter()?;
    std::io::Write::write_all(&mut writer, data)?;
    writer.commit()?;
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
    host: &Hostname,
) -> Result<BTreeMap<String, git2::Oid>> {
    match config_tree.get_name(&host.0) {
        Some(e) => Ok(tree_entries(&repo.find_tree(e.id())?)),
        None => Ok(BTreeMap::new()),
    }
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

        let commit = t.repo.find_commit(c2)?;
        assert_eq!(commit.parent_count(), 1);
        assert_eq!(commit.parent_id(0)?, c1);
        Ok(())
    }
}

//! Shared test helpers: temp directories and convenience functions.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use git2::Repository;

use crate::error::Result;
use crate::store::commit_tree;

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct TempDir(std::path::PathBuf);

impl TempDir {
    pub fn new(label: &str) -> Self {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("deptool-test-{pid}-{id}-{label}"));
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Build a tree from in-memory file data, supporting nested paths like "a/b/c".
fn build_tree_from_files(repo: &Repository, files: &[(&str, &[u8])]) -> Result<git2::Oid> {
    let mut subdirs: BTreeMap<&str, Vec<(&str, &[u8])>> = BTreeMap::new();
    let mut builder = repo.treebuilder(None)?;

    for &(path, content) in files {
        match path.split_once('/') {
            Some((dir, rest)) => subdirs.entry(dir).or_default().push((rest, content)),
            None => {
                let blob = repo.blob(content)?;
                builder.insert(path, blob, 0o100644)?;
            }
        }
    }

    for (dir, sub_files) in &subdirs {
        let subtree = build_tree_from_files(repo, sub_files)?;
        builder.insert(dir, subtree, 0o040000)?;
    }

    Ok(builder.write()?)
}

/// Create a commit with the given files, without touching the filesystem.
pub fn commit_files(repo: &Repository, files: &[(&str, &[u8])]) -> Result<git2::Oid> {
    let tree_oid = build_tree_from_files(repo, files)?;
    commit_tree(repo, tree_oid)
}

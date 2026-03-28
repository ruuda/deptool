//! Shared test helpers: temp directories and convenience functions.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use git2::Repository;

use crate::error::Result;
use crate::store::{build_tree, commit_tree};

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

pub fn commit_dir(repo: &Repository, dir: &Path) -> Result<git2::Oid> {
    let tree_oid = build_tree(repo, dir)?;
    commit_tree(repo, tree_oid)
}

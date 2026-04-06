//! Shared test helpers: temp directories and convenience functions.

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use git2::Repository;

use crate::deploy::Connection;
use crate::error::Result;
use crate::protocol::{self, Hello, Message, Request};
use crate::store::{RefUpdate, Store};

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
fn commit_files(store: &Store, files: &[(&str, &[u8])]) -> Result<git2::Oid> {
    let tree_oid = build_tree_from_files(&store.repo, files)?;
    store.commit_tree(tree_oid)
}

/// A bare Git repository backed by a temporary directory.
pub struct TestRepo {
    pub store: Store,
    _dir: TempDir,
}

impl TestRepo {
    pub fn new() -> Self {
        let dir = TempDir::new("store");
        let repo = Repository::init_bare(dir.path()).expect("repo is created");
        TestRepo {
            store: Store { repo },
            _dir: dir,
        }
    }

    /// Create a byte-for-byte copy of another TestRepo's git store.
    pub fn copy_from(other: &TestRepo) -> Self {
        let dir = TempDir::new("store");
        fs::remove_dir_all(dir.path()).expect("temp dir is removed");
        std::process::Command::new("cp")
            .args(["-r"])
            .arg(other.store.repo.path())
            .arg(dir.path())
            .status()
            .expect("cp succeeds");
        let repo = Repository::open(dir.path()).expect("repo is opened");
        TestRepo {
            store: Store { repo },
            _dir: dir,
        }
    }

    /// Create a commit with the given files, without touching the filesystem.
    pub fn commit(&self, files: &[(&str, &[u8])]) -> git2::Oid {
        commit_files(&self.store, files).expect("commit succeeds")
    }

    /// Read the driver-side tracking ref for a host (`refs/remotes/{host}/current`).
    pub fn get_host_tracking_ref(&self, host: &str) -> Option<git2::Oid> {
        self.store
            .repo
            .find_reference(&format!("refs/remotes/{host}/current"))
            .ok()
            .map(|r| {
                r.peel_to_commit()
                    .expect("tracking ref points to a commit")
                    .id()
            })
    }

    /// Set the driver-side tracking ref for a host (`refs/remotes/{host}/current`).
    pub fn set_host_tracking_ref(&self, host: &str, commit_oid: git2::Oid) {
        self.store
            .set_ref(
                &format!("refs/remotes/{host}/current"),
                commit_oid,
                RefUpdate::SetCurrent,
            )
            .expect("ref is set");
    }
}

/// A host-side test environment: a bare repo with apps and units directories.
///
/// Wraps a `HostSession` and provides helpers for sending requests and
/// collecting responses.
pub struct TestHost {
    pub session: crate::session::HostSession,
    _store: TempDir,
    apps: TempDir,
    units: TempDir,
}

impl TestHost {
    /// Create a new host with a fresh bare repo.
    pub fn new(hostname: &str) -> Self {
        let store = TempDir::new("store");
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        let repo = Repository::init_bare(store.path()).expect("repo is created");
        let session =
            crate::session::HostSession::new_test(repo, hostname, apps.path(), units.path());
        TestHost {
            session,
            _store: store,
            apps,
            units,
        }
    }

    pub fn with_commit(hostname: &str, files: &[(&str, &[u8])]) -> (Self, git2::Oid) {
        let host = Self::new(hostname);
        // Open the same repo as a Store to create the commit.
        let store = Store::open(host.session.store.repo.path()).expect("repo is opened");
        let oid = commit_files(&store, files).expect("commit succeeds");
        (host, oid)
    }

    /// The host-local `refs/heads/current` commit, if any.
    pub fn get_current(&self) -> Option<git2::Oid> {
        self.session
            .store
            .repo
            .find_reference("refs/heads/current")
            .ok()
            .map(|r| {
                r.peel_to_commit()
                    .expect("current ref points to a commit")
                    .id()
            })
    }

    /// Send a request and collect all response messages.
    pub fn interact(&mut self, request: Request) -> Vec<Message> {
        let mut responses = Vec::new();
        self.session
            .handle_request(request, &mut |r| responses.push(r));
        responses
    }

    /// Create a fresh in-memory connection to this host.
    ///
    /// Each call opens the same underlying repo with a new session, like a
    /// new SSH connection to the same host. The apps and units directories
    /// are shared across connections, as they would be in production.
    pub fn connect(&self) -> Box<dyn Connection> {
        let repo = Repository::open(self.session.store.repo.path()).expect("repo is opened");
        let hostname = self.session.hostname.0.clone();
        let session = crate::session::HostSession::new_test(
            repo,
            &hostname,
            self.apps.path(),
            self.units.path(),
        );
        let hello = Hello {
            version: protocol::VERSION.to_string(),
            hostname,
        };
        Box::new(LocalConnection {
            session,
            hello,
            message_buffer: VecDeque::new(),
        })
    }
}

/// In-memory connection that wraps a HostSession directly.
struct LocalConnection {
    session: crate::session::HostSession,
    hello: Hello,
    message_buffer: VecDeque<Message>,
}

impl Connection for LocalConnection {
    fn hello(&self) -> &Hello {
        &self.hello
    }

    fn send_request(&mut self, request: &Request) -> Result<()> {
        let buffer = &mut self.message_buffer;
        self.session
            .handle_request(request.clone(), &mut |msg| buffer.push_back(msg));
        Ok(())
    }

    fn read_message(&mut self) -> Result<Option<Message>> {
        Ok(self.message_buffer.pop_front())
    }

    fn close(&mut self) {}
}

/// Assert that a directory contains exactly the given files with the given contents.
///
/// Paths are relative to `dir`. The directory must not contain any other files.
pub fn assert_dir_contents(dir: &Path, expected: &[(&str, &[u8])]) {
    let mut actual: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    collect_files(dir, dir, &mut actual);

    let expected: BTreeMap<String, &[u8]> =
        expected.iter().map(|&(p, c)| (p.to_string(), c)).collect();

    let actual_keys: Vec<&String> = actual.keys().collect();
    let expected_keys: Vec<&String> = expected.keys().collect();
    assert_eq!(
        actual_keys,
        expected_keys,
        "file list mismatch in {}",
        dir.display()
    );

    for (path, expected_content) in &expected {
        let actual_content = &actual[path];
        assert_eq!(
            actual_content, expected_content,
            "content mismatch for {path}",
        );
    }
}

fn collect_files(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
    for entry in fs::read_dir(dir).expect("directory is readable") {
        let entry = entry.expect("entry is readable");
        let path = entry.path();
        if path.is_dir() {
            collect_files(base, &path, out);
        } else {
            let rel = path.strip_prefix(base).expect("path is under base");
            let content = fs::read(&path).expect("file is readable");
            out.insert(rel.to_string_lossy().to_string(), content);
        }
    }
}

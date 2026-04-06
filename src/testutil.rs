//! Shared test helpers: temp directories and convenience functions.

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use git2::Repository;

use crate::deploy::Connection;
use crate::error::Result;
use crate::protocol::{self, Hello, Message, Request};
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

/// A bare Git repository backed by a temporary directory.
pub struct TestRepo {
    pub repo: Repository,
    _store: TempDir,
}

impl TestRepo {
    pub fn new() -> Self {
        let store = TempDir::new("store");
        let repo = Repository::init_bare(store.path()).expect("repo is created");
        TestRepo {
            repo,
            _store: store,
        }
    }

    /// Create a commit with the given files, without touching the filesystem.
    pub fn commit(&self, files: &[(&str, &[u8])]) -> git2::Oid {
        commit_files(&self.repo, files).expect("commit succeeds")
    }

    /// Record that a host has already seen a commit.
    pub fn set_current(&self, host: &str, commit_oid: git2::Oid) {
        crate::store::set_ref(
            &self.repo,
            &format!("refs/remotes/{host}/current"),
            commit_oid,
            crate::store::RefUpdate::SetCurrent,
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
    _apps: TempDir,
    _units: TempDir,
}

impl TestHost {
    pub fn new() -> Self {
        let store = TempDir::new("store");
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        let repo = Repository::init_bare(store.path()).expect("repo is created");
        let session =
            crate::session::HostSession::new_test(repo, "web1", apps.path(), units.path());
        TestHost {
            session,
            _store: store,
            _apps: apps,
            _units: units,
        }
    }

    pub fn with_commit(files: &[(&str, &[u8])]) -> (Self, git2::Oid) {
        let store = TempDir::new("store");
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        let repo = Repository::init_bare(store.path()).expect("repo is created");
        let oid = commit_files(&repo, files).expect("commit succeeds");
        let session =
            crate::session::HostSession::new_test(repo, "web1", apps.path(), units.path());
        let host = TestHost {
            session,
            _store: store,
            _apps: apps,
            _units: units,
        };
        (host, oid)
    }

    /// Send a request and collect all response messages.
    pub fn collect(&mut self, request: Request) -> Vec<Message> {
        let mut responses = Vec::new();
        self.session
            .handle_request(request, &mut |r| responses.push(r));
        responses
    }

    /// Convert into a boxed `Connection` for deploy tests.
    pub fn into_connection(self) -> Box<dyn Connection> {
        let keepalive = vec![self._store, self._apps, self._units];
        make_local_connection(self.session, keepalive)
    }
}

/// Wrap a HostSession into a boxed Connection, without owning any TempDirs.
pub fn session_into_connection(session: crate::session::HostSession) -> Box<dyn Connection> {
    make_local_connection(session, Vec::new())
}

fn make_local_connection(
    session: crate::session::HostSession,
    keepalive: Vec<TempDir>,
) -> Box<dyn Connection> {
    let hello = Hello {
        version: protocol::VERSION.to_string(),
        hostname: "web1".to_string(),
    };
    Box::new(LocalConnection {
        session,
        hello,
        message_buffer: VecDeque::new(),
        _keepalive: keepalive,
    })
}

/// In-memory connection that wraps a HostSession directly.
struct LocalConnection {
    session: crate::session::HostSession,
    hello: Hello,
    message_buffer: VecDeque<Message>,
    /// TempDirs kept alive for the connection's lifetime.
    _keepalive: Vec<TempDir>,
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

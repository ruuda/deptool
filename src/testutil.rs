// Deptool -- A declarative configuration deployment tool.
// Copyright 2026 Ruud van Asseldonk

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// A copy of the License has been included in the root of the repository.

//! Shared test helpers: temp directories and convenience functions.

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use git2::{Oid, Repository};

use crate::agent::AgentSession;
use crate::deploy::{Connection, DeployObserver, DeployProgress, HostState};
use crate::error::{HostError, Result};
use crate::prim::Hostname;
use crate::protocol::{self, Hello, Message, Request};
use crate::setup::{BUILD_COMMIT, HostConnector};
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
fn build_tree_from_files(repo: &Repository, files: &[(&str, &[u8])]) -> Result<Oid> {
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

/// Create a commit with the given files and advance main.
///
/// Uses `refs/heads/main` as the parent (if it exists) to build a linear
/// commit chain in tests.
pub fn commit_files(store: &Store, files: &[(&str, &[u8])]) -> Result<Oid> {
    let tree_oid = build_tree_from_files(&store.repo, files)?;
    let parent: Vec<Oid> = store
        .repo
        .find_reference("refs/heads/main")
        .ok()
        .map(|r| r.peel_to_commit().expect("main points to a commit").id())
        .into_iter()
        .collect();
    let oid = store.commit_tree(tree_oid, &parent)?;
    store.set_ref("refs/heads/main", oid, RefUpdate::SetMain)?;
    Ok(oid)
}

/// A bare Git repository backed by a temporary directory.
pub struct TestRepo {
    pub store: Store,
    _dir: TempDir,
}

impl TestRepo {
    pub fn new() -> Self {
        let dir = TempDir::new("store");
        let store = Store::open_or_init(dir.path()).expect("store is created");
        TestRepo { store, _dir: dir }
    }

    /// Create a byte-for-byte copy of another TestRepo's git store.
    pub fn copy_from(other: &TestRepo) -> Self {
        let dir = TempDir::new("store");
        fs::remove_dir_all(dir.path()).expect("temp dir is removed");
        std::process::Command::new("cp")
            .args(["-r"])
            .arg(other.store.path())
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
    pub fn commit(&self, files: &[(&str, &[u8])]) -> Oid {
        commit_files(&self.store, files).expect("commit succeeds")
    }

    /// Build a single-host, single-app plan for display tests.
    pub fn plan_for(
        &self,
        commit: Oid,
        app: &str,
        diff: crate::plan::AppDiff,
    ) -> crate::error::Result<crate::plan::Plan> {
        let app_plan = crate::plan::compute_app_plan(&self.store, diff)?;
        let is_rollback_safe = app_plan.system.is_rollback_safe();
        Ok(crate::plan::Plan {
            commit,
            hosts: std::collections::BTreeMap::from([(
                crate::prim::Hostname::from("web1"),
                crate::plan::HostPlan {
                    apps: std::collections::BTreeMap::from([(app.into(), app_plan)]),
                    expected_current: None,
                    is_rollback_safe,
                },
            )]),
        })
    }

    /// Plan a deploy from a commit's tree.
    pub fn plan(&self, commit: Oid) -> crate::plan::Plan {
        let tree_oid = self.get_commit_tree_oid(commit);
        crate::plan::make_plan(&self.store, tree_oid)
            .expect("plan succeeds")
            .expect("plan has changes")
    }

    /// Get the tree OID for a commit.
    pub fn get_commit_tree_oid(&self, commit_oid: Oid) -> Oid {
        self.store
            .repo
            .find_commit(commit_oid)
            .expect("commit exists")
            .tree_id()
    }

    /// Read the driver-side tracking ref for a host (`refs/remotes/{host}/current`).
    pub fn get_host_tracking_ref(&self, host: &str) -> Option<Oid> {
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
    pub fn set_host_tracking_ref(&self, host: &str, commit_oid: Oid) {
        self.store
            .set_ref(
                &format!("refs/remotes/{host}/current"),
                commit_oid,
                RefUpdate::ApplyComplete,
            )
            .expect("ref is set");
    }
}

/// A host-side test environment: a bare repo with an apps directory.
///
/// Wraps a `AgentSession` and provides helpers for sending requests and
/// collecting responses.
pub struct TestHost {
    pub session: AgentSession,
    _store: TempDir,
    apps: TempDir,
}

impl TestHost {
    pub fn apps_path(&self) -> &Path {
        self.apps.path()
    }

    /// Construct from pre-built parts.
    pub fn from_parts(session: AgentSession, store: TempDir, apps: TempDir) -> Self {
        TestHost {
            session,
            _store: store,
            apps,
        }
    }

    /// Create a new host with a fresh bare repo.
    pub fn new(hostname: &str) -> Self {
        let store = TempDir::new("store");
        let apps = TempDir::new("apps");
        let s = Store::open_or_init(store.path()).expect("store is created");
        let session = AgentSession::new_test(s.repo, hostname, apps.path());
        TestHost {
            session,
            _store: store,
            apps,
        }
    }

    pub fn with_commit(hostname: &str, files: &[(&str, &[u8])]) -> (Self, Oid) {
        let host = Self::new(hostname);
        // Open the same repo as a Store to create the commit.
        let store = Store::open(host.session.store.path()).expect("repo is opened");
        let oid = commit_files(&store, files).expect("commit succeeds");
        (host, oid)
    }

    /// Set the host-local `refs/heads/current` ref.
    pub fn set_current(&self, commit_oid: Oid) {
        self.session
            .store
            .set_ref(
                "refs/heads/current",
                commit_oid,
                RefUpdate::SetCurrent {
                    operator: "deckard@spinner",
                },
            )
            .expect("ref is set");
    }

    /// The commit a ref points to, if the ref exists.
    pub fn get_ref(&self, refname: &str) -> Option<Oid> {
        self.session
            .store
            .repo
            .find_reference(refname)
            .ok()
            .map(|r| r.peel_to_commit().expect("ref points to a commit").id())
    }

    /// Read the reflog as (new_oid, message) pairs, newest first.
    pub fn reflog(&self, refname: &str) -> Vec<(Oid, String)> {
        let reflog = self
            .session
            .store
            .repo
            .reflog(refname)
            .expect("reflog exists");
        (0..reflog.len())
            .map(|i| {
                let entry = reflog.get(i).expect("reflog entry exists");
                let msg = entry.message().expect("valid utf-8").to_string();
                (entry.id_new(), msg)
            })
            .collect()
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
    /// new SSH connection to the same host. The apps directory is shared
    /// across connections, as it would be in production.
    pub fn connect(&self) -> Box<dyn Connection> {
        Self::open_connection(
            self.session.store.path(),
            &self.session.hostname.0,
            self.apps.path(),
        )
    }

    /// Return a thread-safe factory for creating connections to this host.
    ///
    /// The factory captures owned paths (no references to self), so it is
    /// `Send + Sync` and safe to call from multiple threads.
    pub fn connector(&self) -> impl Fn() -> Box<dyn Connection> + Send + Sync + 'static {
        let store_path = self.session.store.path().to_path_buf();
        let hostname = self.session.hostname.0.clone();
        let apps_path = self.apps.path().to_path_buf();
        move || Self::open_connection(&store_path, &hostname, &apps_path)
    }

    fn open_connection(
        store_path: &std::path::Path,
        hostname: &str,
        apps_path: &std::path::Path,
    ) -> Box<dyn Connection> {
        let repo = Repository::open(store_path).expect("repo is opened");
        let current_commit = repo
            .find_reference("refs/heads/current")
            .ok()
            .map(|r| r.peel_to_commit().expect("current ref is a commit").id());
        let session = AgentSession::new_test(repo, hostname, apps_path);
        let hello = Hello {
            version: protocol::VERSION.to_string(),
            build_commit: BUILD_COMMIT.to_string(),
            hostname: hostname.to_string(),
            current_commit,
        };
        Box::new(LocalConnection {
            session,
            hello,
            message_buffer: VecDeque::new(),
        })
    }
}

/// In-memory connection that wraps a AgentSession directly.
struct LocalConnection {
    session: AgentSession,
    hello: Hello,
    message_buffer: VecDeque<Message>,
}

impl Connection for LocalConnection {
    fn hello(&self) -> &Hello {
        &self.hello
    }

    fn send_request(
        &mut self,
        request: &Request,
    ) -> std::result::Result<(), crate::error::HostError> {
        let buffer = &mut self.message_buffer;
        self.session
            .handle_request(request.clone(), &mut |msg| buffer.push_back(msg));
        Ok(())
    }

    fn read_message(&mut self) -> std::result::Result<Option<Message>, crate::error::HostError> {
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

/// Observer that discards all progress updates.
pub struct NoopObserver;

impl DeployObserver for NoopObserver {
    fn state_changed(&mut self, _: &BTreeMap<Hostname, HostState>) {}
    fn log_message(&mut self, _: &BTreeMap<Hostname, HostState>, _: &Hostname, _: &str) {}
}

/// In-memory connector for tests, using pre-built connection factories.
pub struct TestConnector {
    factories: BTreeMap<Hostname, Box<dyn Fn() -> Box<dyn Connection> + Send + Sync>>,
}

pub fn test_connector(hosts: &[&TestHost]) -> TestConnector {
    let mut factories = BTreeMap::new();
    for host in hosts {
        let hostname = host.session.hostname.clone();
        let factory: Box<dyn Fn() -> Box<dyn Connection> + Send + Sync> =
            Box::new(host.connector());
        factories.insert(hostname, factory);
    }
    TestConnector { factories }
}

impl HostConnector for TestConnector {
    fn connect(&self, host: &Hostname) -> std::result::Result<Box<dyn Connection>, HostError> {
        let factory = self.factories.get(host).ok_or_else(|| {
            HostError::ConnectionFailed(format!(
                "ssh: connect to host {host}: Connection timed out"
            ))
        })?;
        Ok(factory())
    }

    fn install(&self, _host: &Hostname) -> std::result::Result<(), HostError> {
        panic!("install not expected in tests")
    }
}

pub fn test_progress(hosts: &[&str]) -> DeployProgress {
    let hosts = hosts.iter().map(|h| Hostname::from(*h)).collect();
    DeployProgress::new(hosts, Box::new(NoopObserver))
}

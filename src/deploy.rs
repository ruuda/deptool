//! Execute a deployment plan by driving remote host sessions.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

use std::collections::BTreeMap;

use crate::error::{Error, Result};
use crate::plan::Plan;
use crate::prim::{Hostname, Oid};
use crate::protocol::{self, Hello, Message, Request};

pub enum HostState {
    Pending,
    Connecting,
    Locked,
    Pushing,
    Applying,
    Done,
    Stale,
    LockBusy,
    Failed(String),
}

impl HostState {
    pub fn is_failure(&self) -> bool {
        matches!(
            self,
            HostState::Stale | HostState::LockBusy | HostState::Failed(_)
        )
    }
}

impl std::fmt::Display for HostState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostState::Pending => f.write_str("pending"),
            HostState::Connecting => f.write_str("connecting"),
            HostState::Locked => f.write_str("locked"),
            HostState::Pushing => f.write_str("pushing"),
            HostState::Applying => f.write_str("applying"),
            HostState::Done => f.write_str("done"),
            HostState::Stale => f.write_str("stale"),
            HostState::LockBusy => f.write_str("locked by another deploy"),
            HostState::Failed(reason) => write!(f, "failed: {reason}"),
        }
    }
}

/// Tracks per-host deploy state, calling `on_change` after each update.
pub struct DeployProgress {
    states: BTreeMap<Hostname, HostState>,
    on_change: Box<dyn FnMut(&BTreeMap<Hostname, HostState>)>,
}

impl DeployProgress {
    pub fn new(
        hosts: Vec<Hostname>,
        on_change: Box<dyn FnMut(&BTreeMap<Hostname, HostState>)>,
    ) -> Self {
        let states = hosts.into_iter().map(|h| (h, HostState::Pending)).collect();
        Self { states, on_change }
    }

    pub fn update(&mut self, host: &Hostname, state: HostState) {
        *self.states.get_mut(host).expect("host is in the plan") = state;
        (self.on_change)(&self.states);
    }

    pub fn has_failures(&self) -> bool {
        self.states.values().any(|s| s.is_failure())
    }

    pub fn num_failed(&self) -> usize {
        self.states.values().filter(|s| s.is_failure()).count()
    }
}

pub trait Connection {
    fn hello(&self) -> &Hello;
    fn send_request(&mut self, request: &Request) -> Result<()>;
    fn read_message(&mut self) -> Result<Option<Message>>;
    /// Close stdin to signal no more requests are coming.
    fn close(&mut self);
}

// TODO: Maybe we should rename "session" to "agent" after all. Then this can be
// AgentSession or something, the thing on the controller/initiator/operator
// side that enables us to talk to the agent.
// TODO: This struct needs a docstring that documents its purpose.
pub struct RemoteSession {
    // Drop order is declaration order: close stdin first so the child can
    // finish, then close our reader, then reap the child process.
    writer: Option<ChildStdin>,
    reader: BufReader<ChildStdout>,
    hello: Hello,

    // Not dead, needed to keep the child process alive.
    #[allow(dead_code)]
    child: Child,
}

impl RemoteSession {
    /// Spawn the session command and read the hello message.
    ///
    /// Returns `Err(AgentNotInstalled)` if the process exits with an error
    /// before sending a hello, indicating that likely the binary is not on the
    /// target.
    pub fn new(mut cmd: Command) -> Result<Self> {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;

        let mut reader = BufReader::new(child.stdout.take().expect("stdout is piped"));
        let writer = child.stdin.take().expect("stdin is piped");

        let mut line = String::new();
        let hello = match reader.read_line(&mut line) {
            Ok(0) => {
                // EOF before hello: check whether the binary was missing.
                match child.wait()?.code() {
                    // When we run `deptool` directly and the shell reports that
                    // the binary is not found, the exit code is 127, but when
                    // we run it through sudo, then sudo fails and exits with
                    // code 1. TODO: Would it be better to start the agent as
                    // the current user and the let it reexec itself under sudo
                    // if its uid is unexpected?
                    Some(1 | 127) => return Err(Error::AgentNotInstalled),
                    other => {
                        return Err(Error::ProtocolError(format!(
                            "agent exited before sending hello; exit status {other:?}"
                        )));
                    }
                }
            }
            Ok(_) => serde_json::from_str(&line)?,
            Err(e) => return Err(e.into()),
        };

        Ok(RemoteSession {
            child,
            reader,
            hello,
            writer: Some(writer),
        })
    }
}

impl Connection for RemoteSession {
    fn hello(&self) -> &Hello {
        &self.hello
    }

    fn send_request(&mut self, request: &Request) -> Result<()> {
        let writer = self.writer.as_mut().expect("stdin is still open");
        serde_json::to_writer(&mut *writer, request)?;
        writeln!(writer)?;
        writer.flush()?;
        Ok(())
    }

    fn read_message(&mut self) -> Result<Option<Message>> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(None);
        }
        let message: Message = serde_json::from_str(&line)?;
        Ok(Some(message))
    }

    fn close(&mut self) {
        self.writer.take();
    }
}

pub struct StaleHost {
    pub expected_commit: Option<Oid>,
    pub actual_commit: Option<Oid>,
    pub connection: Box<dyn Connection>,
}

pub struct LockResult {
    pub locked: Vec<(Hostname, Box<dyn Connection>)>,
    pub stale: Vec<(Hostname, StaleHost)>,
}

/// Open sessions and acquire deploy locks on all hosts in the plan.
///
/// Tries every host even if some fail, so the caller gets all stale info
/// in one pass. Hosts are locked in plan iteration order (asciibetical,
/// since the plan uses a BTreeMap) to avoid deadlocks in case of concurrent
/// deploys.
pub fn lock_hosts(
    plan: &Plan,
    mut connect: impl FnMut(&Hostname) -> Result<Box<dyn Connection>>,
    progress: &mut DeployProgress,
) -> LockResult {
    let mut result = LockResult {
        locked: Vec::new(),
        stale: Vec::new(),
    };

    for (host, host_plan) in &plan.hosts {
        progress.update(host, HostState::Connecting);

        let mut conn = match connect(host) {
            Ok(c) => c,
            Err(err) => {
                progress.update(host, HostState::Failed(err.to_string()));
                continue;
            }
        };

        assert_eq!(
            conn.hello().version,
            protocol::VERSION,
            "agent version matches operator version"
        );

        let lock_request = Request::Lock {
            expected_current_commit: host_plan.expected_current.clone(),
        };
        if let Err(err) = conn.send_request(&lock_request) {
            progress.update(host, HostState::Failed(err.to_string()));
            continue;
        }

        match conn.read_message() {
            Ok(Some(Message::Locked)) => {
                progress.update(host, HostState::Locked);
                result.locked.push((host.clone(), conn));
            }
            Ok(Some(Message::LockStale {
                expected_commit,
                actual_commit,
            })) => {
                progress.update(host, HostState::Stale);
                result.stale.push((
                    host.clone(),
                    StaleHost {
                        expected_commit,
                        actual_commit,
                        connection: conn,
                    },
                ));
            }
            Ok(Some(Message::LockBusy)) => {
                progress.update(host, HostState::LockBusy);
            }
            other => {
                progress.update(
                    host,
                    HostState::Failed(format!("unexpected lock response: {other:?}")),
                );
            }
        }
    }

    result
}

/// Send Apply to all locked hosts and stream responses.
pub fn apply_hosts(
    plan: &Plan,
    connections: &mut [(Hostname, Box<dyn Connection>)],
    progress: &mut DeployProgress,
    mut on_message: impl FnMut(&Hostname, Message),
) -> Result<()> {
    for (host, conn) in connections.iter_mut() {
        progress.update(host, HostState::Applying);
        let request = Request::Apply {
            target_commit: plan.commit.clone(),
        };
        conn.send_request(&request)?;
        conn.close();
        while let Some(message) = conn.read_message()? {
            if matches!(message, Message::ApplyComplete { .. }) {
                progress.update(host, HostState::Done);
            }
            on_message(host, message);
        }
    }
    Ok(())
}

/// Send packfiles to all locked hosts over their session connections.
///
/// Packs are built per-host, containing only objects the host doesn't
/// already have, based on the plan's `expected_current` for that host. We only
/// execute this if we acquired a lock on the host, and we can only take the
/// lock if our expected commit is up to date, so `expected_current` is a valid
/// base for the packfile.
pub fn push_packs(
    repo: &git2::Repository,
    plan: &Plan,
    connections: &mut [(Hostname, Box<dyn Connection>)],
    progress: &mut DeployProgress,
) -> Result<()> {
    for (host, conn) in connections.iter_mut() {
        progress.update(host, HostState::Pushing);
        let have_commit = plan.hosts[host]
            .expected_current
            .as_ref()
            .map(git2::Oid::from);
        let pack_bytes =
            crate::store::create_pack(repo, git2::Oid::from(&plan.commit), have_commit)?;
        let encoded = BASE64.encode(&pack_bytes);
        conn.send_request(&Request::ReceivePack { pack_data: encoded })?;
        match conn.read_message()? {
            Some(Message::PackReceived) => {}
            Some(Message::Error { message }) => {
                return Err(Error::ProtocolError(format!(
                    "{host}: receive pack failed: {message}"
                )));
            }
            other => {
                return Err(Error::ProtocolError(format!(
                    "{host}: unexpected response to ReceivePack: {other:?}"
                )));
            }
        }
    }
    Ok(())
}

/// Fetch objects from stale hosts over their still-open sessions.
///
/// For each stale host whose actual commit we don't already have, sends
/// `RequestObjects` and receives a packfile. Updates the local tracking
/// ref for each host.
pub fn fetch_stale_objects(
    repo: &git2::Repository,
    stale: &mut [(Hostname, StaleHost)],
) -> Result<()> {
    for (host, info) in stale.iter_mut() {
        let actual_commit = match &info.actual_commit {
            Some(c) => c.clone(),
            None => continue,
        };

        // Fetch the pack if we don't already have this commit.
        if repo.find_commit(git2::Oid::from(&actual_commit)).is_err() {
            info.connection.send_request(&Request::RequestObjects {
                have_commit: info.expected_commit.clone(),
            })?;

            match info.connection.read_message()? {
                Some(Message::SendPack { pack_data }) => {
                    let bytes = BASE64
                        .decode(&pack_data)
                        .expect("SendPack contains valid base64");
                    crate::store::write_pack(repo, &bytes)?;
                }
                Some(Message::Error { message }) => {
                    return Err(Error::ProtocolError(format!(
                        "{host}: RequestObjects failed: {message}"
                    )));
                }
                other => {
                    return Err(Error::ProtocolError(format!(
                        "{host}: unexpected response to RequestObjects: {other:?}"
                    )));
                }
            }
        }

        crate::store::set_ref(
            repo,
            &format!("refs/remotes/{host}/current"),
            git2::Oid::from(&actual_commit),
            crate::store::RefUpdate::FetchStale,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};

    use super::*;
    use crate::plan::HostPlan;
    use crate::prim::Oid;
    use crate::session::HostSession;
    use crate::testutil::{TempDir, commit_files};

    fn test_progress(hosts: &[&str]) -> DeployProgress {
        let hosts = hosts.iter().map(|h| Hostname::from(*h)).collect();
        DeployProgress::new(hosts, Box::new(|_| {}))
    }

    /// In-memory connection that wraps a HostSession directly.
    struct LocalConnection {
        session: HostSession,
        hello: Hello,
        message_buffer: VecDeque<Message>,
        _store: TempDir,
        _apps: TempDir,
        _units: TempDir,
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

    struct TestHost {
        conn: Box<dyn Connection>,
        commit_oid: git2::Oid,
    }

    fn test_host() -> Result<TestHost> {
        let store = TempDir::new("store");
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        let repo = git2::Repository::init_bare(store.path())?;
        let commit_oid = commit_files(&repo, &[("web1/nginx/nginx.conf", b"v1")])?;
        let on_units_changed = Box::new(|_: &_| Ok(()));
        let session = HostSession::new(
            repo,
            "web1".into(),
            apps.path().to_path_buf(),
            units.path().to_path_buf(),
            on_units_changed,
        );
        let hello = Hello {
            version: protocol::VERSION.to_string(),
            hostname: "web1".to_string(),
        };
        let conn = Box::new(LocalConnection {
            session,
            hello,
            message_buffer: VecDeque::new(),
            _store: store,
            _apps: apps,
            _units: units,
        });
        Ok(TestHost { conn, commit_oid })
    }

    fn single_host_plan(commit: Oid) -> Plan {
        Plan {
            commit,
            hosts: BTreeMap::from([(
                Hostname::from("web1"),
                HostPlan {
                    apps: BTreeMap::new(),
                    expected_current: None,
                },
            )]),
        }
    }

    #[test]
    fn lock_and_apply_emits_apply_complete_for_fresh_host() -> Result<()> {
        let host = test_host()?;
        let plan = single_host_plan(host.commit_oid.into());
        let mut conn = Some(host.conn);
        let mut progress = test_progress(&["web1"]);
        let mut lock_result = lock_hosts(
            &plan,
            |_| Ok(conn.take().expect("connect called once")),
            &mut progress,
        );

        assert_eq!(lock_result.locked.len(), 1);

        let mut messages = Vec::new();
        apply_hosts(&plan, &mut lock_result.locked, &mut progress, |_, msg| {
            messages.push(msg)
        })?;

        assert!(matches!(
            messages.last(),
            Some(Message::ApplyComplete { .. })
        ));
        Ok(())
    }

    #[test]
    fn lock_reports_stale_when_current_ref_mismatches() -> Result<()> {
        let store = TempDir::new("store");
        let repo = git2::Repository::init_bare(store.path())?;

        let actual_commit = commit_files(&repo, &[("web1/nginx/conf", b"v1")])?;
        crate::store::set_ref(
            &repo,
            "refs/heads/current",
            actual_commit,
            crate::store::RefUpdate::SetCurrent,
        )?;

        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        let on_units_changed = Box::new(|_: &_| Ok(()));
        let session = HostSession::new(
            repo,
            "web1".into(),
            apps.path().to_path_buf(),
            units.path().to_path_buf(),
            on_units_changed,
        );
        let hello = Hello {
            version: protocol::VERSION.to_string(),
            hostname: "web1".to_string(),
        };
        let conn: Box<dyn Connection> = Box::new(LocalConnection {
            session,
            hello,
            message_buffer: VecDeque::new(),
            _store: store,
            _apps: apps,
            _units: units,
        });

        let plan = single_host_plan(Oid::from("0000000000000000000000000000000000000000"));
        let mut conn = Some(conn);
        let mut progress = test_progress(&["web1"]);
        let lock_result = lock_hosts(
            &plan,
            |_| Ok(conn.take().expect("connect called once")),
            &mut progress,
        );

        assert!(lock_result.locked.is_empty());
        assert_eq!(lock_result.stale.len(), 1);
        assert!(lock_result.stale[0].1.actual_commit.is_some());
        Ok(())
    }

    #[test]
    fn lock_push_pack_and_apply_with_separate_driver_and_target() -> Result<()> {
        // Driver has the commit, target starts empty.
        let driver_store = TempDir::new("driver");
        let driver_repo = git2::Repository::init_bare(driver_store.path())?;
        let commit_oid = commit_files(&driver_repo, &[("web1/app/conf", b"hello")])?;

        let target_store = TempDir::new("target");
        let target_repo = git2::Repository::init_bare(target_store.path())?;
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        let on_units_changed: Box<dyn Fn(&_) -> Result<()>> = Box::new(|_: &_| Ok(()));
        let session = HostSession::new(
            target_repo,
            "web1".into(),
            apps.path().to_path_buf(),
            units.path().to_path_buf(),
            on_units_changed,
        );
        let hello = Hello {
            version: protocol::VERSION.to_string(),
            hostname: "web1".to_string(),
        };
        let conn: Box<dyn Connection> = Box::new(LocalConnection {
            session,
            hello,
            message_buffer: VecDeque::new(),
            _store: target_store,
            _apps: apps,
            _units: units,
        });

        let plan = single_host_plan(commit_oid.into());
        let mut conn = Some(conn);
        let mut progress = test_progress(&["web1"]);
        let lock_result = lock_hosts(
            &plan,
            |_| Ok(conn.take().expect("connect called once")),
            &mut progress,
        );

        let mut connections = lock_result.locked;
        push_packs(&driver_repo, &plan, &mut connections, &mut progress)?;

        let mut messages = Vec::new();
        apply_hosts(&plan, &mut connections, &mut progress, |_, msg| {
            messages.push(msg)
        })?;

        assert!(matches!(
            messages.last(),
            Some(Message::ApplyComplete { .. })
        ));
        Ok(())
    }

    /// Push objects and apply a commit on a target via HostSession.
    ///
    /// Uses the same create_pack + write_pack machinery as production.
    fn push_and_apply(
        driver_repo: &git2::Repository,
        target_path: &std::path::Path,
        apps_path: &std::path::Path,
        units_path: &std::path::Path,
        target_commit: git2::Oid,
        have_commit: Option<git2::Oid>,
        expected_current: Option<Oid>,
    ) -> Result<()> {
        let pack = crate::store::create_pack(driver_repo, target_commit, have_commit)?;
        let repo = git2::Repository::open(target_path)?;
        crate::store::write_pack(&repo, &pack)?;

        let on_units_changed: Box<dyn Fn(&_) -> Result<()>> = Box::new(|_: &_| Ok(()));
        let mut session = HostSession::new(
            repo,
            "web1".into(),
            apps_path.to_path_buf(),
            units_path.to_path_buf(),
            on_units_changed,
        );
        let mut messages = Vec::new();
        session.handle_request(
            Request::Lock {
                expected_current_commit: expected_current,
            },
            &mut |msg| messages.push(msg),
        );
        assert_eq!(messages, vec![Message::Locked], "lock should succeed");

        messages.clear();
        session.handle_request(
            Request::Apply {
                target_commit: target_commit.into(),
            },
            &mut |msg| messages.push(msg),
        );
        assert!(
            matches!(messages.last(), Some(Message::ApplyComplete { .. })),
            "apply should succeed, got: {messages:?}",
        );
        Ok(())
    }

    #[test]
    fn fetch_resolves_stale_after_concurrent_deploy() -> Result<()> {
        let target_store = TempDir::new("target");
        let target_repo = git2::Repository::init_bare(target_store.path())?;
        drop(target_repo);
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");

        // Driver A commits v1, pushes pack to target, applies.
        let driver_a = TempDir::new("driver_a");
        let repo_a = git2::Repository::init_bare(driver_a.path())?;
        let commit_v1 = commit_files(&repo_a, &[("web1/app/conf", b"v1")])?;

        push_and_apply(
            &repo_a,
            target_store.path(),
            apps.path(),
            units.path(),
            commit_v1,
            None,
            None,
        )?;
        crate::store::set_ref(
            &repo_a,
            "refs/remotes/web1/current",
            commit_v1,
            crate::store::RefUpdate::ApplyComplete,
        )?;

        // Driver B: copy of A at this point (same main, same tracking ref).
        let driver_b = TempDir::new("driver_b");
        std::fs::remove_dir_all(driver_b.path())?;
        std::process::Command::new("cp")
            .args(["-r"])
            .arg(driver_a.path())
            .arg(driver_b.path())
            .status()?;
        let repo_b = git2::Repository::open(driver_b.path())?;

        // Driver A deploys v2.
        let commit_v2 = commit_files(&repo_a, &[("web1/app/conf", b"v2")])?;
        push_and_apply(
            &repo_a,
            target_store.path(),
            apps.path(),
            units.path(),
            commit_v2,
            Some(commit_v1),
            Some(commit_v1.into()),
        )?;

        // Driver B commits v3 (diverges from A's v2) and tries to lock.
        let commit_v3 = commit_files(&repo_b, &[("web1/app/conf", b"v3")])?;

        // B's plan still thinks current is commit_v1.
        let plan = Plan {
            commit: commit_v3.into(),
            hosts: BTreeMap::from([(
                Hostname::from("web1"),
                HostPlan {
                    apps: BTreeMap::new(),
                    expected_current: Some(commit_v1.into()),
                },
            )]),
        };

        // Lock should report stale: target has commit_v2, not commit_v1.
        let mut progress = test_progress(&["web1"]);
        let mut lock_result = lock_hosts(
            &plan,
            |_| {
                let repo = git2::Repository::open(target_store.path())?;
                let on_units_changed: Box<dyn Fn(&_) -> Result<()>> = Box::new(|_: &_| Ok(()));
                let session = HostSession::new(
                    repo,
                    "web1".into(),
                    apps.path().to_path_buf(),
                    units.path().to_path_buf(),
                    on_units_changed,
                );
                let hello = Hello {
                    version: protocol::VERSION.to_string(),
                    hostname: "web1".to_string(),
                };
                Ok(Box::new(LocalConnection {
                    session,
                    hello,
                    message_buffer: VecDeque::new(),
                    _store: TempDir::new("unused"),
                    _apps: TempDir::new("unused"),
                    _units: TempDir::new("unused"),
                }) as Box<dyn Connection>)
            },
            &mut progress,
        );

        assert!(lock_result.locked.is_empty());
        assert_eq!(lock_result.stale.len(), 1);
        assert_eq!(lock_result.stale[0].1.actual_commit, Some(commit_v2.into()));

        // Driver B doesn't have commit_v2 locally.
        assert!(
            repo_b.find_commit(commit_v2).is_err(),
            "commit_v2 should not exist in driver B yet"
        );

        // Fetch objects over the stale session connection.
        fetch_stale_objects(&repo_b, &mut lock_result.stale)?;

        // B now has the objects and tracking ref.
        let tracking_ref = repo_b
            .find_reference("refs/remotes/web1/current")?
            .peel_to_commit()?;
        assert_eq!(tracking_ref.id(), commit_v2);
        assert!(repo_b.find_commit(commit_v2).is_ok());

        Ok(())
    }

    // TODO: Add an integration test that spawns the real deptool binary in
    // local mode and exercises the full stdin/stdout protocol roundtrip.
}

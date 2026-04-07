//! Execute a deployment plan by driving remote host sessions.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

use std::collections::BTreeMap;

use git2::Oid;

use crate::error::{Error, Result};
use crate::plan::Plan;
use crate::prim::Hostname;
use crate::protocol::{self, Hello, Message, Request};
use crate::store::{RefUpdate, Store};

#[derive(Debug, PartialEq)]
pub enum HostState {
    Pending,
    Connecting,
    InstallingAgent,
    Locked,
    Pushing,
    Applying,
    Done,
    Stale,
    LockBusy(Option<String>),
    Failed(String),
}

impl HostState {
    pub fn is_failure(&self) -> bool {
        matches!(
            self,
            HostState::Stale | HostState::LockBusy(_) | HostState::Failed(_)
        )
    }
}

impl std::fmt::Display for HostState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostState::Pending => f.write_str("pending"),
            HostState::Connecting => f.write_str("connecting"),
            HostState::InstallingAgent => f.write_str("installing agent"),
            HostState::Locked => f.write_str("locked"),
            HostState::Pushing => f.write_str("pushing"),
            HostState::Applying => f.write_str("applying"),
            HostState::Done => f.write_str("done"),
            HostState::Stale => f.write_str("stale"),
            HostState::LockBusy(Some(who)) => write!(f, "locked by {who}"),
            HostState::LockBusy(None) => f.write_str("locked by another deploy"),
            HostState::Failed(reason) => write!(f, "failed: {reason}"),
        }
    }
}

/// Receives deploy events for display or logging.
pub trait DeployObserver {
    fn state_changed(&mut self, states: &BTreeMap<Hostname, HostState>);
    fn log_message(&mut self, states: &BTreeMap<Hostname, HostState>, host: &Hostname, text: &str);
}

/// Tracks per-host deploy state, notifying an observer on changes.
pub struct DeployProgress {
    states: BTreeMap<Hostname, HostState>,
    observer: Box<dyn DeployObserver>,
}

impl DeployProgress {
    pub fn new(hosts: Vec<Hostname>, observer: Box<dyn DeployObserver>) -> Self {
        let states = hosts.into_iter().map(|h| (h, HostState::Pending)).collect();
        Self { states, observer }
    }

    pub fn update(&mut self, host: &Hostname, state: HostState) {
        *self.states.get_mut(host).expect("host is in the plan") = state;
        self.observer.state_changed(&self.states);
    }

    pub fn log_message(&mut self, host: &Hostname, text: &str) {
        self.observer.log_message(&self.states, host, text);
    }

    pub fn has_failures(&self) -> bool {
        self.states.values().any(|s| s.is_failure())
    }

    pub fn num_failed(&self) -> usize {
        self.states.values().filter(|s| s.is_failure()).count()
    }

    /// Get a host's state by hostname.
    ///
    /// This exists to make the tests a bit less verbose.
    #[cfg(test)]
    pub fn state(&self, host: &str) -> &HostState {
        &self.states[&Hostname(host.into())]
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
            .stderr(Stdio::piped())
            .spawn()?;

        let mut reader = BufReader::new(child.stdout.take().expect("stdout is piped"));
        let writer = child.stdin.take().expect("stdin is piped");

        let mut line = String::new();
        let hello = match reader.read_line(&mut line) {
            Ok(0) => {
                // EOF before hello — the process exited without speaking
                // our protocol. Read stderr for diagnostics and classify
                // by exit code.
                let mut stderr = String::new();
                if let Some(mut err) = child.stderr.take() {
                    let _ = std::io::Read::read_to_string(&mut err, &mut stderr);
                }
                let stderr = stderr.trim();
                match child.wait()?.code() {
                    // 255: SSH connection failure (host unreachable, DNS
                    // failure, connection refused, timeout, etc.)
                    Some(255) => {
                        return Err(Error::ConnectionFailed(stderr.to_string()));
                    }
                    // When we run `deptool` directly and the shell reports that
                    // the binary is not found, the exit code is 127, but when
                    // we run it through sudo, then sudo fails and exits with
                    // code 1. TODO: Would it be better to start the agent as
                    // the current user and the let it reexec itself under sudo
                    // if its uid is unexpected?
                    Some(1 | 127) => return Err(Error::AgentNotInstalled),
                    Some(code) => {
                        return Err(Error::ProtocolError(format!(
                            "ssh exited with status {code} \
                             before agent sent hello: {stderr}"
                        )));
                    }
                    // On Unix, None means the process was killed by a signal.
                    None => {
                        return Err(Error::ProtocolError(format!(
                            "ssh killed by signal \
                             before agent sent hello: {stderr}"
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
    operator: &str,
    mut connect: impl FnMut(&Hostname) -> Result<Box<dyn Connection>>,
    mut install: impl FnMut(&Hostname) -> Result<()>,
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
            Err(Error::AgentNotInstalled) => {
                progress.update(host, HostState::InstallingAgent);
                if let Err(err) = install(host) {
                    progress.update(host, HostState::Failed(err.to_string()));
                    continue;
                }
                match connect(host) {
                    Ok(c) => c,
                    Err(err) => {
                        progress.update(host, HostState::Failed(err.to_string()));
                        continue;
                    }
                }
            }
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
            operator: operator.to_string(),
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
            Ok(Some(Message::LockBusy { held_by })) => {
                progress.update(host, HostState::LockBusy(held_by));
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
    store: &Store,
    plan: &Plan,
    connections: &mut [(Hostname, Box<dyn Connection>)],
    progress: &mut DeployProgress,
) -> Result<()> {
    for (host, conn) in connections.iter_mut() {
        progress.update(host, HostState::Applying);
        let request = Request::Apply {
            target_commit: plan.commit.clone(),
        };
        conn.send_request(&request)?;
        conn.close();
        while let Some(message) = conn.read_message()? {
            match &message {
                Message::ApplyComplete { commit, .. } => {
                    progress.update(host, HostState::Done);
                    store.set_ref(
                        &format!("refs/remotes/{host}/current"),
                        *commit,
                        RefUpdate::ApplyComplete,
                    )?;
                }
                Message::SystemdUnitChangeFailed { unit, operation } => {
                    progress.log_message(host, &format!("{operation} {unit}: failed"));
                }
                Message::SystemdUnitStatus { output } => {
                    progress.log_message(host, output.trim_end());
                }
                _ => {}
            }
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
    store: &Store,
    plan: &Plan,
    connections: &mut [(Hostname, Box<dyn Connection>)],
    progress: &mut DeployProgress,
) -> Result<()> {
    for (host, conn) in connections.iter_mut() {
        progress.update(host, HostState::Pushing);
        let have_commit = plan.hosts[host].expected_current;
        let pack_bytes = store.create_pack(plan.commit, have_commit)?;
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
pub fn fetch_stale_objects(store: &Store, stale: &mut [(Hostname, StaleHost)]) -> Result<()> {
    for (host, info) in stale.iter_mut() {
        let actual_commit = match &info.actual_commit {
            Some(c) => c.clone(),
            None => continue,
        };

        // Fetch the pack if we don't already have this commit.
        if store.repo.find_commit(actual_commit).is_err() {
            info.connection.send_request(&Request::RequestObjects {
                have_commit: info.expected_commit.clone(),
            })?;

            match info.connection.read_message()? {
                Some(Message::SendPack { pack_data }) => {
                    let bytes = BASE64
                        .decode(&pack_data)
                        .expect("SendPack contains valid base64");
                    store.write_pack(&bytes)?;
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

        store.set_ref(
            &format!("refs/remotes/{host}/current"),
            actual_commit,
            RefUpdate::FetchStale,
        )?;
    }
    Ok(())
}

/// Run a full deployment: lock all hosts, push packs, and apply.
///
/// If any host fails to lock (stale, busy, or connection error), fetches
/// objects from stale hosts and aborts without pushing or applying to any
/// host.
pub fn run_deploy(
    store: &Store,
    plan: &Plan,
    operator: &str,
    connect: impl FnMut(&Hostname) -> Result<Box<dyn Connection>>,
    install: impl FnMut(&Hostname) -> Result<()>,
    progress: &mut DeployProgress,
) -> Result<()> {
    let mut lock_result = lock_hosts(plan, operator, connect, install, progress);

    if progress.has_failures() {
        // Fetch objects from stale hosts so we have the data for the next plan.
        fetch_stale_objects(store, &mut lock_result.stale)?;
        let n = progress.num_failed();
        return Err(Error::DeployFailed(format!(
            "failed to lock {n} host(s), aborting",
        )));
    }

    push_packs(store, plan, &mut lock_result.locked, progress)?;
    apply_hosts(store, plan, &mut lock_result.locked, progress)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::HostPlan;
    use crate::testutil::{TestHost, TestRepo};

    struct NoopObserver;
    impl DeployObserver for NoopObserver {
        fn state_changed(&mut self, _: &BTreeMap<Hostname, HostState>) {}
        fn log_message(&mut self, _: &BTreeMap<Hostname, HostState>, _: &Hostname, _: &str) {}
    }

    fn test_progress(hosts: &[&str]) -> DeployProgress {
        let hosts = hosts.iter().map(|h| Hostname::from(*h)).collect();
        DeployProgress::new(hosts, Box::new(NoopObserver))
    }

    /// Run a deploy through run_deploy using in-memory connections.
    fn deploy_to(driver: &TestRepo, targets: &[&TestHost], plan: &Plan) -> Result<()> {
        let hostnames: Vec<_> = plan.hosts.keys().map(|h| h.0.as_str()).collect();
        let mut progress = test_progress(&hostnames);
        run_deploy(
            &driver.store,
            plan,
            "deckard@spinner",
            |host| {
                let target = targets
                    .iter()
                    .find(|t| t.session.hostname == *host)
                    .expect("host is in targets");
                Ok(target.connect())
            },
            |_| panic!("install not expected"),
            &mut progress,
        )
    }

    fn make_plan(commit: Oid, hosts: &[(&str, Option<Oid>)]) -> Plan {
        Plan {
            commit,
            hosts: hosts
                .iter()
                .map(|(name, expected_current)| {
                    (
                        Hostname::from(*name),
                        HostPlan {
                            apps: BTreeMap::new(),
                            expected_current: expected_current.clone(),
                            is_fast_forward: true,
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn lock_push_pack_and_apply_with_separate_driver_and_target() -> Result<()> {
        // Driver has the commit, target starts empty.
        let driver = TestRepo::new();
        let commit_oid = driver.commit(&[("web1/app/conf", b"hello")]);

        let target = TestHost::new("web1");
        let plan = make_plan(commit_oid, &[("web1", None)]);
        deploy_to(&driver, &[&target], &plan)?;

        Ok(())
    }

    #[test]
    fn fetch_resolves_stale_after_concurrent_deploy() -> Result<()> {
        let target = TestHost::new("web1");
        let driver_a = TestRepo::new();

        // Driver A deploys v1, pushes pack to target, applies.
        let commit_v1 = driver_a.commit(&[("web1/app/conf", b"v1")]);
        deploy_to(
            &driver_a,
            &[&target],
            &make_plan(commit_v1, &[("web1", None)]),
        )?;

        // Driver B: copy of A at this point (same main, same tracking ref).
        let driver_b = TestRepo::copy_from(&driver_a);

        // Driver A deploys v2.
        let commit_v2 = driver_a.commit(&[("web1/app/conf", b"v2")]);
        deploy_to(
            &driver_a,
            &[&target],
            &make_plan(commit_v2, &[("web1", Some(commit_v1))]),
        )?;

        // Driver B commits v3 (diverges from A's v2) and tries to deploy.
        // B's plan still thinks current is commit_v1, but target has commit_v2.
        let commit_v3 = driver_b.commit(&[("web1/app/conf", b"v3")]);
        let plan = make_plan(commit_v3, &[("web1", Some(commit_v1))]);
        let mut progress = test_progress(&["web1"]);
        let result = run_deploy(
            &driver_b.store,
            &plan,
            "deckard@spinner",
            |_| Ok(target.connect()),
            |_| panic!("install not expected"),
            &mut progress,
        );

        // Deploy should abort because the host is stale.
        assert!(result.is_err());
        assert_eq!(*progress.state("web1"), HostState::Stale);

        // The stale fetch should have brought commit_v2 into driver B,
        // so B can re-plan with up-to-date information.
        assert!(driver_b.store.repo.find_commit(commit_v2).is_ok());
        assert_eq!(driver_b.get_host_tracking_ref("web1"), Some(commit_v2));

        Ok(())
    }

    #[test]
    fn deploy_aborts_if_one_host_is_stale() -> Result<()> {
        let web1 = TestHost::new("web1");
        let web2 = TestHost::new("web2");
        let driver = TestRepo::new();

        // Deploy v1 to both hosts.
        let commit_v1 = driver.commit(&[("web1/app/conf", b"v1"), ("web2/app/conf", b"v1")]);
        deploy_to(
            &driver,
            &[&web1, &web2],
            &make_plan(commit_v1, &[("web1", None), ("web2", None)]),
        )?;

        // Another driver sneaks in and deploys v2 to web1 only.
        let other = TestRepo::copy_from(&driver);
        let commit_v2 = other.commit(&[("web1/app/conf", b"v2"), ("web2/app/conf", b"v1")]);
        deploy_to(
            &other,
            &[&web1],
            &make_plan(commit_v2, &[("web1", Some(commit_v1))]),
        )?;

        // Our driver tries to deploy v3 to both hosts. web1 is stale, web2 is fine.
        let commit_v3 = driver.commit(&[("web1/app/conf", b"v3"), ("web2/app/conf", b"v3")]);
        let plan = make_plan(
            commit_v3,
            &[("web1", Some(commit_v1)), ("web2", Some(commit_v1))],
        );
        let mut progress = test_progress(&["web1", "web2"]);
        let result = run_deploy(
            &driver.store,
            &plan,
            "deckard@spinner",
            |host| match host.0.as_str() {
                "web1" => Ok(web1.connect()),
                "web2" => Ok(web2.connect()),
                _ => panic!("unexpected host: {host:?}"),
            },
            |_| panic!("install not expected"),
            &mut progress,
        );

        // Deploy should abort: web1 is stale.
        assert!(result.is_err());
        assert_eq!(*progress.state("web1"), HostState::Stale);

        // web2 should NOT have been modified despite being lockable.
        assert_eq!(
            web2.get_current(),
            Some(commit_v1),
            "web2 should still be at v1"
        );

        Ok(())
    }

    #[test]
    fn deploy_aborts_if_host_is_busy() -> Result<()> {
        let target = TestHost::new("web1");
        let driver = TestRepo::new();
        let commit = driver.commit(&[("web1/app/conf", b"v1")]);

        // Simulate another driver holding the lock.
        let lock_path = target.session.store.get_lock_file_path();
        let lock_holder = std::fs::File::create(&lock_path).expect("lock file is created");
        assert!(
            crate::session::try_flock_exclusive(&lock_holder).expect("flock succeeds"),
            "lock is acquired",
        );

        let plan = make_plan(commit, &[("web1", None)]);
        let mut progress = test_progress(&["web1"]);
        let result = run_deploy(
            &driver.store,
            &plan,
            "deckard@spinner",
            |_| Ok(target.connect()),
            |_| panic!("install not expected"),
            &mut progress,
        );

        assert!(result.is_err());
        assert!(matches!(progress.state("web1"), HostState::LockBusy(_)));

        drop(lock_holder);
        Ok(())
    }

    #[test]
    fn deploy_aborts_if_one_host_fails_to_connect() -> Result<()> {
        let web1 = TestHost::new("web1");
        let driver = TestRepo::new();
        let commit = driver.commit(&[("web1/app/conf", b"v1"), ("web2/app/conf", b"v1")]);

        // web2 is in the plan but unreachable.
        let plan = make_plan(commit, &[("web1", None), ("web2", None)]);
        let mut progress = test_progress(&["web1", "web2"]);
        let result = run_deploy(
            &driver.store,
            &plan,
            "deckard@spinner",
            |host| match host.0.as_str() {
                "web1" => Ok(web1.connect()),
                other => Err(Error::ConnectionFailed(format!(
                    "ssh: connect to host {other}: Connection timed out",
                ))),
            },
            |_| panic!("install not expected"),
            &mut progress,
        );

        // Deploy aborts because web2 is unreachable.
        assert!(result.is_err());
        assert!(matches!(progress.state("web2"), HostState::Failed(_),));

        // web1 should NOT have been modified.
        assert_eq!(
            web1.get_current(),
            None,
            "web1 should not have been deployed to"
        );

        Ok(())
    }
}

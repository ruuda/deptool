//! Execute a deployment plan by driving remote agents.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use git2::Oid;

use parking_lot::Mutex;

use crate::error::{ApplyError, Error, HostError, Result};
use crate::plan::Plan;
use crate::prim::Hostname;
use crate::protocol::{self, Hello, Message, Request};
use crate::store::{RefUpdate, Store};

#[derive(Debug)]
pub enum HostState {
    Pending,
    Connecting,
    InstallingAgent,
    Connected,
    Locked,
    Pushing,
    Applying,
    RollingBack,
    Done,
    Stale,
    LockBusy(Option<String>),
    RolledBack(ApplyError),
    RollbackFailed {
        apply_error: ApplyError,
        rollback_error: ApplyError,
    },
    Failed(HostError),
}

impl HostState {
    pub fn is_failure(&self) -> bool {
        matches!(
            self,
            HostState::Stale
                | HostState::LockBusy(_)
                | HostState::RolledBack(_)
                | HostState::RollbackFailed { .. }
                | HostState::Failed(_)
        )
    }
}

impl std::fmt::Display for HostState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostState::Pending => f.write_str("pending"),
            HostState::Connecting => f.write_str("connecting"),
            HostState::InstallingAgent => f.write_str("installing agent"),
            HostState::Connected => f.write_str("connected"),
            HostState::Locked => f.write_str("locked"),
            HostState::Pushing => f.write_str("pushing"),
            HostState::Applying => f.write_str("applying"),
            HostState::RollingBack => f.write_str("rolling back"),
            HostState::Done => f.write_str("done"),
            HostState::Stale => f.write_str("stale"),
            HostState::LockBusy(Some(who)) => write!(f, "locked by {who}"),
            HostState::LockBusy(None) => f.write_str("locked by another deploy"),
            HostState::RolledBack(err) => write!(f, "rolled back after failure: {err}"),
            HostState::RollbackFailed {
                apply_error,
                rollback_error,
            } => write!(
                f,
                "failed: {apply_error}, rollback also failed: {rollback_error}"
            ),
            HostState::Failed(err) => write!(f, "failed: {err}"),
        }
    }
}

impl From<HostError> for HostState {
    fn from(err: HostError) -> Self {
        HostState::Failed(err)
    }
}

/// Receives deploy events for display or logging.
pub trait DeployObserver: Send {
    fn state_changed(&mut self, states: &BTreeMap<Hostname, HostState>);
    fn log_message(&mut self, states: &BTreeMap<Hostname, HostState>, host: &Hostname, text: &str);
}

/// Tracks per-host deploy state, notifying an observer on changes.
pub struct DeployProgress {
    inner: Mutex<ProgressInner>,
}

struct ProgressInner {
    states: BTreeMap<Hostname, HostState>,
    observer: Box<dyn DeployObserver>,
}

impl DeployProgress {
    pub fn new(hosts: Vec<Hostname>, observer: Box<dyn DeployObserver>) -> Self {
        let states = hosts.into_iter().map(|h| (h, HostState::Pending)).collect();
        Self {
            inner: Mutex::new(ProgressInner { states, observer }),
        }
    }

    pub fn update(&self, host: &Hostname, state: impl Into<HostState>) {
        let inner = &mut *self.inner.lock();
        *inner.states.get_mut(host).expect("host is in the plan") = state.into();
        inner.observer.state_changed(&inner.states);
    }

    pub fn log_message(&self, host: &Hostname, text: &str) {
        let inner = &mut *self.inner.lock();
        inner.observer.log_message(&inner.states, host, text);
    }

    pub fn num_failed(&self) -> usize {
        let states = &self.inner.lock().states;
        states.values().filter(|s| s.is_failure()).count()
    }

    #[cfg(test)]
    fn state(&self, host: &str) -> parking_lot::MappedMutexGuard<'_, HostState> {
        parking_lot::MutexGuard::map(self.inner.lock(), |inner| {
            inner
                .states
                .get_mut(&Hostname(host.into()))
                .expect("host is in the plan")
        })
    }
}

pub trait Connection: Send {
    fn hello(&self) -> &Hello;
    fn send_request(&mut self, request: &Request) -> std::result::Result<(), HostError>;
    fn read_message(&mut self) -> std::result::Result<Option<Message>, HostError>;
    /// Close stdin to signal no more requests are coming.
    fn close(&mut self);
}

/// Driver-side handle for talking to a remote agent over SSH.
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
    pub fn new(mut cmd: Command) -> std::result::Result<Self, HostError> {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(HostError::connection_failed)?;

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
                let code = child.wait().map_err(HostError::connection_failed)?.code();
                match code {
                    // 255: SSH connection failure (host unreachable, DNS
                    // failure, connection refused, timeout, etc.)
                    Some(255) => {
                        return Err(HostError::ConnectionFailed(stderr.to_string()));
                    }
                    // When we run `deptool` directly and the shell reports that
                    // the binary is not found, the exit code is 127, but when
                    // we run it through sudo, then sudo fails and exits with
                    // code 1. TODO: Would it be better to start the agent as
                    // the current user and the let it reexec itself under sudo
                    // if its uid is unexpected?
                    Some(1 | 127) => return Err(HostError::AgentNotInstalled),
                    Some(code) => {
                        return Err(HostError::ProtocolError(format!(
                            "ssh exited with status {code} \
                             before agent sent hello: {stderr}"
                        )));
                    }
                    // On Unix, None means the process was killed by a signal.
                    None => {
                        return Err(HostError::ProtocolError(format!(
                            "ssh killed by signal \
                             before agent sent hello: {stderr}"
                        )));
                    }
                }
            }
            Ok(_) => serde_json::from_str(&line)
                .map_err(|e| HostError::ProtocolError(format!("invalid hello: {e}")))?,
            Err(e) => return Err(HostError::connection_failed(e)),
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

    fn send_request(&mut self, request: &Request) -> std::result::Result<(), HostError> {
        let writer = self.writer.as_mut().expect("stdin is still open");
        serde_json::to_writer(&mut *writer, request).map_err(HostError::protocol_error)?;
        writeln!(writer).map_err(HostError::protocol_error)?;
        writer.flush().map_err(HostError::protocol_error)?;
        Ok(())
    }

    fn read_message(&mut self) -> std::result::Result<Option<Message>, HostError> {
        let mut line = String::new();
        let n = self
            .reader
            .read_line(&mut line)
            .map_err(HostError::protocol_error)?;
        if n == 0 {
            return Ok(None);
        }
        let message: Message = serde_json::from_str(&line).map_err(HostError::protocol_error)?;
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
    pub locked: BTreeMap<Hostname, Box<dyn Connection>>,
    pub stale: BTreeMap<Hostname, StaleHost>,
}

/// Connect to a host, installing the agent if needed.
fn try_connect(
    host: &Hostname,
    connect: &(impl Fn(&Hostname) -> std::result::Result<Box<dyn Connection>, HostError> + Sync),
    install: &(impl Fn(&Hostname) -> std::result::Result<(), HostError> + Sync),
    progress: &DeployProgress,
) -> Option<Box<dyn Connection>> {
    progress.update(host, HostState::Connecting);
    let conn = match connect(host) {
        Ok(c) => c,
        Err(HostError::AgentNotInstalled) => {
            progress.update(host, HostState::InstallingAgent);
            if let Err(err) = install(host) {
                progress.update(host, err);
                return None;
            }
            match connect(host) {
                Ok(c) => c,
                Err(err) => {
                    progress.update(host, err);
                    return None;
                }
            }
        }
        Err(err) => {
            progress.update(host, err);
            return None;
        }
    };
    assert_eq!(
        conn.hello().version,
        protocol::VERSION,
        "agent version matches operator version"
    );
    if conn.hello().hostname != host.0 {
        progress.update(
            host,
            HostError::HostnameMismatch(conn.hello().hostname.clone()),
        );
        return None;
    }
    progress.update(host, HostState::Connected);
    Some(conn)
}

/// Connect to all hosts and lock them, in parallel.
///
/// Each host connects and immediately locks without waiting for other
/// hosts. Deadlock is not possible because agents use non-blocking
/// try-locks (returning `LockBusy` immediately if held). Tries every
/// host even if some fail, so the caller gets all stale info in one pass.
pub fn connect_and_lock(
    plan: &Plan,
    operator: &str,
    connect: &(impl Fn(&Hostname) -> std::result::Result<Box<dyn Connection>, HostError> + Sync),
    install: &(impl Fn(&Hostname) -> std::result::Result<(), HostError> + Sync),
    progress: &DeployProgress,
) -> LockResult {
    let result = Mutex::new(LockResult {
        locked: BTreeMap::new(),
        stale: BTreeMap::new(),
    });

    std::thread::scope(|s| {
        for (host, host_plan) in &plan.hosts {
            let lock_request = Request::Lock {
                expected_current_commit: host_plan.expected_current,
                operator: operator.to_string(),
            };
            let result = &result;
            s.spawn(move || {
                let mut conn = match try_connect(host, connect, install, progress) {
                    Some(c) => c,
                    None => return,
                };
                let msg = match conn.send_request(&lock_request) {
                    Ok(()) => conn.read_message(),
                    Err(err) => Err(err),
                };
                match msg {
                    Ok(Some(Message::Locked)) => {
                        progress.update(host, HostState::Locked);
                        result.lock().locked.insert(host.clone(), conn);
                    }
                    Ok(Some(Message::LockStale {
                        expected_commit,
                        actual_commit,
                    })) => {
                        progress.update(host, HostState::Stale);
                        result.lock().stale.insert(
                            host.clone(),
                            StaleHost {
                                expected_commit,
                                actual_commit,
                                connection: conn,
                            },
                        );
                    }
                    Ok(Some(Message::LockBusy { held_by })) => {
                        progress.update(host, HostState::LockBusy(held_by));
                    }
                    Ok(other) => {
                        progress.update(
                            host,
                            HostError::ProtocolError(format!(
                                "unexpected lock response: {other:?}"
                            )),
                        );
                    }
                    Err(err) => {
                        progress.update(host, err);
                    }
                }
            });
        }
    });

    result.into_inner()
}

/// Push pack and apply on a single host.
fn push_and_apply_host(
    store_path: &Path,
    host: &Hostname,
    conn: &mut Box<dyn Connection>,
    plan: &Plan,
    packs: &BTreeMap<Option<Oid>, String>,
    progress: &DeployProgress,
) -> std::result::Result<(), HostError> {
    progress.update(host, HostState::Pushing);
    let key = plan.hosts[host].expected_current;
    let encoded = &packs[&key];
    conn.send_request(&Request::ReceivePack {
        pack_data: encoded.clone(),
    })?;
    match conn.read_message()? {
        Some(Message::PackReceived) => {}
        Some(Message::Error(apply_err)) => {
            return Err(HostError::Apply(apply_err));
        }
        other => {
            return Err(HostError::ProtocolError(format!(
                "unexpected response to ReceivePack: {other:?}"
            )));
        }
    }

    progress.update(host, HostState::Applying);
    conn.send_request(&Request::Apply {
        target_commit: plan.commit,
        is_rollback_safe: plan.hosts[host].is_rollback_safe,
    })?;
    conn.close();
    let mut applied_commit = None;
    while let Some(message) = conn.read_message()? {
        match &message {
            Message::ApplyComplete { commit, .. } => {
                applied_commit = Some(*commit);
            }
            Message::RollingBack => {
                progress.update(host, HostState::RollingBack);
            }
            Message::RolledBack { error } => {
                progress.update(host, HostState::RolledBack(error.clone()));
                return Ok(());
            }
            Message::RollbackFailed {
                apply_error,
                rollback_error,
            } => {
                progress.update(
                    host,
                    HostState::RollbackFailed {
                        apply_error: apply_error.clone(),
                        rollback_error: rollback_error.clone(),
                    },
                );
                return Ok(());
            }
            Message::SystemdUnitStatus { output } => {
                let filtered = filter_systemd_status(output.trim_end());
                progress.log_message(host, &filtered);
            }
            Message::Error(apply_err) => {
                return Err(HostError::Apply(apply_err.clone()));
            }
            _ => {}
        }
    }
    let applied_commit = applied_commit.ok_or_else(|| {
        HostError::ProtocolError("agent closed without ApplyComplete".to_string())
    })?;
    assert_eq!(
        applied_commit, plan.commit,
        "applied commit matches plan commit"
    );

    let store = Store::open(store_path)?;
    store.set_ref(
        &format!("refs/remotes/{host}/current"),
        applied_commit,
        RefUpdate::ApplyComplete,
    )?;

    progress.update(host, HostState::Done);
    Ok(())
}

/// Pre-compute deduplicated packfiles for all hosts in the plan.
///
/// Multiple hosts with the same `expected_current` need the same pack, so
/// we build each unique pack once and base64-encode it for sending.
pub fn build_packs(store: &Store, plan: &Plan) -> Result<BTreeMap<Option<Oid>, String>> {
    let mut packs = BTreeMap::new();
    for host_plan in plan.hosts.values() {
        let key = host_plan.expected_current;
        if !packs.contains_key(&key) {
            let bytes = store.create_pack(plan.commit, key)?;
            packs.insert(key, BASE64.encode(&bytes));
        }
    }
    Ok(packs)
}

/// Fetch objects from stale hosts over their still-open sessions.
///
/// For each stale host whose actual commit we don't already have, sends
/// `RequestObjects` and receives a packfile. Updates the local tracking
/// ref for each host. Reports per-host errors via progress.
pub fn fetch_stale_objects(
    store: &Store,
    stale: &mut BTreeMap<Hostname, StaleHost>,
    progress: &DeployProgress,
) {
    for (host, info) in stale.iter_mut() {
        match fetch_from_stale_host(store, host, info) {
            Ok(()) => {}
            Err(err) => progress.update(host, err),
        }
    }
}

fn fetch_from_stale_host(
    store: &Store,
    host: &Hostname,
    info: &mut StaleHost,
) -> std::result::Result<(), HostError> {
    let actual_commit = match info.actual_commit {
        Some(c) => c,
        None => return Ok(()),
    };

    // Fetch the pack if we don't already have this commit.
    if store.repo.find_commit(actual_commit).is_err() {
        info.connection.send_request(&Request::RequestObjects {
            have_commit: info.expected_commit,
        })?;

        match info.connection.read_message()? {
            Some(Message::SendPack { pack_data }) => {
                let bytes = BASE64
                    .decode(&pack_data)
                    .expect("SendPack contains valid base64");
                store.write_pack(&bytes)?;
            }
            Some(Message::Error(apply_err)) => {
                return Err(HostError::Apply(apply_err));
            }
            other => {
                return Err(HostError::ProtocolError(format!(
                    "unexpected response to RequestObjects: {other:?}"
                )));
            }
        }
    }

    store.set_ref(
        &format!("refs/remotes/{host}/current"),
        actual_commit,
        RefUpdate::FetchStale,
    )?;

    Ok(())
}

/// Run a full deployment: connect+lock all hosts, push packs, and apply.
///
/// Connect and lock run together per-host in parallel threads. Push+apply
/// also runs in parallel after all locks succeed. If any host fails to
/// lock (stale, busy, or connection error), fetches objects from stale
/// hosts and aborts without pushing or applying to any host.
pub fn run_deploy(
    store: &Store,
    plan: &Plan,
    operator: &str,
    connect: impl Fn(&Hostname) -> std::result::Result<Box<dyn Connection>, HostError> + Sync,
    install: impl Fn(&Hostname) -> std::result::Result<(), HostError> + Sync,
    progress: &DeployProgress,
) -> Result<()> {
    let mut lock_result = connect_and_lock(plan, operator, &connect, &install, progress);

    if lock_result.locked.len() < plan.hosts.len() {
        // Fetch objects from stale hosts so we have the data for the next plan.
        fetch_stale_objects(store, &mut lock_result.stale, progress);
        let n = progress.num_failed();
        return Err(Error::DeployFailed(format!(
            "failed to lock {n} host(s), aborting",
        )));
    }

    let packs = build_packs(store, plan)?;
    let store_path = store.path();

    std::thread::scope(|s| {
        let packs = &packs;
        for (host, mut conn) in lock_result.locked {
            s.spawn(move || {
                if let Err(err) =
                    push_and_apply_host(store_path, &host, &mut conn, plan, packs, progress)
                {
                    progress.update(&host, err);
                }
            });
        }
    });

    match progress.num_failed() {
        0 => Ok(()),
        n => Err(Error::DeployFailed(format!(
            "encountered errors on {n} host(s)"
        ))),
    }
}

/// Strip noise from `systemctl status` output.
///
/// The full output includes CGroup trees, memory stats, CPU time, etc.
/// that bury the signal (is it running? why did it fail?) in noise. We
/// keep only the header, Loaded, Active, Process, Main PID, and journal
/// log lines.
fn filter_systemd_status(output: &str) -> String {
    const DROP_PREFIXES: &[&str] = &[
        " Invocation:",
        "TriggeredBy:",
        "   Duration:",
        "      Tasks:",
        "     Memory:",
        "   Mem peak:",
        "        CPU:",
        "     CGroup:",
        "    Drop-In:",
    ];

    output
        .lines()
        .filter(|line| {
            // Drop CGroup tree and Drop-In continuation lines.
            if line.contains('├') || line.contains('└') {
                return false;
            }
            !DROP_PREFIXES.iter().any(|p| line.starts_with(p))
        })
        .collect::<Vec<_>>()
        .join("\n")
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
        let progress = test_progress(&hostnames);
        // Pre-build Sync-safe connection factories (TestHost is not Sync
        // because git2::Repository is not Sync).
        let connectors: Vec<_> = targets
            .iter()
            .map(|t| (t.session.hostname.clone(), t.connector()))
            .collect();
        run_deploy(
            &driver.store,
            plan,
            "deckard@spinner",
            |host| {
                let (_, connector) = connectors
                    .iter()
                    .find(|(h, _)| h == host)
                    .expect("host is in targets");
                Ok(connector())
            },
            |_| panic!("install not expected"),
            &progress,
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
                            is_rollback_safe: true,
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
        let progress = test_progress(&["web1"]);
        let connector = target.connector();
        let result = run_deploy(
            &driver_b.store,
            &plan,
            "deckard@spinner",
            |_| Ok(connector()),
            |_| panic!("install not expected"),
            &progress,
        );

        // Deploy should abort because the host is stale.
        assert!(result.is_err());
        assert!(matches!(*progress.state("web1"), HostState::Stale));

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
        let progress = test_progress(&["web1", "web2"]);
        let c1 = web1.connector();
        let c2 = web2.connector();
        let result = run_deploy(
            &driver.store,
            &plan,
            "deckard@spinner",
            |host| match host.0.as_str() {
                "web1" => Ok(c1()),
                "web2" => Ok(c2()),
                _ => panic!("unexpected host: {host:?}"),
            },
            |_| panic!("install not expected"),
            &progress,
        );

        // Deploy should abort: web1 is stale.
        assert!(result.is_err());
        assert!(matches!(*progress.state("web1"), HostState::Stale));

        // web2 should NOT have been modified despite being lockable.
        assert_eq!(
            web2.get_ref("refs/heads/current"),
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
            crate::agent::try_flock_exclusive(&lock_holder).expect("flock succeeds"),
            "lock is acquired",
        );

        let plan = make_plan(commit, &[("web1", None)]);
        let progress = test_progress(&["web1"]);
        let connector = target.connector();
        let result = run_deploy(
            &driver.store,
            &plan,
            "deckard@spinner",
            |_| Ok(connector()),
            |_| panic!("install not expected"),
            &progress,
        );

        assert!(result.is_err());
        assert!(matches!(*progress.state("web1"), HostState::LockBusy(_)));

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
        let progress = test_progress(&["web1", "web2"]);
        let c1 = web1.connector();
        let result = run_deploy(
            &driver.store,
            &plan,
            "deckard@spinner",
            |host| match host.0.as_str() {
                "web1" => Ok(c1()),
                other => Err(HostError::ConnectionFailed(format!(
                    "ssh: connect to host {other}: Connection timed out",
                ))),
            },
            |_| panic!("install not expected"),
            &progress,
        );

        // Deploy aborts because web2 is unreachable.
        assert!(result.is_err());
        assert!(matches!(*progress.state("web2"), HostState::Failed(_)));

        // web1 should NOT have been modified.
        assert_eq!(
            web1.get_ref("refs/heads/current"),
            None,
            "web1 should not have been deployed to"
        );

        Ok(())
    }

    #[test]
    fn deploy_fails_on_hostname_mismatch() {
        struct Conn(Hello);
        impl Connection for Conn {
            fn hello(&self) -> &Hello {
                &self.0
            }
            fn send_request(&mut self, _: &Request) -> std::result::Result<(), HostError> {
                unimplemented!()
            }
            fn read_message(&mut self) -> std::result::Result<Option<Message>, HostError> {
                unimplemented!()
            }
            fn close(&mut self) {}
        }

        let progress = test_progress(&["web1"]);
        let host = Hostname::from("web1");
        let result = try_connect(
            &host,
            &|_| {
                Ok(Box::new(Conn(Hello {
                    version: protocol::VERSION.to_string(),
                    hostname: "spinner".to_string(),
                })))
            },
            &|_| panic!("install not expected"),
            &progress,
        );

        assert!(result.is_none());
        assert!(matches!(
            &*progress.state("web1"),
            HostState::Failed(HostError::HostnameMismatch(h)) if h == "spinner"
        ));
    }

    #[test]
    fn push_and_apply_reports_failure_on_bad_pack() -> Result<()> {
        let driver = TestRepo::new();
        let commit = driver.commit(&[("web1/app/conf", b"v1")]);
        let plan = make_plan(commit, &[("web1", None)]);

        let target = TestHost::new("web1");
        let mut conn = target.connect();
        conn.send_request(&Request::Lock {
            expected_current_commit: None,
            operator: "deckard@spinner".into(),
        })?;
        assert_eq!(conn.read_message()?, Some(Message::Locked));

        // Send a corrupt pack so the agent fails during ReceivePack.
        let packs = BTreeMap::from([(None, BASE64.encode(b"not a valid pack"))]);
        let progress = test_progress(&["web1"]);
        let host = Hostname::from("web1");
        let store_path = driver.store.path();
        let result = push_and_apply_host(store_path, &host, &mut conn, &plan, &packs, &progress);

        assert!(result.is_err(), "push should fail on bad pack");
        Ok(())
    }

    #[test]
    fn filter_systemd_status_failed_unit() {
        let input = "\
× nginx.service - The NGINX HTTP and reverse proxy server
     Loaded: loaded (/etc/systemd/system/nginx.service; enabled; preset: disabled)
     Active: failed (Result: exit-code) since Thu 2026-04-16 18:56:59 UTC; 324ms ago
   Duration: 20h 53min 33.037s
 Invocation: 25802412ac844cd0b033e563b28a5881
    Process: 77498 ExecStartPre=/usr/bin/nginx -T (code=exited, status=1/FAILURE)
   Mem peak: 10.2M
        CPU: 31ms

Apr 16 18:56:59 host systemd[1]: Starting nginx.service...
Apr 16 18:56:59 host nginx[77498]: [emerg] unknown directive \"intentional_error\"
Apr 16 18:56:59 host systemd[1]: Failed to start nginx.service.";

        assert_eq!(
            filter_systemd_status(input),
            "\
× nginx.service - The NGINX HTTP and reverse proxy server
     Loaded: loaded (/etc/systemd/system/nginx.service; enabled; preset: disabled)
     Active: failed (Result: exit-code) since Thu 2026-04-16 18:56:59 UTC; 324ms ago
    Process: 77498 ExecStartPre=/usr/bin/nginx -T (code=exited, status=1/FAILURE)

Apr 16 18:56:59 host systemd[1]: Starting nginx.service...
Apr 16 18:56:59 host nginx[77498]: [emerg] unknown directive \"intentional_error\"
Apr 16 18:56:59 host systemd[1]: Failed to start nginx.service.",
        );
    }

    #[test]
    fn filter_systemd_status_running_unit() {
        let input = "\
● nginx.service - The NGINX HTTP and reverse proxy server
     Loaded: loaded (/etc/systemd/system/nginx.service; enabled; preset: disabled)
     Active: active (running) since Thu 2026-04-16 18:57:00 UTC; 313ms ago
 Invocation: e88314b8d6584b7a93d0aed6522e5af3
    Process: 77587 ExecStartPre=/usr/bin/nginx -T (code=exited, status=0/SUCCESS)
   Main PID: 77595 (nginx)
      Tasks: 2 (limit: 2912)
     Memory: 11.1M (peak: 11.8M)
        CPU: 73ms
     CGroup: /system.slice/nginx.service
             ├─77595 \"nginx: master process /usr/bin/nginx\"
             └─77601 \"nginx: worker process\"";

        assert_eq!(
            filter_systemd_status(input),
            "\
● nginx.service - The NGINX HTTP and reverse proxy server
     Loaded: loaded (/etc/systemd/system/nginx.service; enabled; preset: disabled)
     Active: active (running) since Thu 2026-04-16 18:57:00 UTC; 313ms ago
    Process: 77587 ExecStartPre=/usr/bin/nginx -T (code=exited, status=0/SUCCESS)
   Main PID: 77595 (nginx)",
        );
    }
}

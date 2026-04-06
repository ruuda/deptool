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
    InstallingAgent,
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
            HostState::InstallingAgent => f.write_str("installing agent"),
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
            // Suppress stderr so "command not found" on first connect
            // (before agent installation) doesn't disturb the status display.
            .stderr(Stdio::null())
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
    repo: &git2::Repository,
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
            if let Message::ApplyComplete { ref commit, .. } = message {
                progress.update(host, HostState::Done);
                crate::store::set_ref(
                    repo,
                    &format!("refs/remotes/{host}/current"),
                    git2::Oid::from(commit),
                    crate::store::RefUpdate::ApplyComplete,
                )?;
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

/// Run a full deployment: lock all hosts, push packs, and apply.
///
/// If any host fails to lock (stale, busy, or connection error), fetches
/// objects from stale hosts and aborts without pushing or applying to any
/// host.
pub fn run_deploy(
    repo: &git2::Repository,
    plan: &Plan,
    connect: impl FnMut(&Hostname) -> Result<Box<dyn Connection>>,
    install: impl FnMut(&Hostname) -> Result<()>,
    progress: &mut DeployProgress,
) -> Result<()> {
    let mut lock_result = lock_hosts(plan, connect, install, progress);

    if progress.has_failures() {
        // Fetch objects from stale hosts so we have the data for the next plan.
        fetch_stale_objects(repo, &mut lock_result.stale)?;
        let n = progress.num_failed();
        return Err(Error::InvalidConfig(format!(
            "failed to lock {n} host(s), aborting",
        )));
    }

    push_packs(repo, plan, &mut lock_result.locked, progress)?;
    apply_hosts(repo, plan, &mut lock_result.locked, progress, |_, _| {})?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::plan::HostPlan;
    use crate::prim::Oid;
    use crate::testutil::{TestHost, TestRepo};

    fn test_progress(hosts: &[&str]) -> DeployProgress {
        let hosts = hosts.iter().map(|h| Hostname::from(*h)).collect();
        DeployProgress::new(hosts, Box::new(|_| {}))
    }

    /// Run a deploy through run_deploy using in-memory connections.
    fn deploy_to(
        driver: &TestRepo,
        targets: &BTreeMap<Hostname, &TestHost>,
        plan: &Plan,
    ) -> Result<()> {
        let mut progress = test_progress(
            &targets.keys().map(|h| h.0.as_str()).collect::<Vec<_>>(),
        );
        run_deploy(
            &driver.repo,
            plan,
            |host| Ok(targets[host].connect()),
            |_| panic!("install not expected"),
            &mut progress,
        )
    }

    fn make_plan(
        commit: Oid,
        hosts: &[(&str, Option<Oid>)],
    ) -> Plan {
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
        let targets = BTreeMap::from([(Hostname::from("web1"), &target)]);
        let plan = make_plan(commit_oid.into(), &[("web1", None)]);
        deploy_to(&driver, &targets, &plan)?;

        Ok(())
    }

    #[test]
    fn fetch_resolves_stale_after_concurrent_deploy() -> Result<()> {
        let target = TestHost::new("web1");
        let targets = BTreeMap::from([(Hostname::from("web1"), &target)]);
        let driver_a = TestRepo::new();

        // Driver A deploys v1, pushes pack to target, applies.
        let commit_v1 = driver_a.commit(&[("web1/app/conf", b"v1")]);
        deploy_to(
            &driver_a,
            &targets,
            &make_plan(commit_v1.into(), &[("web1", None)]),
        )?;

        // Driver B: copy of A at this point (same main, same tracking ref).
        let driver_b = TestRepo::copy_from(&driver_a);

        // Driver A deploys v2.
        let commit_v2 = driver_a.commit(&[("web1/app/conf", b"v2")]);
        deploy_to(
            &driver_a,
            &targets,
            &make_plan(commit_v2.into(), &[("web1", Some(commit_v1.into()))]),
        )?;

        // Driver B commits v3 (diverges from A's v2) and tries to lock.
        // B's plan still thinks current is commit_v1, but target has commit_v2.
        let commit_v3 = driver_b.commit(&[("web1/app/conf", b"v3")]);
        let plan = make_plan(
            commit_v3.into(),
            &[("web1", Some(commit_v1.into()))],
        );
        let mut progress = test_progress(&["web1"]);
        let result = run_deploy(
            &driver_b.repo,
            &plan,
            |_| Ok(target.connect()),
            |_| panic!("install not expected"),
            &mut progress,
        );

        // Deploy should abort because the host is stale.
        assert!(result.is_err());

        // But the stale fetch should have brought commit_v2 into driver B.
        assert!(driver_b.repo.find_commit(commit_v2).is_ok());
        let tracking_ref = driver_b
            .repo
            .find_reference("refs/remotes/web1/current")?
            .peel_to_commit()?;
        assert_eq!(tracking_ref.id(), commit_v2);

        Ok(())
    }

    #[test]
    fn deploy_aborts_if_one_host_is_stale() -> Result<()> {
        let web1 = TestHost::new("web1");
        let web2 = TestHost::new("web2");
        let driver = TestRepo::new();

        // Deploy v1 to both hosts.
        let commit_v1 = driver.commit(&[("web1/app/conf", b"v1"), ("web2/app/conf", b"v1")]);
        let targets = BTreeMap::from([
            (Hostname::from("web1"), &web1),
            (Hostname::from("web2"), &web2),
        ]);
        deploy_to(
            &driver,
            &targets,
            &make_plan(commit_v1.into(), &[("web1", None), ("web2", None)]),
        )?;

        // Another driver sneaks in and deploys v2 to web1 only.
        let other = TestRepo::copy_from(&driver);
        let commit_v2 = other.commit(&[("web1/app/conf", b"v2"), ("web2/app/conf", b"v1")]);
        let web1_only = BTreeMap::from([(Hostname::from("web1"), &web1)]);
        deploy_to(
            &other,
            &web1_only,
            &make_plan(commit_v2.into(), &[("web1", Some(commit_v1.into()))]),
        )?;

        // Our driver tries to deploy v3 to both hosts. web1 is stale, web2 is fine.
        let commit_v3 = driver.commit(&[("web1/app/conf", b"v3"), ("web2/app/conf", b"v3")]);
        let plan = make_plan(
            commit_v3.into(),
            &[
                ("web1", Some(commit_v1.into())),
                ("web2", Some(commit_v1.into())),
            ],
        );
        let mut progress = test_progress(&["web1", "web2"]);
        let result = run_deploy(
            &driver.repo,
            &plan,
            |host| Ok(targets[host].connect()),
            |_| panic!("install not expected"),
            &mut progress,
        );

        // Deploy should abort: web1 is stale.
        assert!(result.is_err());

        // web2 should NOT have been modified despite being lockable.
        let web2_current = web2
            .session
            .repo
            .find_reference("refs/heads/current")
            .expect("web2 has a current ref")
            .peel_to_commit()
            .expect("ref points to a commit")
            .id();
        assert_eq!(web2_current, commit_v1, "web2 should still be at v1");

        Ok(())
    }

    #[test]
    fn deploy_aborts_if_host_is_busy() -> Result<()> {
        let target = TestHost::new("web1");
        let driver = TestRepo::new();
        let commit = driver.commit(&[("web1/app/conf", b"v1")]);

        // Simulate another driver holding the lock.
        let lock_path = target.session.repo.path().join("deptool.lock");
        let lock_holder = std::fs::File::create(&lock_path).expect("lock file is created");
        assert!(
            crate::session::try_flock_exclusive(&lock_holder).expect("flock succeeds"),
            "lock is acquired",
        );

        let plan = make_plan(commit.into(), &[("web1", None)]);
        let mut progress = test_progress(&["web1"]);
        let result = run_deploy(
            &driver.repo,
            &plan,
            |_| Ok(target.connect()),
            |_| panic!("install not expected"),
            &mut progress,
        );

        assert!(result.is_err());

        drop(lock_holder);
        Ok(())
    }

    #[test]
    fn deploy_aborts_if_one_host_fails_to_connect() -> Result<()> {
        let web1 = TestHost::new("web1");
        let driver = TestRepo::new();
        let commit = driver.commit(&[("web1/app/conf", b"v1"), ("web2/app/conf", b"v1")]);

        let plan = make_plan(
            commit.into(),
            &[("web1", None), ("web2", None)],
        );
        let mut progress = test_progress(&["web1", "web2"]);
        let result = run_deploy(
            &driver.repo,
            &plan,
            |host| match host.0.as_str() {
                "web1" => Ok(web1.connect()),
                _ => Err(Error::AgentNotInstalled),
            },
            |_| Err(Error::InvalidConfig("install failed".into())),
            &mut progress,
        );

        // Deploy aborts because web2 failed.
        assert!(result.is_err());

        // web1 should NOT have been modified.
        assert!(
            web1.session.repo.find_reference("refs/heads/current").is_err(),
            "web1 should not have been deployed to",
        );

        Ok(())
    }

    // TODO: Add an integration test that spawns the real deptool binary in
    // local mode and exercises the full stdin/stdout protocol roundtrip.
}

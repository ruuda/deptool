//! Execute a deployment plan by driving remote host sessions.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use crate::error::{Error, Result};
use crate::plan::Plan;
use crate::prim::{Hostname, Oid};
use crate::protocol::{self, Hello, Message, Request};

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

pub enum LockFailure {
    Stale {
        expected_commit: Option<Oid>,
        actual_commit: Option<Oid>,
    },
    Busy,
    VersionMismatch {
        remote_version: String,
    },
    ConnectionFailed(String),
}

pub struct LockResult {
    pub locked: Vec<(Hostname, Box<dyn Connection>)>,
    pub failures: Vec<(Hostname, LockFailure)>,
}

/// Open sessions and acquire deploy locks on all hosts in the plan.
///
/// Tries every host even if some fail, so the caller gets all stale info
/// in one pass. Hosts are locked in plan iteration order (asciibetical,
/// since the plan uses a BTreeMap).
pub fn lock_hosts(
    plan: &Plan,
    mut connect: impl FnMut(&Hostname) -> Result<Box<dyn Connection>>,
) -> LockResult {
    let mut result = LockResult {
        locked: Vec::new(),
        failures: Vec::new(),
    };

    for (host, host_plan) in &plan.hosts {
        let mut conn = match connect(host) {
            Ok(c) => c,
            Err(err) => {
                result
                    .failures
                    .push((host.clone(), LockFailure::ConnectionFailed(err.to_string())));
                continue;
            }
        };

        if conn.hello().version != protocol::VERSION {
            result.failures.push((
                host.clone(),
                LockFailure::VersionMismatch {
                    remote_version: conn.hello().version.clone(),
                },
            ));
            continue;
        }

        let lock_request = Request::Lock {
            expected_current_commit: host_plan.expected_current.clone(),
        };
        if let Err(err) = conn.send_request(&lock_request) {
            result
                .failures
                .push((host.clone(), LockFailure::ConnectionFailed(err.to_string())));
            continue;
        }

        match conn.read_message() {
            Ok(Some(Message::Locked)) => {
                result.locked.push((host.clone(), conn));
            }
            Ok(Some(Message::LockStale {
                expected_commit,
                actual_commit,
            })) => {
                result.failures.push((
                    host.clone(),
                    LockFailure::Stale {
                        expected_commit,
                        actual_commit,
                    },
                ));
            }
            Ok(Some(Message::LockBusy)) => {
                result.failures.push((host.clone(), LockFailure::Busy));
            }
            Ok(Some(other)) => {
                result.failures.push((
                    host.clone(),
                    LockFailure::ConnectionFailed(format!("unexpected lock response: {other:?}")),
                ));
            }
            Ok(None) => {
                result.failures.push((
                    host.clone(),
                    LockFailure::ConnectionFailed("agent closed connection during lock".into()),
                ));
            }
            Err(err) => {
                result
                    .failures
                    .push((host.clone(), LockFailure::ConnectionFailed(err.to_string())));
            }
        }
    }

    result
}

/// Send Apply to all locked hosts and stream responses.
pub fn apply_hosts(
    plan: &Plan,
    connections: &mut Vec<(Hostname, Box<dyn Connection>)>,
    mut on_message: impl FnMut(&Hostname, Message),
) -> Result<()> {
    for (host, conn) in connections.iter_mut() {
        let host_plan = &plan.hosts[host];
        let request = Request::Apply {
            target_commit: plan.commit.clone(),
            expected_current_commit: host_plan.expected_current.clone(),
        };
        conn.send_request(&request)?;
        conn.close();
        while let Some(message) = conn.read_message()? {
            on_message(host, message);
        }
    }
    Ok(())
}

/// Push a commit to a remote store so its objects are available there.
///
/// Uses `git push` with the given URL. The commit is pushed to
/// `refs/heads/main` on the remote — a ref the agent doesn't use, it's
/// just a way to transfer the objects.
// TODO: In the future, detect in the planning phase when the remote is
// ahead (push would fail because remote main is not an ancestor).
pub fn push_to_host(
    store_path: &Path,
    remote_url: &str,
    commit: &Oid,
    receive_pack: Option<&str>,
) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.arg("--git-dir").arg(store_path);
    cmd.arg("push");
    if let Some(rp) = receive_pack {
        cmd.arg(format!("--receive-pack={rp}"));
    }
    cmd.arg(remote_url);
    cmd.arg(format!("{}:refs/heads/main", commit));

    let output = cmd.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::GitPush {
            remote_url: remote_url.to_string(),
            message: stderr.trim().to_string(),
        });
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
        test_host_with_version(protocol::VERSION)
    }

    fn test_host_with_version(protocol_version: &str) -> Result<TestHost> {
        let store = TempDir::new("store");
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        let repo = git2::Repository::init_bare(store.path())?;
        let commit_oid = commit_files(&repo, &[("web1/nginx/nginx.conf", b"v1")])?;
        // In tests, skip the daemon-reload + restart step.
        let on_units_changed = Box::new(|_: &_| Ok(()));
        let session = HostSession::new(
            repo,
            "web1".into(),
            apps.path().to_path_buf(),
            units.path().to_path_buf(),
            on_units_changed,
        );
        let hello = Hello {
            version: protocol_version.to_string(),
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
        let lock_result =
            lock_hosts(&plan, |_| Ok(conn.take().expect("connect called once")));

        assert!(lock_result.failures.is_empty());
        assert_eq!(lock_result.locked.len(), 1);

        let mut messages = Vec::new();
        apply_hosts(&plan, &mut lock_result.locked.into_iter().collect(), |_, msg| {
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
        let lock_result =
            lock_hosts(&plan, |_| Ok(conn.take().expect("connect called once")));

        assert!(lock_result.locked.is_empty());
        assert_eq!(lock_result.failures.len(), 1);
        assert!(matches!(
            &lock_result.failures[0].1,
            LockFailure::Stale { actual_commit: Some(_), .. }
        ));
        Ok(())
    }

    #[test]
    fn lock_reports_version_mismatch() -> Result<()> {
        let host = test_host_with_version("0.0.0-fake")?;
        let plan = single_host_plan(host.commit_oid.into());
        let mut conn = Some(host.conn);
        let lock_result =
            lock_hosts(&plan, |_| Ok(conn.take().expect("connect called once")));

        assert!(lock_result.locked.is_empty());
        assert_eq!(lock_result.failures.len(), 1);
        assert!(matches!(
            &lock_result.failures[0].1,
            LockFailure::VersionMismatch { .. }
        ));
        Ok(())
    }

    // TODO: Add an integration test that spawns the real deptool binary in
    // local mode and exercises the full stdin/stdout protocol roundtrip.
}

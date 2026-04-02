//! Execute a deployment plan by driving remote host sessions.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use crate::error::{Error, Result};
use crate::plan::Plan;
use crate::prim::Hostname;
use crate::protocol::{self, Hello, Message, Request};

pub trait Connection {
    fn hello(&self) -> &Hello;
    fn send(&mut self, request: &Request, on_message: &mut dyn FnMut(Message)) -> Result<()>;
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

/// Exit code returned by a shell when the command is not found.
const EXIT_COMMAND_NOT_FOUND: i32 = 127;

impl RemoteSession {
    /// Spawn the session command and read the hello message.
    ///
    /// Returns `Err(AgentNotInstalled)` if the process exits with 127
    /// before sending a hello, indicating the binary is not on the target.
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
                match child.try_wait()?.and_then(|s| s.code()) {
                    Some(EXIT_COMMAND_NOT_FOUND) => return Err(Error::AgentNotInstalled),
                    _ => {
                        return Err(Error::SetupProtocolError(
                            "agent exited before sending hello".into(),
                        ));
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

    fn read_message(&mut self) -> Result<Option<Message>> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(None);
        }
        let message: Message = serde_json::from_str(&line)?;
        Ok(Some(message))
    }

    fn send_request(&mut self, request: &protocol::Request) -> Result<()> {
        let writer = self.writer.as_mut().expect("stdin is still open");
        serde_json::to_writer(&mut *writer, request)?;
        writeln!(writer)?;
        writer.flush()?;
        Ok(())
    }

    /// Close stdin so the remote session knows no more requests are coming.
    fn close_stdin(&mut self) {
        self.writer.take();
    }
}

impl Connection for RemoteSession {
    fn hello(&self) -> &Hello {
        &self.hello
    }

    fn send(&mut self, request: &Request, on_message: &mut dyn FnMut(Message)) -> Result<()> {
        self.send_request(request)?;

        // Close stdin so that the other end knows no more messages are coming,
        // and it can exit and close its stdout, so that we can read messages
        // until EOF below.
        self.close_stdin();

        while let Some(message) = self.read_message()? {
            on_message(message);
        }
        Ok(())
    }
}

pub fn execute_plan(
    plan: &Plan,
    mut connect: impl FnMut(&Hostname) -> Result<Box<dyn Connection>>,
    mut on_message: impl FnMut(&Hostname, &Message),
) -> Result<()> {
    for (host, host_plan) in &plan.hosts {
        let mut conn = connect(host)?;

        let hello = conn.hello();
        if hello.version != protocol::VERSION {
            on_message(
                host,
                &Message::Error {
                    message: format!(
                        "version mismatch (local {}, remote {})",
                        protocol::VERSION,
                        hello.version,
                    ),
                },
            );
            continue;
        }

        let request = Request::Apply {
            target_commit: plan.commit.clone(),
            expected_current_commit: host_plan.expected_current.clone(),
        };
        conn.send(&request, &mut |message| {
            on_message(host, &message);
        })?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::plan::HostPlan;
    use crate::prim::Oid;
    use crate::session::HostSession;
    use crate::testutil::{TempDir, commit_files};

    /// In-memory connection that wraps a HostSession directly.
    struct LocalConnection {
        session: HostSession,
        hello: Hello,
        _store: TempDir,
        _apps: TempDir,
        _units: TempDir,
    }

    impl Connection for LocalConnection {
        fn hello(&self) -> &Hello {
            &self.hello
        }

        fn send(&mut self, request: &Request, on_message: &mut dyn FnMut(Message)) -> Result<()> {
            self.session
                .handle_request(request.clone(), &mut |msg| on_message(msg));
            Ok(())
        }
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
            _store: store,
            _apps: apps,
            _units: units,
        });
        Ok(TestHost { conn, commit_oid })
    }

    /// Execute a plan with a single host, returning all messages.
    fn run_single_host(commit: Oid, conn: Box<dyn Connection>) -> Result<Vec<Message>> {
        let plan = Plan {
            commit,
            hosts: BTreeMap::from([(
                Hostname::from("web1"),
                HostPlan {
                    apps: BTreeMap::new(),
                    expected_current: None,
                },
            )]),
        };
        let mut messages = Vec::new();
        let mut conn = Some(conn);
        execute_plan(
            &plan,
            |_| Ok(conn.take().expect("connect is called once per host")),
            |_, msg| messages.push(msg.clone()),
        )?;
        Ok(messages)
    }

    #[test]
    fn execute_plan_emits_apply_complete_for_fresh_host() -> Result<()> {
        let host = test_host()?;
        let messages = run_single_host(host.commit_oid.into(), host.conn)?;

        assert!(matches!(
            messages.last(),
            Some(Message::ApplyComplete { .. })
        ));
        Ok(())
    }

    #[test]
    fn execute_plan_reports_stale_when_current_ref_mismatches() -> Result<()> {
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
        let conn = Box::new(LocalConnection {
            session,
            hello,
            _store: store,
            _apps: apps,
            _units: units,
        });

        // The plan expects no prior commit, but the host already has one.
        let fake_commit = Oid::from("0000000000000000000000000000000000000000");
        let messages = run_single_host(fake_commit, conn)?;

        assert_eq!(messages.len(), 1);
        assert!(matches!(messages[0], Message::Stale { .. }));
        Ok(())
    }

    #[test]
    fn execute_plan_skips_host_on_version_mismatch() -> Result<()> {
        let host = test_host_with_version("0.0.0-fake")?;
        let messages = run_single_host(host.commit_oid.into(), host.conn)?;

        assert_eq!(messages.len(), 1);
        match &messages[0] {
            Message::Error { message } if message.contains("version mismatch") => { /* Ok */ }
            other => panic!("Expected version mismatch error, got {other:?}"),
        }
        Ok(())
    }

    // TODO: Add an integration test that spawns the real deptool binary in
    // local mode and exercises the full stdin/stdout protocol roundtrip.
}

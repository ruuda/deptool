//! Execute a deployment plan by driving remote host sessions.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use crate::error::Result;
use crate::plan::{Hostname, Plan};
use crate::protocol::{self, Hello, Message, Request};

pub trait Connection {
    fn read_hello(&mut self) -> Result<Hello>;
    fn send(&mut self, request: &Request, on_message: &mut dyn FnMut(Message)) -> Result<()>;
}

// TODO: Maybe we should rename "session" to "agent" after all. Then this can be
// AgentSession or something, the thing on the controller/initiator/operator
// side that enables us to talk to the agent.
pub struct RemoteSession {
    // Drop order is declaration order: close stdin first so the child can
    // finish, then close our reader, then reap the child process.
    writer: Option<ChildStdin>,
    reader: BufReader<ChildStdout>,

    // Not dead, needed to keep the child process alive.
    #[allow(dead_code)]
    child: Child,
}

impl RemoteSession {
    pub fn new(mut cmd: Command) -> Result<Self> {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;

        let reader = BufReader::new(child.stdout.take().expect("stdout is piped"));
        let writer = child.stdin.take().expect("stdin is piped");

        Ok(RemoteSession {
            child,
            reader,
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
    fn read_hello(&mut self) -> Result<Hello> {
        let mut line = String::new();
        self.reader.read_line(&mut line)?;
        Ok(serde_json::from_str(&line)?)
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

        let hello = conn.read_hello()?;
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
    use crate::oid::Oid;
    use crate::plan::HostPlan;
    use crate::session::HostSession;
    use crate::testutil::TempDir;

    /// In-memory connection that wraps a HostSession directly.
    struct LocalConnection {
        session: HostSession,
        hello: Hello,
        _store: TempDir,
    }

    impl LocalConnection {
        fn new(session: HostSession, hello: Hello, store: TempDir) -> Self {
            LocalConnection {
                session,
                hello,
                _store: store,
            }
        }
    }

    impl Connection for LocalConnection {
        fn read_hello(&mut self) -> Result<Hello> {
            Ok(self.hello.clone())
        }

        fn send(&mut self, request: &Request, on_message: &mut dyn FnMut(Message)) -> Result<()> {
            self.session
                .handle_request(request.clone(), &mut |msg| on_message(msg));
            Ok(())
        }
    }

    fn test_connection(host: &Hostname) -> Result<Box<dyn Connection>> {
        let store = TempDir::new("store");
        let repo = git2::Repository::init_bare(store.path())?;
        let session = HostSession::new(repo);
        let hello = Hello {
            version: protocol::VERSION.to_string(),
            hostname: host.0.clone(),
        };
        Ok(Box::new(LocalConnection::new(session, hello, store)))
    }

    #[test]
    fn execute_plan_emits_apply_complete_for_fresh_host() -> Result<()> {
        let plan = Plan {
            commit: Oid::from("0000000000000000000000000000000000000000"),
            hosts: BTreeMap::from([(
                Hostname::from("web1"),
                HostPlan {
                    apps: BTreeMap::new(),
                    expected_current: None,
                },
            )]),
        };

        let mut messages = Vec::new();
        execute_plan(&plan, test_connection, |_, msg| messages.push(msg.clone()))?;

        assert_eq!(messages.len(), 1);
        assert!(matches!(messages[0], Message::ApplyComplete { .. }));
        Ok(())
    }

    #[test]
    fn execute_plan_reports_stale_when_current_ref_mismatches() -> Result<()> {
        let store = TempDir::new("store");
        let repo = git2::Repository::init_bare(store.path())?;

        // Create a commit and point the host's current ref at it.
        let actual_commit = crate::testutil::commit_files(&repo, &[("web1/nginx/conf", b"v1")])?;
        crate::store::set_ref(
            &repo,
            "refs/heads/current",
            actual_commit,
            crate::store::RefUpdate::SetCurrent,
        )?;

        // The plan expects no prior commit, but the host already has one.
        let plan = Plan {
            commit: Oid::from("0000000000000000000000000000000000000000"),
            hosts: BTreeMap::from([(
                Hostname::from("web1"),
                HostPlan {
                    apps: BTreeMap::new(),
                    expected_current: None,
                },
            )]),
        };

        let session = HostSession::new(repo);
        let hello = Hello {
            version: protocol::VERSION.to_string(),
            hostname: "web1".to_string(),
        };
        let mut conn: Option<Box<dyn Connection>> =
            Some(Box::new(LocalConnection::new(session, hello, store)));

        let mut messages = Vec::new();
        execute_plan(
            &plan,
            |_| Ok(conn.take().expect("connect is called once per host")),
            |_, msg| messages.push(msg.clone()),
        )?;

        assert_eq!(messages.len(), 1);
        assert!(matches!(messages[0], Message::Stale { .. }));
        Ok(())
    }
}

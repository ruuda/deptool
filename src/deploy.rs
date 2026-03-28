//! Execute a deployment plan by driving remote host sessions.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use crate::error::Result;
use crate::plan::{Hostname, Plan};
use crate::protocol::{self, Message};

struct RemoteSession {
    // Drop order is declaration order: close stdin first so the child can
    // finish, then close our reader, then reap the child process.
    writer: Option<ChildStdin>,
    reader: BufReader<ChildStdout>,

    // Not dead, needed to keep the child process alive.
    #[allow(dead_code)]
    child: Child,
}

impl RemoteSession {
    fn new(mut cmd: Command) -> Result<Self> {
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

pub fn execute_plan(
    plan: &Plan,
    make_session_command: impl Fn(&Hostname) -> Command,
) -> Result<()> {
    for (host, host_plan) in &plan.hosts {
        eprintln!("{host}: connecting ...");

        let mut session = RemoteSession::new(make_session_command(host))?;

        match session.read_message()? {
            Some(Message::Hello { version, hostname }) => {
                eprintln!("{host}: connected to {hostname}, deptool {version}");
                if version != protocol::VERSION {
                    eprintln!(
                        "{host}: version mismatch (local {}, remote {version}), skipping",
                        protocol::VERSION,
                    );
                    continue;
                }
            }
            other => {
                eprintln!("{host}: unexpected initial message: {other:?}, skipping");
                continue;
            }
        }

        let request = protocol::Request::Apply {
            target_commit: plan.commit.clone(),
            expected_current_commit: host_plan.expected_current.clone(),
        };
        session.send_request(&request)?;

        // Close stdin so that the other end knows no more messages are coming,
        // and it can exit and close its stdout, so that we can read messages
        // until EOF below.
        session.close_stdin();

        while let Some(message) = session.read_message()? {
            match &message {
                Message::AppliedApp { app, diff } => {
                    eprintln!("{host}: applied {app}: {diff:?}");
                }
                Message::ApplyComplete { .. } => {
                    eprintln!("{host}: done");
                }
                Message::Stale {
                    expected_commit,
                    actual_commit,
                } => {
                    eprintln!(
                        "{host}: stale plan (expected {expected_commit:?}, actual {actual_commit:?})"
                    );
                }
                Message::Error { message } => {
                    eprintln!("{host}: error: {message}");
                }
                Message::Hello { .. } => {
                    eprintln!("{host}: unexpected Hello message");
                }
            }
        }
    }

    Ok(())
}

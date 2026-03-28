//! Deptool: a simple declarative deployment tool.

mod deploy;
mod error;
mod oid;
mod plan;
mod protocol;
mod session;
mod store;

#[cfg(test)]
mod testutil;

use std::fs;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::Command;

use bpaf::{Bpaf, Parser};
use git2::Repository;

use error::Result;

#[derive(Debug, Clone, Bpaf)]
enum HostCmd {
    /// Apply a single commit and exit.
    #[bpaf(command)]
    Apply {
        /// Path to the bare Git store.
        #[bpaf(positional("STORE"))]
        store: PathBuf,
        /// Commit hash to apply.
        #[bpaf(positional("COMMIT"))]
        commit: String,
    },
    /// Start an interactive session over stdin/stdout.
    #[bpaf(command)]
    Session {
        /// Path to the bare Git store.
        #[bpaf(positional("STORE"))]
        store: PathBuf,
    },
}

#[derive(Debug, Clone)]
enum DeployMode {
    Local,
    Remote,
}

#[derive(Debug, Clone, Bpaf)]
enum Cmd {
    /// Record a directory as a new commit in the store.
    #[bpaf(command)]
    Commit {
        /// Path to the bare Git store.
        #[bpaf(positional("STORE"))]
        store: PathBuf,
        /// Directory to commit.
        #[bpaf(positional("DIR"))]
        dir: PathBuf,
    },
    /// Compute and save the deployment plan.
    #[bpaf(command)]
    Plan {
        /// Path to the bare Git store.
        #[bpaf(positional("STORE"))]
        store: PathBuf,
        /// Output file for the plan (default: plan.json).
        #[bpaf(long("plan-file"), fallback(PathBuf::from("plan.json")))]
        output: PathBuf,
    },
    /// Apply a saved plan to all hosts.
    #[bpaf(command)]
    Apply {
        /// Path to the plan file (default: plan.json).
        #[bpaf(long("plan-file"), fallback(PathBuf::from("plan.json")))]
        plan: PathBuf,
        /// Path to the store on the remote host.
        #[bpaf(long("remote-store"))]
        remote_store: String,
        /// Run deptool locally instead of over SSH (for testing).
        #[bpaf(long("local"), flag(DeployMode::Local, DeployMode::Remote))]
        mode: DeployMode,
    },
    /// Commands that run on target hosts.
    #[bpaf(command)]
    Host {
        #[bpaf(external(host_cmd))]
        cmd: HostCmd,
    },
}

fn run() -> Result<()> {
    let cmd = cmd().to_options().run();

    match cmd {
        Cmd::Commit { store, dir } => {
            let repo = match Repository::open(&store) {
                Ok(r) => r,
                Err(_) => Repository::init_bare(&store)?,
            };
            let tree_oid = store::build_tree(&repo, &dir)?;
            let commit_oid = store::commit_tree(&repo, tree_oid)?;
            println!("{commit_oid}");
        }
        Cmd::Plan { store, output } => {
            let repo = Repository::open(&store)?;
            let plan = plan::make_plan(&repo)?;
            let json = serde_json::to_string_pretty(&plan)?;
            fs::write(&output, json)?;
            eprintln!("Plan written to {}", output.display());
        }
        Cmd::Apply {
            plan: plan_path,
            remote_store,
            mode,
        } => {
            let json = fs::read_to_string(&plan_path)?;
            let plan: plan::Plan = serde_json::from_str(&json)?;
            let make_command = |host: &plan::Hostname| match mode {
                DeployMode::Local => {
                    let mut cmd =
                        Command::new(std::env::current_exe().expect("current exe is known"));
                    cmd.args(["host", "session", &remote_store]);
                    cmd
                }
                DeployMode::Remote => {
                    // SSH concatenates remote arguments into a single string
                    // and passes them to the user's login shell. This is safe
                    // as long as the store path and hostname contain no spaces
                    // or shell metacharacters, which holds for our setup.
                    let mut cmd = Command::new("ssh");
                    cmd.args([&host.0, "deptool", "host", "session", &remote_store]);
                    cmd
                }
            };
            deploy::execute_plan(
                &plan,
                |host| Ok(Box::new(deploy::RemoteSession::new(make_command(host))?)),
                |host, message| eprintln!("{host}: {message:?}"),
            )?;
        }
        Cmd::Host { cmd } => run_host(cmd)?,
    }

    Ok(())
}

fn run_host(cmd: HostCmd) -> Result<()> {
    match cmd {
        HostCmd::Apply { store, commit } => {
            let repo = Repository::open(&store)?;
            let session = session::HostSession::new(repo);
            let request = protocol::Request::Apply {
                target_commit: commit.as_str().into(),
                expected_current_commit: None,
            };
            session.handle_request(request, &mut |response| {
                eprintln!("{response:?}");
            });
        }
        HostCmd::Session { store } => {
            let repo = Repository::open(&store)?;
            let session = session::HostSession::new(repo);
            let stdin = std::io::stdin().lock();
            let mut stdout = std::io::stdout().lock();

            let hostname = fs::read_to_string("/etc/hostname")
                .unwrap_or("(unknown hostname)".into())
                .trim()
                .to_string();
            let hello = protocol::Hello {
                version: protocol::VERSION.to_string(),
                hostname,
            };
            serde_json::to_writer(&mut stdout, &hello)?;
            writeln!(stdout)?;
            stdout.flush()?;

            for line in stdin.lines() {
                let request: protocol::Request = serde_json::from_str(&line?)?;
                session.handle_request(request, &mut |message| {
                    // Ignore write errors here; the operator may have disconnected.
                    let _ = serde_json::to_writer(&mut stdout, &message);
                    let _ = writeln!(stdout);
                    let _ = stdout.flush();
                });
            }
        }
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

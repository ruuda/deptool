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
        Cmd::Apply { plan: plan_path } => {
            let json = fs::read_to_string(&plan_path)?;
            let plan: plan::Plan = serde_json::from_str(&json)?;
            for (host, host_plan) in &plan.hosts {
                eprintln!(
                    "Would deploy to {host}: {} app(s) to change",
                    host_plan.apps.len()
                );
            }
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
            let hello = protocol::Response::Hello {
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

//! Deptool: a simple declarative deployment tool.

mod apply;
mod deploy;
mod display;
mod error;
mod plan;
mod prim;
mod protocol;
mod session;
mod setup;
mod store;

#[cfg(test)]
mod testutil;

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::Command;

use bpaf::Bpaf;
use git2::Repository;

use deploy::Connection;
use error::{Error, Result};
use prim::Hostname;

#[derive(Debug, Clone, Bpaf)]
enum AgentCmd {
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

#[derive(Debug, Clone, Copy)]
enum DeployMode {
    Local,
    Remote,
}

#[derive(Debug, Clone, Copy)]
enum ConfirmMode {
    Prompt,
    ApplyWithoutPrompt,
}

#[derive(Debug, Clone, Bpaf)]
enum Cmd {
    /// Record a directory as a new commit in the store.
    #[bpaf(command)]
    Commit {
        /// Path to the local store (default: ./deptool_store).
        #[bpaf(long("store"), fallback(PathBuf::from("deptool_store")))]
        store: PathBuf,
        /// Directory to commit.
        #[bpaf(positional("DIR"))]
        dir: PathBuf,
    },
    /// Plan and apply changes to all hosts.
    #[bpaf(command)]
    Deploy {
        /// Path to the local store (default: ./deptool_store).
        #[bpaf(long("store"), fallback(PathBuf::from("deptool_store")))]
        store: PathBuf,
        /// Path to the store on target hosts (default: /var/lib/deptool/store).
        #[bpaf(
            long("remote-store"),
            fallback(PathBuf::from("/var/lib/deptool/store"))
        )]
        remote_store: PathBuf,
        /// Compute and display the plan, then exit without applying.
        #[bpaf(long("plan-only"), switch)]
        plan_only: bool,
        /// Apply without prompting for confirmation.
        #[bpaf(
            long("no-confirm"),
            flag(ConfirmMode::ApplyWithoutPrompt, ConfirmMode::Prompt)
        )]
        confirm_mode: ConfirmMode,
        /// Run the agent locally instead of over SSH (for testing).
        #[bpaf(long("local"), flag(DeployMode::Local, DeployMode::Remote))]
        mode: DeployMode,
    },
    /// Commands that run on target hosts (used internally over SSH).
    #[bpaf(command)]
    Agent {
        #[bpaf(external(agent_cmd))]
        cmd: AgentCmd,
    },
}

#[derive(Debug, Clone, Bpaf)]
#[bpaf(options)]
struct Args {
    #[bpaf(external(cmd))]
    cmd: Cmd,
}

fn run_deploy(
    store: PathBuf,
    remote_store: PathBuf,
    plan_only: bool,
    confirm_mode: ConfirmMode,
    mode: DeployMode,
) -> Result<()> {
    let repo = Repository::open(&store)?;
    let plan = plan::make_plan(&repo)?;

    if plan.hosts.is_empty() {
        eprintln!("All hosts are up to date.");
        return Ok(());
    }

    let color = display::UseColor::from_env();
    display::print_plan(&mut std::io::stdout(), &repo, &plan, color)?;

    if plan_only {
        return Ok(());
    }

    let decision = match confirm_mode {
        ConfirmMode::ApplyWithoutPrompt => display::Decision::Apply,
        ConfirmMode::Prompt => display::confirm(&repo, &plan, &store, color)?,
    };
    if let display::Decision::Abort = decision {
        return Ok(());
    }

    let remote_store_str = remote_store
        .to_str()
        .ok_or_else(|| Error::InvalidConfig("remote store path is not valid UTF-8".into()))?
        .to_string();

    let binary = std::fs::read(std::env::current_exe().expect("current exe path is known"))?;
    // 5 bytes (10 hex chars) should be long enough to avoid collisions,
    // and short enough to keep paths and commands readable and debuggable.
    let suffix = setup::truncated_sha256(&binary, 5);
    let bin_name = format!("deptool-{}-{}", protocol::VERSION, &suffix);
    let remote_bin_path = format!("/var/lib/deptool/bin/{bin_name}");

    // SSH concatenates remote arguments into a single shell string.
    // We assert the inputs are shell-safe; in the future we should
    // pass the store path over stdin instead.
    let is_shell_safe = |s: &str| s.chars().all(|c| c.is_alphanumeric() || "/_.-".contains(c));
    assert!(
        is_shell_safe(&remote_store_str),
        "remote store path is free of shell metacharacters"
    );
    assert!(
        is_shell_safe(&remote_bin_path),
        "remote binary path is free of shell metacharacters"
    );

    let connect = |host: &Hostname| -> Result<Box<dyn Connection>> {
        let cmd = match mode {
            DeployMode::Local => {
                let mut cmd =
                    Command::new(std::env::current_exe().expect("current exe path is known"));
                cmd.args(["agent", "session", &remote_store_str]);
                cmd
            }
            DeployMode::Remote => {
                assert!(
                    is_shell_safe(&host.0),
                    "hostname is free of shell metacharacters"
                );
                let mut cmd = Command::new("ssh");
                cmd.args([
                    &host.0,
                    "sudo",
                    &remote_bin_path,
                    "agent",
                    "session",
                    &remote_store_str,
                ]);
                cmd
            }
        };
        let session = deploy::RemoteSession::new(cmd)?;
        Ok(Box::new(session))
    };
    let install =
        |host: &Hostname| -> Result<()> { setup::install_binary(host, &remote_bin_path, &binary) };

    let hosts: Vec<_> = plan.hosts.keys().cloned().collect();
    let mut printer = display::StatusPrinter::new(color);
    let mut progress =
        deploy::DeployProgress::new(hosts, Box::new(move |states| printer.print(states)));
    let mut lock_result = deploy::lock_hosts(&plan, connect, install, &mut progress);

    if progress.has_failures() {
        // Fetch objects from stale hosts over their still-open
        // sessions so we have the data for the next plan.
        if let Err(err) = deploy::fetch_stale_objects(&repo, &mut lock_result.stale) {
            eprintln!("failed to fetch stale objects: {err}");
        }
        let n = progress.num_failed();
        return Err(Error::InvalidConfig(format!(
            "failed to lock {n} host(s), aborting",
        )));
    }

    let mut connections = lock_result.locked;

    deploy::push_packs(&repo, &plan, &mut connections, &mut progress)?;
    deploy::apply_hosts(&repo, &plan, &mut connections, &mut progress, |_, _| {})?;

    Ok(())
}

fn run() -> Result<()> {
    let args = args().run();

    match args.cmd {
        Cmd::Commit { store, dir } => {
            let repo = store::open_or_init(&store)?;
            let tree_oid = store::build_tree(&repo, &dir)?;
            let commit_oid = store::commit_tree(&repo, tree_oid)?;
            println!("{commit_oid}");
        }
        Cmd::Deploy {
            store,
            remote_store,
            plan_only,
            confirm_mode,
            mode,
        } => run_deploy(store, remote_store, plan_only, confirm_mode, mode)?,
        Cmd::Agent { cmd } => run_agent(cmd)?,
    }

    Ok(())
}

const DEFAULT_APPS_DIR: &str = "/var/lib/deptool/apps";
const DEFAULT_UNIT_DIR: &str = "/etc/systemd/system";

fn read_hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .unwrap_or("(unknown hostname)".into())
        .trim()
        .to_string()
}

fn systemd_apply_changes(changes: &plan::UnitChanges) -> error::Result<()> {
    std::process::Command::new("systemctl")
        .arg("daemon-reload")
        .status()?;
    for unit in &changes.disable {
        std::process::Command::new("systemctl")
            .args(["disable", "--now", unit])
            .status()?;
    }
    for unit in &changes.enable {
        std::process::Command::new("systemctl")
            .args(["enable", "--now", unit])
            .status()?;
    }
    for unit in &changes.restart {
        std::process::Command::new("systemctl")
            .args(["restart", unit])
            .status()?;
    }
    // TODO: Capture `systemctl status <unit>` output and report it
    // back to the operator, so they can see startup logs or failure
    // reasons without having to SSH in.
    Ok(())
}

fn make_host_session(repo: Repository, hostname: String) -> session::HostSession {
    session::HostSession::new(
        repo,
        prim::Hostname(hostname),
        PathBuf::from(DEFAULT_APPS_DIR),
        PathBuf::from(DEFAULT_UNIT_DIR),
        Box::new(systemd_apply_changes),
    )
}

fn run_agent(cmd: AgentCmd) -> Result<()> {
    match cmd {
        AgentCmd::Apply { store, commit } => {
            let repo = Repository::open(&store)?;
            let hostname = read_hostname();
            let mut session = make_host_session(repo, hostname);
            let request = protocol::Request::Apply {
                target_commit: commit.as_str().into(),
            };
            session.handle_request(request, &mut |response| {
                eprintln!("{response:?}");
            });
        }
        AgentCmd::Session { store } => {
            let repo = store::open_or_init(&store)?;
            let hostname = read_hostname();
            let mut session = make_host_session(repo, hostname.clone());
            let stdin = std::io::stdin().lock();
            let mut stdout = std::io::stdout().lock();

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

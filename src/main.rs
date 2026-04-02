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

use error::Result;

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

fn run() -> Result<()> {
    let args = args().run();

    match args.cmd {
        Cmd::Commit { store, dir } => {
            let repo = match Repository::open(&store) {
                Ok(r) => r,
                Err(_) => Repository::init_bare(&store)?,
            };
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
        } => {
            let repo = Repository::open(&store)?;
            let plan = plan::make_plan(&repo)?;

            if plan.hosts.is_empty() {
                eprintln!("All hosts are up to date.");
                return Ok(());
            }

            display::print_plan(&mut std::io::stdout(), &repo, &plan)?;

            if plan_only {
                return Ok(());
            }

            let decision = match confirm_mode {
                ConfirmMode::ApplyWithoutPrompt => display::Decision::Apply,
                ConfirmMode::Prompt => display::confirm(&repo, &plan, &store)?,
            };
            if let display::Decision::Abort = decision {
                return Ok(());
            }

            let remote_store_str = remote_store
                .to_str()
                .ok_or_else(|| {
                    error::Error::InvalidConfig("remote store path is not valid UTF-8".into())
                })?
                .to_string();

            let binary = std::fs::read(
                std::env::current_exe().expect("current exe path is known"),
            )?;
            let suffix = setup::binary_suffix(&binary);
            let bin_name = setup::binary_name(protocol::VERSION, &suffix);
            let remote_bin_path = setup::remote_binary_path(&bin_name);

            // SSH concatenates remote arguments into a single shell string.
            // We assert the inputs are shell-safe; in the future we should
            // pass the store path over stdin instead.
            let is_shell_safe =
                |s: &str| s.chars().all(|c| c.is_alphanumeric() || "/_.-".contains(c));
            assert!(
                is_shell_safe(&remote_store_str),
                "remote store path is free of shell metacharacters"
            );
            assert!(
                is_shell_safe(&remote_bin_path),
                "remote binary path is free of shell metacharacters"
            );

            let make_session_cmd = |host: &prim::Hostname| -> Command {
                match mode {
                    DeployMode::Local => {
                        let mut cmd = Command::new(
                            std::env::current_exe().expect("current exe path is known"),
                        );
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
                            &remote_bin_path,
                            "agent",
                            "session",
                            &remote_store_str,
                        ]);
                        cmd
                    }
                }
            };

            let make_install_cmd = |host: &prim::Hostname| -> Command {
                let mut cmd = Command::new("ssh");
                cmd.args([&host.0, &setup::install_command(&remote_bin_path)]);
                cmd
            };

            deploy::execute_plan(
                &plan,
                |host| {
                    deploy::connect_with_setup(
                        || make_session_cmd(host),
                        |bytes| {
                            let mut child = make_install_cmd(host)
                                .stdin(std::process::Stdio::piped())
                                .stdout(std::process::Stdio::piped())
                                .spawn()?;
                            use std::io::Write;
                            child.stdin.take().expect("stdin is piped").write_all(bytes)?;
                            Ok(child.wait_with_output()?.stdout)
                        },
                        &binary,
                        &remote_bin_path,
                    )
                },
                |host, message| eprintln!("{host}: {message:?}"),
            )?;
        }
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
            let session = make_host_session(repo, hostname);
            let request = protocol::Request::Apply {
                target_commit: commit.as_str().into(),
                expected_current_commit: None,
            };
            session.handle_request(request, &mut |response| {
                eprintln!("{response:?}");
            });
        }
        AgentCmd::Session { store } => {
            let repo = match Repository::open(&store) {
                Ok(r) => r,
                Err(_) => Repository::init_bare(&store)?,
            };
            let hostname = read_hostname();
            let session = make_host_session(repo, hostname.clone());
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

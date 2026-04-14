//! Deptool: a simple declarative deployment tool.

use deptool::*;

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use bpaf::Bpaf;

use deploy::Connection;
use error::{ApplyError, Error, HostError, Result};
use prim::Hostname;
use store::Store;

#[derive(Debug, Clone, Bpaf)]
enum AgentCmd {
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

#[derive(Debug, Clone, Copy)]
enum PushMode {
    ForwardOnly,
    ForcePush,
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
        /// Allow deploying commits that don't descend from the host's current state.
        #[bpaf(long("force-push"), flag(PushMode::ForcePush, PushMode::ForwardOnly))]
        push_mode: PushMode,
        #[bpaf(long("local"), flag(DeployMode::Local, DeployMode::Remote), hide)]
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
    push_mode: PushMode,
    mode: DeployMode,
) -> Result<()> {
    let repo = Store::open(&store)?;
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

    for (host, host_plan) in &plan.hosts {
        if !host_plan.is_fast_forward {
            match push_mode {
                PushMode::ForwardOnly => return Err(Error::Diverged(host.clone())),
                PushMode::ForcePush => break,
            }
        }
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
        .expect("remote store path is valid UTF-8")
        .to_string();

    let binary = std::fs::read(std::env::current_exe().expect("current exe path is known"))?;
    // 5 bytes (10 hex chars) should be long enough to avoid collisions,
    // and short enough to keep paths and commands readable and debuggable.
    let suffix = setup::truncated_sha256(&binary, 5);
    let bin_name = format!("deptool-{}-{}", protocol::VERSION, &suffix);
    let remote_bin_path = format!("{}/{bin_name}", setup::BIN_DIR);

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

    let connect = |host: &Hostname| -> std::result::Result<Box<dyn Connection>, HostError> {
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
    let install = |host: &Hostname| -> std::result::Result<(), HostError> {
        setup::install_binary(host, &remote_bin_path, &binary)
    };

    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
    let hostname = prim::read_hostname();
    let operator = format!("{user}@{hostname}");

    let hosts: Vec<_> = plan.hosts.keys().cloned().collect();
    let printer = display::StatusPrinter::new(color);
    let progress = deploy::DeployProgress::new(hosts, Box::new(printer));

    deploy::run_deploy(&repo, &plan, &operator, connect, install, &progress)
}

fn run() -> Result<()> {
    let args = args().run();

    match args.cmd {
        Cmd::Commit { store, dir } => {
            let store = Store::open_or_init(&store)?;
            let tree_oid = store.build_tree(&dir)?;
            store.validate(tree_oid)?;
            match store.commit_tree(tree_oid)? {
                Some(commit_oid) => println!("{commit_oid}"),
                None => println!("No changes."),
            }
        }
        Cmd::Deploy {
            store,
            remote_store,
            plan_only,
            confirm_mode,
            push_mode,
            mode,
        } => run_deploy(
            store,
            remote_store,
            plan_only,
            confirm_mode,
            push_mode,
            mode,
        )?,
        Cmd::Agent { cmd } => run_agent(cmd)?,
    }

    Ok(())
}

use session::AgentConfig;

fn post_apply(
    desired_units: &apply::DesiredUnits,
    changes: &plan::SystemDiff<PathBuf>,
    emit: &mut dyn FnMut(protocol::Message),
    apps_dir: &Path,
    unit_dir: &Path,
) -> std::result::Result<(), ApplyError> {
    // Reconcile manifest symlinks (e.g. config files in /etc) *before*
    // any systemd lifecycle operations, because units may depend on paths
    // that these symlinks provide.
    apply::reconcile_manifest_symlinks(apps_dir, &changes.symlinks)?;

    let mut touched: Vec<&str> = Vec::new();

    for unit in &changes.units.disable {
        touched.push(unit);
        systemctl_ok(&["disable", "--now", unit]);
    }

    // Reconcile unit symlinks, then reload so systemd picks them up.
    // This runs after disable because systemd treats our symlinks as
    // "linked units" and `systemctl disable` removes the link itself,
    // not just the enablement symlinks. Reconciling here restores them
    // and also picks up any new units from the deploy.
    let symlink_changes = apply::reconcile_symlinks(desired_units, apps_dir, unit_dir)?;

    // Only poke systemd when something actually changed on disk.
    let needs_reload = !symlink_changes.is_empty()
        || !touched.is_empty()
        || !changes.units.enable.is_empty()
        || !changes.units.restart.is_empty();
    if needs_reload {
        systemctl_ok(&["daemon-reload"]);
    }

    for unit in &changes.units.enable {
        touched.push(unit);
        systemctl_ok(&["enable", "--now", unit]);
    }
    for unit in &changes.units.restart {
        touched.push(unit);
        systemctl_ok(&["restart", unit]);
    }

    if touched.is_empty() {
        return Ok(());
    }

    // Let services initialize before checking state. Previously we usd 100ms,
    // but that was not enough for a binary inside a verity-protected EROFS to
    // fully start and then fail on a potato cloud VM, let's give it 300ms.
    std::thread::sleep(std::time::Duration::from_millis(300));

    let mut is_active_cmd = vec!["is-active"];
    for unit in changes.units.enable.iter().chain(&changes.units.restart) {
        is_active_cmd.push(unit);
    }
    let all_active = is_active_cmd.len() == 1 || systemctl_ok(&is_active_cmd);

    // Force color: systemctl won't color because stdout is a pipe,
    // but the output is forwarded to the operator's terminal.
    let output = match std::process::Command::new("systemctl")
        .arg("status")
        .args(&touched)
        .env("SYSTEMD_COLORS", "true")
        .output()
    {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        Err(err) => format!("failed to run systemctl status: {err}"),
    };
    emit(protocol::Message::SystemdUnitStatus { output });

    if all_active {
        Ok(())
    } else {
        Err(ApplyError::SystemdActivationFailed)
    }
}

fn systemctl_ok(args: &[&str]) -> bool {
    std::process::Command::new("systemctl")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn make_host_session(store: Store, config: &AgentConfig) -> session::HostSession {
    let apps_dir = config.apps_dir.clone();
    let unit_dir = config.unit_dir.clone();
    session::HostSession::new(
        store,
        prim::Hostname(config.hostname.clone()),
        config.apps_dir.clone(),
        Box::new(move |desired_units, changes, emit| {
            post_apply(desired_units, changes, emit, &apps_dir, &unit_dir)
        }),
    )
}

fn run_agent(cmd: AgentCmd) -> Result<()> {
    match cmd {
        AgentCmd::Session { store } => {
            // Since we install the exact agent binary that the driver needs on
            // demand, versions can pile up on the target host (especially
            // during development), so GC the bin directory.
            let gc_result = match std::env::current_exe() {
                Ok(exe) => setup::gc_bin_dir(&exe),
                Err(_) => Ok(()), // Can't determine our path, skip GC.
            };
            if let Err(err) = gc_result {
                eprintln!("gc: {err}");
            }

            let store = Store::open_or_init(&store)?;
            let config = AgentConfig::from_env();
            let mut session = make_host_session(store, &config);
            let stdin = std::io::stdin().lock();
            let mut stdout = std::io::stdout().lock();

            let hello = protocol::Hello {
                version: protocol::VERSION.to_string(),
                hostname: config.hostname,
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

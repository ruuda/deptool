//! Deptool: a simple declarative deployment tool.

use deptool::*;

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use bpaf::Bpaf;

use deploy::Connection;
use error::{ApplyError, HostError, Result};
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

#[derive(Debug, Clone, Bpaf)]
enum Cmd {
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
        #[bpaf(long("local"), flag(DeployMode::Local, DeployMode::Remote), hide)]
        mode: DeployMode,
        /// Directory containing the cluster config to deploy.
        #[bpaf(positional("DIR"))]
        dir: PathBuf,
    },
    /// Refresh tracking refs by connecting to hosts.
    #[bpaf(command)]
    Sync {
        /// Path to the local store (default: ./deptool_store).
        #[bpaf(long("store"), fallback(PathBuf::from("deptool_store")))]
        store: PathBuf,
        /// Path to the store on target hosts (default: /var/lib/deptool/store).
        #[bpaf(
            long("remote-store"),
            fallback(PathBuf::from("/var/lib/deptool/store"))
        )]
        remote_store: PathBuf,
        #[bpaf(
            long("all"),
            flag(sync::SyncMode::AllHosts, sync::SyncMode::OnlyAffectedHosts)
        )]
        mode: sync::SyncMode,
        /// Directory containing the cluster config.
        #[bpaf(positional("DIR"))]
        dir: PathBuf,
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

fn is_shell_safe(s: &str) -> bool {
    s.chars().all(|c| c.is_alphanumeric() || "/_.-".contains(c))
}

struct RemoteConnector {
    remote_store: String,
    remote_bin_path: String,
    binary: Vec<u8>,
}

impl RemoteConnector {
    fn new(remote_store: &Path) -> Result<Self> {
        let remote_store = remote_store
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
        assert!(
            is_shell_safe(&remote_store),
            "remote store path is free of shell metacharacters"
        );
        assert!(
            is_shell_safe(&remote_bin_path),
            "remote binary path is free of shell metacharacters"
        );
        Ok(Self {
            remote_store,
            remote_bin_path,
            binary,
        })
    }
}

impl setup::HostConnector for RemoteConnector {
    fn connect(&self, host: &Hostname) -> std::result::Result<Box<dyn Connection>, HostError> {
        assert!(
            is_shell_safe(&host.0),
            "hostname is free of shell metacharacters"
        );
        let mut cmd = Command::new("ssh");
        cmd.args([
            &host.0,
            "sudo",
            &self.remote_bin_path,
            "agent",
            "session",
            &self.remote_store,
        ]);
        let session = deploy::RemoteSession::new(cmd)?;
        Ok(Box::new(session))
    }

    fn install(&self, host: &Hostname) -> std::result::Result<(), HostError> {
        setup::install_binary(host, &self.remote_bin_path, &self.binary)
    }
}

struct LocalConnector {
    remote_store: String,
}

impl setup::HostConnector for LocalConnector {
    fn connect(&self, _host: &Hostname) -> std::result::Result<Box<dyn Connection>, HostError> {
        let mut cmd = Command::new(std::env::current_exe().expect("current exe path is known"));
        cmd.args(["agent", "session", &self.remote_store]);
        let session = deploy::RemoteSession::new(cmd)?;
        Ok(Box::new(session))
    }

    fn install(&self, _host: &Hostname) -> std::result::Result<(), HostError> {
        Ok(())
    }
}

fn run_deploy(
    store: PathBuf,
    dir: PathBuf,
    remote_store: PathBuf,
    plan_only: bool,
    confirm_mode: ConfirmMode,
    mode: DeployMode,
) -> Result<()> {
    let repo = Store::open_or_init(&store)?;
    let tree_oid = repo.build_tree(&dir)?;

    let plan = match plan::make_plan(&repo, tree_oid)? {
        Some(plan) => plan,
        None => {
            eprintln!("All hosts are up to date.");
            return Ok(());
        }
    };

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

    let connector: Box<dyn setup::HostConnector> = match mode {
        DeployMode::Remote => Box::new(RemoteConnector::new(&remote_store)?),
        DeployMode::Local => {
            let remote_store = remote_store
                .to_str()
                .expect("remote store path is valid UTF-8");
            Box::new(LocalConnector {
                remote_store: remote_store.to_string(),
            })
        }
    };

    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
    let hostname = prim::read_hostname();
    let operator = format!("{user}@{hostname}");

    let hosts: Vec<_> = plan.hosts.keys().cloned().collect();
    let observer = display::StatusPrinter::new(display::UseColor::from_env());
    let progress = deploy::DeployProgress::new(hosts, Box::new(observer));

    deploy::run_deploy(&repo, &plan, &operator, &*connector, &progress)
}

fn run() -> Result<()> {
    let args = args().run();

    match args.cmd {
        Cmd::Deploy {
            store,
            remote_store,
            plan_only,
            confirm_mode,
            mode,
            dir,
        } => run_deploy(store, dir, remote_store, plan_only, confirm_mode, mode)?,
        Cmd::Sync {
            store,
            remote_store,
            mode,
            dir,
        } => {
            let store = Store::open_or_init(&store)?;
            let connector = RemoteConnector::new(&remote_store)?;
            let observer = display::StatusPrinter::new(display::UseColor::from_env());
            sync::run_sync(&store, &dir, &connector, mode, Box::new(observer))?;
        }
        Cmd::Agent { cmd } => run_agent(cmd)?,
    }

    Ok(())
}

use agent::AgentConfig;

fn activate(
    desired_units: &plan::DesiredUnits,
    changes: &plan::SystemDiff<PathBuf>,
    emit: &mut dyn FnMut(protocol::Message),
    log: &mut dyn FnMut(&str),
    apps_dir: &Path,
    unit_dir: &Path,
) -> std::result::Result<(), ApplyError> {
    // Reconcile manifest symlinks (e.g. config files in /etc) *before*
    // any systemd lifecycle operations, because units may depend on paths
    // that these symlinks provide.
    // TODO: log individual symlink changes.
    checkout::reconcile_config_symlinks(apps_dir, &changes.symlinks)?;

    for unit in &changes.units.disable {
        log(&format!("disabling {unit}"));
        systemctl_ok(&["disable", "--now", unit]);
    }

    // Reconcile unit symlinks, then reload so systemd picks them up.
    // This runs after disable because systemd treats our symlinks as
    // "linked units" and `systemctl disable` removes the link itself,
    // not just the enablement symlinks. Reconciling here restores them
    // and also picks up any new units from the deploy.
    let symlink_changes = checkout::reconcile_unit_symlinks(desired_units, apps_dir, unit_dir)?;

    // Only poke systemd when something actually changed on disk.
    let needs_reload = !symlink_changes.is_empty() || !changes.units.is_empty();
    if needs_reload {
        log("daemon-reload");
        systemctl_ok(&["daemon-reload"]);
    }

    let mut touched: Vec<&str> = Vec::new();

    for unit in &changes.units.enable {
        log(&format!("enabling {unit}"));
        touched.push(unit);
        systemctl_ok(&["enable", "--now", unit]);
    }
    for unit in &changes.units.restart {
        log(&format!("restarting {unit}"));
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
    is_active_cmd.extend(&touched);
    let all_active = systemctl_ok(&is_active_cmd);

    // Force color: systemctl won't color because stdout is a pipe,
    // but the output is forwarded to the operator's terminal.
    let output = match std::process::Command::new("systemctl")
        .args(&["status", "--lines=5"])
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

fn make_agent_session(store: Store, config: &AgentConfig) -> agent::AgentSession {
    let apps_dir = config.apps_dir.clone();
    let unit_dir = config.unit_dir.clone();
    let log_dir = config
        .apps_dir
        .parent()
        .unwrap_or(Path::new("/var/lib/deptool"));
    let log_path = log_dir.join("agent.log");
    agent::AgentSession::new(
        store,
        prim::Hostname(config.hostname.clone()),
        config.apps_dir.clone(),
        Box::new(
            move |desired_units, changes, emit, log: &mut dyn FnMut(&str)| {
                activate(desired_units, changes, emit, log, &apps_dir, &unit_dir)
            },
        ),
        Some(log_path),
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
            let current_commit = store.current_commit();
            let hello = protocol::Hello {
                version: protocol::VERSION.to_string(),
                hostname: config.hostname.clone(),
                current_commit,
            };
            let mut session = make_agent_session(store, &config);
            let stdin = std::io::stdin().lock();
            let mut stdout = std::io::stdout().lock();
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

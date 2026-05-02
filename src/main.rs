// Deptool -- A declarative configuration deployment tool.
// Copyright 2026 Ruud van Asseldonk

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// A copy of the License has been included in the root of the repository.

//! Deptool: a simple declarative deployment tool.

use deptool::*;

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use bpaf::{Bpaf, long};

use deploy::Connection;
use error::{ApplyError, Error, HostError, Result};
use plan::HostFilter;
use prim::Hostname;
use store::Store;

#[derive(Debug, Clone, Copy)]
enum ConnectMode {
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
        /// Compute and display the plan, then exit without applying.
        #[bpaf(long("plan-only"), switch)]
        plan_only: bool,
        /// Apply without prompting for confirmation.
        #[bpaf(
            long("no-confirm"),
            flag(ConfirmMode::ApplyWithoutPrompt, ConfirmMode::Prompt)
        )]
        confirm_mode: ConfirmMode,
        /// Restrict to the listed hosts (comma-separated, repeatable).
        #[bpaf(long("limit"), argument("HOSTS"), many)]
        limit: Vec<String>,
        #[bpaf(long("local"), flag(ConnectMode::Local, ConnectMode::Remote), hide)]
        mode: ConnectMode,
        /// Directory containing the config tree. Defaults to the previously
        /// used one.
        #[bpaf(positional("DIR"))]
        dir: Option<PathBuf>,
    },
    /// Refresh tracking refs by connecting to hosts.
    #[bpaf(command)]
    Sync {
        /// Only sync hosts whose deployed state differs from the config tree.
        #[bpaf(
            long("changed"),
            flag(sync::SyncMode::OnlyChanged, sync::SyncMode::AllHosts)
        )]
        sync_mode: sync::SyncMode,
        /// Restrict to the listed hosts (comma-separated, repeatable).
        #[bpaf(long("limit"), argument("HOSTS"), many)]
        limit: Vec<String>,
        #[bpaf(long("local"), flag(ConnectMode::Local, ConnectMode::Remote), hide)]
        connect_mode: ConnectMode,
        /// Directory containing the config tree. Defaults to the previously
        /// used one.
        #[bpaf(positional("DIR"))]
        dir: Option<PathBuf>,
    },
    /// Measure round-trip latency to each host in the cluster.
    #[bpaf(command)]
    Ping {
        /// Restrict to the listed hosts (comma-separated, repeatable).
        #[bpaf(long("limit"), argument("HOSTS"), many)]
        limit: Vec<String>,
        #[bpaf(long("local"), flag(ConnectMode::Local, ConnectMode::Remote), hide)]
        connect_mode: ConnectMode,
        /// Directory containing the config tree. Defaults to the previously
        /// used one.
        #[bpaf(positional("DIR"))]
        dir: Option<PathBuf>,
    },
    /// Show per-host deployment status, computed offline.
    #[bpaf(command)]
    Status {
        /// Restrict to the listed hosts (comma-separated, repeatable).
        #[bpaf(long("limit"), argument("HOSTS"), many)]
        limit: Vec<String>,
        /// Directory containing the config tree. Defaults to the previously
        /// used one.
        #[bpaf(positional("DIR"))]
        dir: Option<PathBuf>,
    },
    /// Show the full diff that would be applied by the next deploy.
    #[bpaf(command)]
    Diff {
        /// Show a per-file diffstat instead of the full diff.
        #[bpaf(
            long("stat"),
            flag(display::DiffMode::Stat, display::DiffMode::Full)
        )]
        mode: display::DiffMode,
        /// Restrict to the listed hosts (comma-separated, repeatable).
        #[bpaf(long("limit"), argument("HOSTS"), many)]
        limit: Vec<String>,
        /// Directory containing the config tree. Defaults to the previously
        /// used one.
        #[bpaf(positional("DIR"))]
        dir: Option<PathBuf>,
    },
    /// Create an empty store in the current directory.
    ///
    /// If `<dir>` is provided, it is recorded as the default cluster directory
    /// for subsequent commands.
    #[bpaf(command)]
    Init {
        /// Cluster directory to record as the default for subsequent commands.
        #[bpaf(positional("DIR"))]
        dir: Option<PathBuf>,
    },
    /// Run the agent on a target host (invoked internally over SSH).
    #[bpaf(command, hide)]
    Agent {
        /// Path to the bare Git store.
        #[bpaf(positional("STORE"))]
        store: PathBuf,
    },
}

#[derive(Debug, Clone, Bpaf)]
#[bpaf(options)]
struct Args {
    #[bpaf(external(cmd))]
    cmd: Cmd,
}

/// Resolve the local store path: `$DEPTOOL_STORE` if set, else `.deptool`.
fn store_path() -> PathBuf {
    std::env::var_os("DEPTOOL_STORE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".deptool"))
}

fn make_connector(mode: ConnectMode) -> Result<Box<dyn setup::HostConnector>> {
    let remote_store = PathBuf::from(prim::test_override(
        "DEPTOOL_REMOTE_STORE",
        "/var/lib/deptool/store",
    ));
    match mode {
        ConnectMode::Remote => Ok(Box::new(RemoteConnector::new(&remote_store)?)),
        ConnectMode::Local => {
            let remote_store = remote_store
                .to_str()
                .expect("remote store path is valid UTF-8");
            Ok(Box::new(LocalConnector {
                remote_store: remote_store.to_string(),
            }))
        }
    }
}

fn is_shell_safe(s: &str) -> bool {
    s.chars().all(|c| c.is_alphanumeric() || "/_.-".contains(c))
}

struct RemoteConnector {
    remote_store: String,
    remote_bin_path: String,
    binaries_dir: PathBuf,
    bin_name: String,
}

impl RemoteConnector {
    fn new(remote_store: &Path) -> Result<Self> {
        let remote_store = remote_store
            .to_str()
            .expect("remote store path is valid UTF-8")
            .to_string();

        // First 10 hex chars of the build commit. Long enough to avoid
        // collisions, short enough to keep paths and commands readable.
        // Stable across cross-compiled targets built from the same source,
        // so per-arch binaries from one release share a name on the host.
        let suffix = &setup::BUILD_COMMIT[..10];
        let bin_name = format!("deptool-{}-{suffix}", protocol::VERSION);
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
            binaries_dir: setup::binaries_dir(),
            bin_name,
        })
    }
}

impl setup::HostConnector for RemoteConnector {
    fn connect(&self, host: &Hostname) -> std::result::Result<Box<dyn Connection>, HostError> {
        assert!(
            is_shell_safe(&host.0),
            "hostname is free of shell metacharacters"
        );
        let mut cmd = setup::ssh_command();
        cmd.args([
            &host.0,
            "sudo",
            &self.remote_bin_path,
            "agent",
            &self.remote_store,
        ]);
        let session = deploy::RemoteSession::new(cmd)?;
        Ok(Box::new(session))
    }

    fn install(&self, host: &Hostname) -> std::result::Result<(), HostError> {
        setup::install_binary(
            &self.binaries_dir,
            &self.bin_name,
            &self.remote_bin_path,
            host,
        )
    }
}

struct LocalConnector {
    remote_store: String,
}

impl setup::HostConnector for LocalConnector {
    fn connect(&self, _host: &Hostname) -> std::result::Result<Box<dyn Connection>, HostError> {
        let mut cmd = Command::new(std::env::current_exe().expect("current exe path is known"));
        cmd.args(["agent", &self.remote_store]);
        let session = deploy::RemoteSession::new(cmd)?;
        Ok(Box::new(session))
    }

    fn install(&self, _host: &Hostname) -> std::result::Result<(), HostError> {
        Ok(())
    }
}

/// Last component of the config tree path, shown to the operator so they can
/// see which cluster is being deployed when the positional was omitted.
fn cluster_name(dir: &Path) -> &str {
    dir.file_name()
        .unwrap_or(dir.as_os_str())
        .to_str()
        .expect("cluster name is valid UTF-8")
}

/// Resolve the config tree directory, recording an explicit one as the default
/// for subsequent runs.
fn resolve_dir(store: &Store, dir: Option<PathBuf>) -> Result<PathBuf> {
    match dir {
        Some(d) => {
            store.set_default_cluster(&d)?;
            Ok(d)
        }
        None => store.get_default_cluster()?.ok_or(Error::NoDefaultCluster),
    }
}

fn run_deploy(
    store: PathBuf,
    dir: Option<PathBuf>,
    plan_only: bool,
    confirm_mode: ConfirmMode,
    mode: ConnectMode,
    filter: &HostFilter,
) -> Result<()> {
    let repo = Store::open(&store)?;
    let dir = resolve_dir(&repo, dir)?;
    let tree_oid = repo.build_tree(&dir)?;

    let plan = match plan::make_plan(&repo, tree_oid, filter)? {
        Some(draft) => draft.finalize(&repo)?,
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
        ConfirmMode::Prompt => display::confirm(&repo, &plan, cluster_name(&dir), color)?,
    };
    if let display::Decision::Abort = decision {
        return Ok(());
    }

    let connector = make_connector(mode)?;

    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
    let hostname = prim::read_hostname();
    let operator = format!("{user}@{hostname}");

    let hosts: Vec<_> = plan.hosts.keys().cloned().collect();
    let observer = display::StatusPrinter::new(display::UseColor::from_env());
    let progress = deploy::DeployProgress::new(hosts, Box::new(observer));

    let result = deploy::run_deploy(&repo, &plan, &operator, &*connector, &progress);

    // Per-host status lines have room for one phrase. Print explanation
    // blocks at the end for failure classes that need more space.
    for (description, items) in progress.explain_errors() {
        eprintln!();
        eprintln!("{description}");
        for item in &items {
            eprintln!();
            eprintln!("{item}");
        }
        eprintln!();
    }

    result
}

fn run_ping(
    store: PathBuf,
    dir: Option<PathBuf>,
    connect_mode: ConnectMode,
    filter: &HostFilter,
) -> Result<()> {
    let store = Store::open(&store)?;
    let dir = resolve_dir(&store, dir)?;
    let tree_oid = store.build_tree(&dir)?;
    let mut hosts_map = store.host_trees(tree_oid)?;
    filter.apply(&mut hosts_map)?;
    let hosts: Vec<_> = hosts_map.into_keys().collect();
    if hosts.is_empty() {
        eprintln!("No hosts to ping.");
        return Ok(());
    }
    let connector = make_connector(connect_mode)?;
    let observer = display::StatusPrinter::new(display::UseColor::from_env());
    let progress = deploy::DeployProgress::new(hosts.clone(), Box::new(observer));
    ping::run_ping(&hosts, &*connector, &progress);
    Ok(())
}

fn run_sync(
    store: PathBuf,
    dir: Option<PathBuf>,
    sync_mode: sync::SyncMode,
    connect_mode: ConnectMode,
    filter: &HostFilter,
) -> Result<()> {
    let store = Store::open(&store)?;
    let dir = resolve_dir(&store, dir)?;
    let hosts = sync::select_hosts_to_sync(&store, &dir, sync_mode, filter)?;
    if hosts.is_empty() {
        let hint = match sync_mode {
            sync::SyncMode::OnlyChanged => {
                "No hosts have config changes. Drop --changed to sync every host."
            }
            sync::SyncMode::AllHosts => "No hosts to sync.",
        };
        eprintln!("{hint}");
        return Ok(());
    }
    let connector = make_connector(connect_mode)?;
    let observer = display::StatusPrinter::new(display::UseColor::from_env());
    let progress = deploy::DeployProgress::new(hosts.keys().cloned().collect(), Box::new(observer));
    sync::run_sync(&store, &hosts, &*connector, &progress);
    Ok(())
}

fn run_status(store: PathBuf, dir: Option<PathBuf>, filter: &HostFilter) -> Result<()> {
    let store = Store::open(&store)?;
    let dir = resolve_dir(&store, dir)?;
    let states = status::compute_status(&store, &dir, filter)?;
    let short_len = status::min_unambiguous_short_len(&store, &states)?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    status::print_status(&mut out, &states, short_len, display::UseColor::from_env())?;
    Ok(())
}

fn run_diff(
    store: PathBuf,
    dir: Option<PathBuf>,
    mode: display::DiffMode,
    filter: &HostFilter,
) -> Result<()> {
    let store = Store::open(&store)?;
    let dir = resolve_dir(&store, dir)?;
    let tree_oid = store.build_tree(&dir)?;
    match plan::make_plan(&store, tree_oid, filter)? {
        Some(draft) => {
            // This creates a dangling commit in the store, but a GC will
            // collect it at some point, it's not worth complicating the code
            // to avoid this.
            let plan = draft.finalize(&store)?;
            display::print_diff(&store, &plan, mode, display::UseColor::from_env())?;
        }
        None => eprintln!("All hosts are up to date."),
    }
    Ok(())
}

fn run() -> Result<()> {
    // Register --version with bpaf (with only the long name, no -V) so it
    // appears in --help output. The flag is actually intercepted in main()
    // before bpaf runs, to avoid bpaf's hardcoded `Version: ` prefix.
    let args = args()
        .version(env!("CARGO_PKG_VERSION"))
        .version_parser(long("version").help("Print version and exit"))
        .run();

    let store = store_path();
    match args.cmd {
        Cmd::Deploy {
            plan_only,
            confirm_mode,
            limit,
            mode,
            dir,
        } => run_deploy(
            store,
            dir,
            plan_only,
            confirm_mode,
            mode,
            &HostFilter::from_limit(&limit),
        )?,
        Cmd::Sync {
            sync_mode,
            limit,
            connect_mode,
            dir,
        } => run_sync(
            store,
            dir,
            sync_mode,
            connect_mode,
            &HostFilter::from_limit(&limit),
        )?,
        Cmd::Ping {
            limit,
            connect_mode,
            dir,
        } => run_ping(store, dir, connect_mode, &HostFilter::from_limit(&limit))?,
        Cmd::Status { limit, dir } => {
            run_status(store, dir, &HostFilter::from_limit(&limit))?
        }
        Cmd::Diff { mode, limit, dir } => {
            run_diff(store, dir, mode, &HostFilter::from_limit(&limit))?
        }
        Cmd::Init { dir } => {
            let initialized = Store::open_or_init(&store)?;
            eprintln!("Initialized store at '{}'.", store.display());
            if let Some(dir) = dir {
                let already_existed = dir.exists();
                std::fs::create_dir_all(&dir)?;
                initialized.set_default_cluster(&dir)?;
                let verb = if already_existed { "Using" } else { "Created" };
                eprintln!(
                    "{verb} cluster directory '{}' and recorded it as the default.",
                    dir.display(),
                );
            }
        }
        Cmd::Agent { store } => run_agent(store)?,
    }

    Ok(())
}

use agent::AgentConfig;

fn activate(
    desired: &plan::DesiredState,
    changes: &plan::SystemDiff<PathBuf>,
    emit: &mut dyn FnMut(protocol::Message),
    log: &mut dyn FnMut(&str),
    apps_dir: &Path,
    unit_dir: &Path,
    sysusers_dir: &Path,
) -> std::result::Result<(), ApplyError> {
    // Reconcile manifest symlinks (e.g. config files in /etc) *before*
    // any systemd lifecycle operations, because units may depend on paths
    // that these symlinks provide.
    // TODO: log individual symlink changes.
    checkout::reconcile_config_symlinks(apps_dir, &changes.symlinks)?;

    // Reconcile sysusers symlinks and materialize users before starting
    // units, because units may run as those users. On app removal this
    // wastefully runs sysusers after unlinking the config, but it's
    // idempotent and not worth special-casing.
    checkout::reconcile_managed_symlinks(&desired.sysusers, apps_dir, sysusers_dir)?;
    if changes.sysusers.content_changed {
        // Invoke the binary directly -- the systemd-sysusers.service
        // unit has ConditionNeedsUpdate=/etc which skips execution
        // outside of early boot.
        log("running systemd-sysusers");
        let result = std::process::Command::new("systemd-sysusers")
            // Default console log level suppresses user creation messages.
            .env("SYSTEMD_LOG_LEVEL", "info")
            .output();
        match result {
            Ok(output) => {
                // Systemd tools log to stderr, not stdout.
                let text = String::from_utf8_lossy(&output.stderr);
                let trimmed = text.trim_end();
                if !trimmed.is_empty() {
                    emit(protocol::Message::SysusersOutput {
                        output: format!("systemd-sysusers:\n{trimmed}"),
                    });
                }
                if !output.status.success() {
                    return Err(ApplyError::SysusersActivationFailed);
                }
            }
            Err(err) => {
                emit(protocol::Message::SysusersOutput {
                    output: format!("systemd-sysusers: {err}"),
                });
                return Err(ApplyError::SysusersActivationFailed);
            }
        }
    }

    for unit in &changes.units.disable {
        log(&format!("disabling {unit}"));
        systemctl_ok(&["disable", "--now", unit]);
    }

    // Reconcile unit symlinks, then reload so systemd picks them up.
    // This runs after disable because systemd treats our symlinks as
    // "linked units" and `systemctl disable` removes the link itself,
    // not just the enablement symlinks. Reconciling here restores them
    // and also picks up any new units from the deploy.
    let symlink_changes = checkout::reconcile_managed_symlinks(&desired.units, apps_dir, unit_dir)?;

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
    let sysusers_dir = config.sysusers_dir.clone();
    let log_dir = config
        .apps_dir
        .parent()
        .unwrap_or(Path::new("/var/lib/deptool"));
    let log_path = log_dir.join("agent.log");
    agent::AgentSession::new(
        store,
        prim::Hostname(config.hostname.clone()),
        config.apps_dir.clone(),
        Box::new(move |desired, changes, emit, log: &mut dyn FnMut(&str)| {
            activate(
                desired,
                changes,
                emit,
                log,
                &apps_dir,
                &unit_dir,
                &sysusers_dir,
            )
        }),
        Some(log_path),
    )
}

fn run_agent(store: PathBuf) -> Result<()> {
    // Since we install the exact binary that the driver needs on
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
        build_commit: setup::BUILD_COMMIT.to_string(),
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
    Ok(())
}

fn main() {
    // Handled outside bpaf so that the output is just `deptool <version>`,
    // rather than bpaf's hardcoded `Version: ...` prefix.
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!(
            "Deptool {} ({}, {})",
            env!("CARGO_PKG_VERSION"),
            &setup::BUILD_COMMIT[..10],
            setup::BUILD_COMMIT_DATE,
        );
        return;
    }
    if let Err(e) = run() {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

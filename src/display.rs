//! Plan display and deploy confirmation prompt.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use git2::{Delta, Oid, Repository};

use crate::deploy::{DeployObserver, HostState};
use crate::error::Result;
use crate::plan::{AppDiff, Plan, SymlinkChanges, UnitChanges};
use crate::prim::Hostname;
use crate::store::Store;

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum UseColor {
    Yes,
    No,
}

impl UseColor {
    /// Respect the NO_COLOR convention: color is off when `NO_COLOR` is set.
    pub fn from_env() -> Self {
        match std::env::var_os("NO_COLOR") {
            Some(val) if !val.is_empty() => UseColor::No,
            _ => UseColor::Yes,
        }
    }

    fn green(self, text: &str) -> String {
        match self {
            UseColor::Yes => format!("\x1b[32m{text}\x1b[0m"),
            UseColor::No => text.to_string(),
        }
    }

    fn red(self, text: &str) -> String {
        match self {
            UseColor::Yes => format!("\x1b[31m{text}\x1b[0m"),
            UseColor::No => text.to_string(),
        }
    }

    fn yellow(self, text: &str) -> String {
        match self {
            UseColor::Yes => format!("\x1b[33m{text}\x1b[0m"),
            UseColor::No => text.to_string(),
        }
    }

    fn blue(self, text: &str) -> String {
        match self {
            UseColor::Yes => format!("\x1b[34m{text}\x1b[0m"),
            UseColor::No => text.to_string(),
        }
    }

    fn bold(self, text: &str) -> String {
        match self {
            UseColor::Yes => format!("\x1b[1m{text}\x1b[0m"),
            UseColor::No => text.to_string(),
        }
    }
}

/// The Git empty tree object, used as the base for diffs against new hosts.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Write the deployment plan as a diffstat.
pub fn print_plan(out: &mut impl Write, store: &Store, plan: &Plan, color: UseColor) -> Result<()> {
    for (host, host_plan) in &plan.hosts {
        write!(out, "{host}")?;
        if !host_plan.is_fast_forward {
            write!(out, " {}", color.red("(diverged)"))?;
        }
        if !host_plan.is_rollback_safe {
            write!(out, " {}", color.yellow("(rollback unavailable)"))?;
        }
        writeln!(out)?;
        for (app, app_plan) in &host_plan.apps {
            match &app_plan.diff {
                AppDiff::Add { new_tree } => {
                    writeln!(out, "    {} {app}", color.green("add"))?;
                    for (prefix, file) in diff_files(&store.repo, empty_tree_oid(), *new_tree)? {
                        writeln!(out, "        {} {file}", color_prefix(color, prefix))?;
                    }
                }
                AppDiff::Remove { .. } => {
                    writeln!(out, "    {} {app}", color.red("remove"))?;
                }
                AppDiff::Update { old_tree, new_tree } => {
                    writeln!(out, "    {} {app}", color.yellow("update"))?;
                    for (prefix, file) in diff_files(&store.repo, *old_tree, *new_tree)? {
                        writeln!(out, "        {} {file}", color_prefix(color, prefix))?;
                    }
                }
            }
            write_symlink_actions(out, &app_plan.system.symlinks, color)?;
            write_unit_actions(out, &app_plan.system.units, color)?;
            writeln!(out)?;
        }
    }
    Ok(())
}

fn color_prefix(color: UseColor, prefix: char) -> String {
    match prefix {
        '+' => color.green("+"),
        '-' => color.red("-"),
        _ => color.yellow("~"),
    }
}

pub enum Decision {
    Apply,
    Abort,
}

/// Show the confirmation prompt.
///
/// `d` shows the full file diff for all hosts in a single pager, then
/// re-shows the prompt. Enter or `N` aborts (the default).
pub fn confirm(store: &Store, plan: &Plan, store_path: &Path, color: UseColor) -> Result<Decision> {
    let all_rollback_safe = plan.hosts.values().all(|h| h.is_rollback_safe);
    if all_rollback_safe {
        println!("Auto-rollback if deploy fails.");
    } else {
        println!("{}", color.yellow("Rollback unavailable for some hosts."));
    }

    let diverged = plan.hosts.values().filter(|h| !h.is_fast_forward).count();
    if diverged > 0 {
        let noun = if diverged == 1 { "host" } else { "hosts" };
        println!(
            "This will {} to {diverged} {noun}, \
             which may inadvertently reverse previous changes!",
            color.bold("force-push"),
        );
    }

    let n = plan.hosts.len();
    let noun = if n == 1 { "host" } else { "hosts" };
    loop {
        print!("Apply to {n} {noun}? [y/N/d] ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        match input.trim() {
            "y" | "Y" => return Ok(Decision::Apply),
            "d" | "D" => show_diffs(store, plan, store_path, color)?,
            _ => return Ok(Decision::Abort),
        }
    }
}

/// Collect the full file diff for every host and pipe it through one pager.
///
/// Without this, each `git diff` invocation opens its own pager, so deploying
/// to N hosts means dismissing N pagers.
fn show_diffs(store: &Store, plan: &Plan, store_path: &Path, color: UseColor) -> Result<()> {
    println!();
    let mut combined = Vec::new();
    for (host, host_plan) in &plan.hosts {
        let old_oid = host_tree_oid(store, host_plan.expected_current.as_ref(), host)?;
        let new_oid = host_tree_oid(store, Some(&plan.commit), host)?;
        if !combined.is_empty() {
            writeln!(combined)?;
        }
        writeln!(combined, "{}", color.blue(&host.to_string()))?;
        let child = Command::new("git")
            .arg("--git-dir")
            .arg(store_path)
            .args(["diff", "--color=always"])
            .arg(old_oid.to_string())
            .arg(new_oid.to_string())
            .stdout(Stdio::piped())
            .spawn()?;
        let output = child.wait_with_output()?;
        combined.extend_from_slice(&output.stdout);
    }
    pipe_through_pager(&combined)
}

/// Pipe content through the user's pager ($PAGER, defaulting to `less`).
fn pipe_through_pager(content: &[u8]) -> Result<()> {
    let pager = std::env::var("PAGER").unwrap_or_else(|_| "less".into());
    let mut parts = pager.split_whitespace();
    let program = parts.next().expect("pager fallback is non-empty");
    let mut cmd = Command::new(program);
    cmd.args(parts).stdin(Stdio::piped());
    // The LESS env var sets default flags for less. F = quit if output fits
    // on one screen, R = pass through ANSI color codes, X = don't clear the
    // screen on exit. Without R, less strips the diff coloring.
    if std::env::var_os("LESS").is_none() {
        cmd.env("LESS", "FRX");
    }
    let mut child = cmd.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        // Ignore broken pipe -- the user may quit the pager early.
        let _ = stdin.write_all(content);
    }
    child.wait()?;
    Ok(())
}

/// Get the tree oid for a host's subtree from a given commit, or the empty tree.
fn host_tree_oid(store: &Store, commit: Option<&Oid>, host: &Hostname) -> Result<Oid> {
    match commit {
        None => Ok(empty_tree_oid()),
        Some(oid) => {
            let tree = store.get_commit_tree(*oid)?;
            match tree.get_name(&host.0) {
                Some(tree) => Ok(tree.id()),
                None => Ok(empty_tree_oid()),
            }
        }
    }
}

fn empty_tree_oid() -> Oid {
    Oid::from_str(EMPTY_TREE).expect("empty tree oid is a hardcoded valid hex string")
}

/// Print unit actions in execution order: disable, unlink, link, enable, restart.
fn write_unit_actions(out: &mut impl Write, units: &UnitChanges, color: UseColor) -> Result<()> {
    for unit in &units.disable {
        writeln!(out, "        {} {unit}", color.red("disable"))?;
    }
    for unit in &units.unlink {
        writeln!(out, "        {} {unit}", color.red("unlink"))?;
    }
    for unit in &units.link {
        writeln!(out, "        {} {unit}", color.green("link"))?;
    }
    for unit in &units.enable {
        writeln!(out, "        {} {unit}", color.green("enable"))?;
    }
    for unit in &units.restart {
        writeln!(out, "        {} {unit}", color.yellow("restart"))?;
    }
    Ok(())
}

/// Print symlink actions in execution order: removes, changes, creates.
fn write_symlink_actions(
    out: &mut impl Write,
    changes: &SymlinkChanges<String>,
    color: UseColor,
) -> Result<()> {
    for link in &changes.remove {
        writeln!(out, "        {} {link}", color.red("unlink"))?;
    }
    for (link, source) in &changes.change {
        writeln!(out, "        {} {link}", color.red("unlink"))?;
        writeln!(out, "        {} {link} -> {source}", color.green("link"))?;
    }
    for (link, source) in &changes.create {
        writeln!(out, "        {} {link} -> {source}", color.green("link"))?;
    }
    Ok(())
}

/// Diff two app trees, returning (prefix_char, filename) pairs.
fn diff_files(repo: &Repository, old_oid: Oid, new_oid: Oid) -> Result<Vec<(char, String)>> {
    let old_tree = repo.find_tree(old_oid)?;
    let new_tree = repo.find_tree(new_oid)?;
    let diff = repo.diff_tree_to_tree(Some(&old_tree), Some(&new_tree), None)?;

    let mut changes = Vec::new();
    diff.foreach(
        &mut |delta, _| {
            let prefix = match delta.status() {
                Delta::Added => '+',
                Delta::Deleted => '-',
                _ => '~',
            };
            let path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .and_then(|p| p.to_str())
                .unwrap_or("?")
                .to_string();
            changes.push((prefix, path));
            true
        },
        None, // binary callback
        None, // hunk callback
        None, // line callback
    )?;
    Ok(changes)
}

/// Renders deploy progress as a live-updating block on a terminal.
pub struct StatusPrinter {
    color: UseColor,
    rendered: bool,
    /// Width of the widest line in the previous render.
    ///
    /// We pad lines with spaces to this width instead of using erase escapes
    /// like `\x1b[K`. Erase escapes fill up to the terminal width with
    /// spaces, and when the terminal is later resized narrower, those
    /// trailing spaces cause the status block to wrap and misrender.
    prev_width: usize,
}

impl StatusPrinter {
    pub fn new(color: UseColor) -> Self {
        Self {
            color,
            rendered: false,
            prev_width: 0,
        }
    }

    pub fn print(&mut self, states: &BTreeMap<Hostname, HostState>) {
        self.render(&mut io::stdout(), states)
            .expect("stdout is writable");
    }

    fn erase_status_block(&mut self, out: &mut impl Write, n: usize) -> Result<()> {
        if self.rendered {
            // We print one blank line before the status.
            for _ in 0..n + 1 {
                write!(out, "\x1b[1A\r{:w$}", "", w = self.prev_width)?;
                write!(out, "\r")?;
            }
            self.prev_width = 0;
            self.rendered = false;
        }
        Ok(())
    }

    fn render(
        &mut self,
        out: &mut impl Write,
        states: &BTreeMap<Hostname, HostState>,
    ) -> Result<()> {
        if self.rendered {
            // Move cursor up to overwrite previous output.
            let n = states.len();
            write!(out, "\x1b[{n}A")?;
        } else {
            // Ensure a blank line before the statuses to separate it visually
            // from other log output.
            writeln!(out)?;
        }
        let name_width = states.keys().map(|h| h.0.len()).max().unwrap_or(0);
        let mut max_width = 0_usize;
        for (host, state) in states {
            let label = format!("{host}:");
            let state_str = self.color_state(state);
            // Visible width: "  " + label (padded) + " " + state text.
            let visible_len = 2 + name_width + 1 + 1 + state.to_string().len();
            let pad = self.prev_width.saturating_sub(visible_len);
            writeln!(
                out,
                "\r  {label:<width$} {state_str}{:pad$}",
                "",
                width = name_width + 1,
            )?;
            max_width = max_width.max(visible_len);
        }
        self.prev_width = max_width;
        out.flush()?;
        self.rendered = true;
        Ok(())
    }

    fn color_state(&self, state: &HostState) -> String {
        match state {
            HostState::Done => self.color.green(&state.to_string()),
            HostState::Failed(_)
            | HostState::RolledBack(_)
            | HostState::RollbackFailed { .. }
            | HostState::Stale
            | HostState::LockBusy(_) => self.color.red(&state.to_string()),
            _ => self.color.yellow(&state.to_string()),
        }
    }

    fn render_log_message(
        &mut self,
        out: &mut impl Write,
        states: &BTreeMap<Hostname, HostState>,
        host: &Hostname,
        text: &str,
    ) -> Result<()> {
        self.erase_status_block(out, states.len())?;
        let header = self.color.bold(&format!("{host}:"));
        write!(out, "\n{header}\n{text}")?;
        if !text.ends_with('\n') {
            writeln!(out)?;
        }
        self.render(out, states)
    }

    /// Render to a buffer, for testing.
    #[cfg(test)]
    fn render_to_string(&mut self, states: &BTreeMap<Hostname, HostState>) -> String {
        let mut buf = Vec::new();
        self.render(&mut buf, states)
            .expect("writing to Vec never fails");
        String::from_utf8(buf).expect("output is utf-8")
    }
}

impl DeployObserver for StatusPrinter {
    fn state_changed(&mut self, states: &BTreeMap<Hostname, HostState>) {
        self.print(states);
    }

    fn log_message(&mut self, states: &BTreeMap<Hostname, HostState>, host: &Hostname, text: &str) {
        self.render_log_message(&mut io::stdout(), states, host, text)
            .expect("stdout is writable");
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use super::*;
    use crate::error::Result;
    use crate::plan::{AppDiff, HostPlan, Plan, compute_app_plan};
    use crate::prim::Hostname;
    use crate::testutil::TestRepo;

    fn app_tree_oid(repo: &Repository, commit_oid: Oid, host: &str, app: &str) -> Oid {
        repo.find_commit(commit_oid)
            .unwrap()
            .tree()
            .unwrap()
            .get_path(Path::new(host).join(app).as_ref())
            .expect("app subtree exists in commit")
            .id()
    }

    fn render(store: &Store, plan: &Plan) -> Result<String> {
        let mut out = Vec::new();
        print_plan(&mut out, store, plan, UseColor::No)?;
        Ok(String::from_utf8(out).expect("output is utf-8"))
    }

    #[test]
    fn added_app_lists_filenames() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/nginx.conf", b"server {}\n")]);
        let new_tree = app_tree_oid(&t.store.repo, c1, "web1", "nginx");
        let plan = Plan {
            commit: c1,
            hosts: BTreeMap::from([(
                Hostname::from("web1"),
                HostPlan {
                    apps: BTreeMap::from([(
                        "nginx".into(),
                        compute_app_plan(&t.store, AppDiff::Add { new_tree })?,
                    )]),
                    expected_current: None,
                    is_fast_forward: true,
                    is_rollback_safe: true,
                },
            )]),
        };
        assert_eq!(
            render(&t.store, &plan)?,
            "\
web1
    add nginx
        + nginx.conf

",
        );
        Ok(())
    }

    #[test]
    fn added_app_with_systemd_shows_enable_action() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[
            ("web1/nginx/nginx.conf", b"server {}\n"),
            (
                "web1/nginx/manifest.json",
                br#"{"systemd":{"units_enabled":["nginx.service"]}}"#,
            ),
        ]);
        let new_tree = app_tree_oid(&t.store.repo, c1, "web1", "nginx");
        let plan = Plan {
            commit: c1,
            hosts: BTreeMap::from([(
                Hostname::from("web1"),
                HostPlan {
                    apps: BTreeMap::from([(
                        "nginx".into(),
                        compute_app_plan(&t.store, AppDiff::Add { new_tree })?,
                    )]),
                    expected_current: None,
                    is_fast_forward: true,
                    is_rollback_safe: true,
                },
            )]),
        };
        assert_eq!(
            render(&t.store, &plan)?,
            "\
web1
    add nginx
        + manifest.json
        + nginx.conf
        enable nginx.service

",
        );
        Ok(())
    }

    #[test]
    fn added_app_shows_link_for_non_enabled_units() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[
            ("web1/nginx/nginx.conf", b"server {}\n"),
            ("web1/nginx/systemd/nginx.service", b"[Service]"),
            ("web1/nginx/systemd/nginx-reload.timer", b"[Timer]"),
            (
                "web1/nginx/manifest.json",
                br#"{"systemd":{"units_enabled":["nginx.service"]}}"#,
            ),
        ]);
        let new_tree = app_tree_oid(&t.store.repo, c1, "web1", "nginx");
        let plan = Plan {
            commit: c1,
            hosts: BTreeMap::from([(
                Hostname::from("web1"),
                HostPlan {
                    apps: BTreeMap::from([(
                        "nginx".into(),
                        compute_app_plan(&t.store, AppDiff::Add { new_tree })?,
                    )]),
                    expected_current: None,
                    is_fast_forward: true,
                    is_rollback_safe: true,
                },
            )]),
        };
        assert_eq!(
            render(&t.store, &plan)?,
            "\
web1
    add nginx
        + manifest.json
        + nginx.conf
        + systemd/nginx-reload.timer
        + systemd/nginx.service
        link nginx-reload.timer
        link nginx.service
        enable nginx.service

",
        );
        Ok(())
    }

    #[test]
    fn removed_app_shows_unlink_for_non_enabled_units() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[
            ("web1/nginx/systemd/nginx.service", b"[Service]"),
            ("web1/nginx/systemd/nginx-reload.timer", b"[Timer]"),
            (
                "web1/nginx/manifest.json",
                br#"{"systemd":{"units_enabled":["nginx.service"]}}"#,
            ),
        ]);
        let old_tree = app_tree_oid(&t.store.repo, c1, "web1", "nginx");
        let plan = Plan {
            commit: c1,
            hosts: BTreeMap::from([(
                Hostname::from("web1"),
                HostPlan {
                    apps: BTreeMap::from([(
                        "nginx".into(),
                        compute_app_plan(&t.store, AppDiff::Remove { old_tree })?,
                    )]),
                    expected_current: Some(c1),
                    is_fast_forward: true,
                    is_rollback_safe: true,
                },
            )]),
        };
        assert_eq!(
            render(&t.store, &plan)?,
            "\
web1
    remove nginx
        disable nginx.service
        unlink nginx-reload.timer
        unlink nginx.service

",
        );
        Ok(())
    }

    #[test]
    fn removed_app_shows_disable_action() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[
            ("web1/nginx/nginx.conf", b"server {}\n"),
            (
                "web1/nginx/manifest.json",
                br#"{"systemd":{"units_enabled":["nginx.service"]}}"#,
            ),
        ]);
        let old_tree = app_tree_oid(&t.store.repo, c1, "web1", "nginx");
        let plan = Plan {
            commit: c1,
            hosts: BTreeMap::from([(
                Hostname::from("web1"),
                HostPlan {
                    apps: BTreeMap::from([(
                        "nginx".into(),
                        compute_app_plan(&t.store, AppDiff::Remove { old_tree })?,
                    )]),
                    expected_current: Some(c1),
                    is_fast_forward: true,
                    is_rollback_safe: true,
                },
            )]),
        };
        assert_eq!(
            render(&t.store, &plan)?,
            "\
web1
    remove nginx
        disable nginx.service

",
        );
        Ok(())
    }

    #[test]
    fn updated_app_shows_changed_files() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/nginx.conf", b"v1")]);
        let c2 = t.commit(&[("web1/nginx/nginx.conf", b"v2")]);
        let old_tree = app_tree_oid(&t.store.repo, c1, "web1", "nginx");
        let new_tree = app_tree_oid(&t.store.repo, c2, "web1", "nginx");
        let plan = Plan {
            commit: c2,
            hosts: BTreeMap::from([(
                Hostname::from("web1"),
                HostPlan {
                    apps: BTreeMap::from([(
                        "nginx".into(),
                        compute_app_plan(&t.store, AppDiff::Update { old_tree, new_tree })?,
                    )]),
                    expected_current: Some(c1),
                    is_fast_forward: true,
                    is_rollback_safe: true,
                },
            )]),
        };
        assert_eq!(
            render(&t.store, &plan)?,
            "\
web1
    update nginx
        ~ nginx.conf

",
        );
        Ok(())
    }

    #[test]
    fn updated_app_with_unchanged_unit_shows_restart_action() -> Result<()> {
        let t = TestRepo::new();
        let manifest = br#"{"systemd":{"units_enabled":["nginx.service"]}}"#;
        let c1 = t.commit(&[
            ("web1/nginx/nginx.conf", b"v1"),
            ("web1/nginx/manifest.json", manifest),
        ]);
        let c2 = t.commit(&[
            ("web1/nginx/nginx.conf", b"v2"),
            ("web1/nginx/manifest.json", manifest),
        ]);
        let old_tree = app_tree_oid(&t.store.repo, c1, "web1", "nginx");
        let new_tree = app_tree_oid(&t.store.repo, c2, "web1", "nginx");
        let plan = Plan {
            commit: c2,
            hosts: BTreeMap::from([(
                Hostname::from("web1"),
                HostPlan {
                    apps: BTreeMap::from([(
                        "nginx".into(),
                        compute_app_plan(&t.store, AppDiff::Update { old_tree, new_tree })?,
                    )]),
                    expected_current: Some(c1),
                    is_fast_forward: true,
                    is_rollback_safe: true,
                },
            )]),
        };
        assert_eq!(
            render(&t.store, &plan)?,
            "\
web1
    update nginx
        ~ nginx.conf
        restart nginx.service

",
        );
        Ok(())
    }

    #[test]
    fn diverged_host_shows_warning() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/nginx.conf", b"v1")]);
        let new_tree = app_tree_oid(&t.store.repo, c1, "web1", "nginx");
        let plan = Plan {
            commit: c1,
            hosts: BTreeMap::from([(
                Hostname::from("web1"),
                HostPlan {
                    apps: BTreeMap::from([(
                        "nginx".into(),
                        compute_app_plan(&t.store, AppDiff::Add { new_tree })?,
                    )]),
                    expected_current: None,
                    is_fast_forward: false,
                    is_rollback_safe: true,
                },
            )]),
        };
        assert_eq!(
            render(&t.store, &plan)?,
            "\
web1 (diverged)
    add nginx
        + nginx.conf

",
        );
        Ok(())
    }

    #[test]
    fn rollback_unavailable_host_shows_warning() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/nginx.conf", b"v1")]);
        let new_tree = app_tree_oid(&t.store.repo, c1, "web1", "nginx");
        let plan = Plan {
            commit: c1,
            hosts: BTreeMap::from([(
                Hostname::from("web1"),
                HostPlan {
                    apps: BTreeMap::from([(
                        "nginx".into(),
                        compute_app_plan(&t.store, AppDiff::Add { new_tree })?,
                    )]),
                    expected_current: None,
                    is_fast_forward: true,
                    is_rollback_safe: false,
                },
            )]),
        };
        assert_eq!(
            render(&t.store, &plan)?,
            "\
web1 (rollback unavailable)
    add nginx
        + nginx.conf

",
        );
        Ok(())
    }

    #[test]
    fn added_app_with_symlinks_shows_symlink_action() -> Result<()> {
        let t = TestRepo::new();
        let manifest = br#"{"symlinks": {"/etc/nginx/nginx.conf": "nginx.conf"}}"#;
        let c1 = t.commit(&[
            ("web1/nginx/nginx.conf", b"server {}"),
            ("web1/nginx/manifest.json", manifest),
        ]);
        let new_tree = app_tree_oid(&t.store.repo, c1, "web1", "nginx");
        let plan = Plan {
            commit: c1,
            hosts: BTreeMap::from([(
                Hostname::from("web1"),
                HostPlan {
                    apps: BTreeMap::from([(
                        "nginx".into(),
                        compute_app_plan(&t.store, AppDiff::Add { new_tree })?,
                    )]),
                    expected_current: None,
                    is_fast_forward: true,
                    is_rollback_safe: true,
                },
            )]),
        };
        assert_eq!(
            render(&t.store, &plan)?,
            "\
web1
    add nginx
        + manifest.json
        + nginx.conf
        link /etc/nginx/nginx.conf -> nginx.conf

",
        );
        Ok(())
    }

    #[test]
    fn symlink_actions_printed_before_unit_actions() -> Result<()> {
        let t = TestRepo::new();
        let manifest = br#"{
            "systemd": {"units_enabled": ["nginx.service"]},
            "symlinks": {"/etc/nginx/nginx.conf": "nginx.conf"}
        }"#;
        let c1 = t.commit(&[
            ("web1/nginx/nginx.conf", b"server {}"),
            ("web1/nginx/manifest.json", manifest),
        ]);
        let new_tree = app_tree_oid(&t.store.repo, c1, "web1", "nginx");
        let plan = Plan {
            commit: c1,
            hosts: BTreeMap::from([(
                Hostname::from("web1"),
                HostPlan {
                    apps: BTreeMap::from([(
                        "nginx".into(),
                        compute_app_plan(&t.store, AppDiff::Add { new_tree })?,
                    )]),
                    expected_current: None,
                    is_fast_forward: true,
                    is_rollback_safe: true,
                },
            )]),
        };
        assert_eq!(
            render(&t.store, &plan)?,
            "\
web1
    add nginx
        + manifest.json
        + nginx.conf
        link /etc/nginx/nginx.conf -> nginx.conf
        enable nginx.service

",
        );
        Ok(())
    }

    #[test]
    fn changed_symlink_shows_unlink_then_link_paired() -> Result<()> {
        let t = TestRepo::new();
        let m1 = br#"{"symlinks": {"/etc/nginx/nginx.conf": "old.conf"}}"#;
        let m2 = br#"{"symlinks": {"/etc/nginx/nginx.conf": "new.conf"}}"#;
        let c1 = t.commit(&[
            ("web1/nginx/old.conf", b"v1"),
            ("web1/nginx/manifest.json", m1),
        ]);
        let c2 = t.commit(&[
            ("web1/nginx/new.conf", b"v2"),
            ("web1/nginx/manifest.json", m2),
        ]);
        let old_tree = app_tree_oid(&t.store.repo, c1, "web1", "nginx");
        let new_tree = app_tree_oid(&t.store.repo, c2, "web1", "nginx");
        let plan = Plan {
            commit: c2,
            hosts: BTreeMap::from([(
                Hostname::from("web1"),
                HostPlan {
                    apps: BTreeMap::from([(
                        "nginx".into(),
                        compute_app_plan(&t.store, AppDiff::Update { old_tree, new_tree })?,
                    )]),
                    expected_current: Some(c1),
                    is_fast_forward: true,
                    is_rollback_safe: true,
                },
            )]),
        };
        assert_eq!(
            render(&t.store, &plan)?,
            "\
web1
    update nginx
        ~ manifest.json
        + new.conf
        - old.conf
        unlink /etc/nginx/nginx.conf
        link /etc/nginx/nginx.conf -> new.conf

",
        );
        Ok(())
    }

    #[test]
    fn removed_app_with_symlinks_shows_unlink_action() -> Result<()> {
        let t = TestRepo::new();
        let manifest = br#"{"symlinks": {"/etc/nginx/nginx.conf": "nginx.conf"}}"#;
        let c1 = t.commit(&[
            ("web1/nginx/nginx.conf", b"server {}"),
            ("web1/nginx/manifest.json", manifest),
        ]);
        let old_tree = app_tree_oid(&t.store.repo, c1, "web1", "nginx");
        let plan = Plan {
            commit: c1,
            hosts: BTreeMap::from([(
                Hostname::from("web1"),
                HostPlan {
                    apps: BTreeMap::from([(
                        "nginx".into(),
                        compute_app_plan(&t.store, AppDiff::Remove { old_tree })?,
                    )]),
                    expected_current: Some(c1),
                    is_fast_forward: true,
                    is_rollback_safe: true,
                },
            )]),
        };
        assert_eq!(
            render(&t.store, &plan)?,
            "\
web1
    remove nginx
        unlink /etc/nginx/nginx.conf

",
        );
        Ok(())
    }

    #[test]
    fn status_printer_initial_render() {
        let states = BTreeMap::from([
            (Hostname::from("web1"), HostState::Connecting),
            (Hostname::from("web2"), HostState::Connecting),
        ]);
        let mut printer = StatusPrinter::new(UseColor::No);
        assert_eq!(
            printer.render_to_string(&states),
            "\n\
             \r  web1: connecting\n\
             \r  web2: connecting\n",
        );
    }

    #[test]
    fn status_printer_second_render_moves_cursor_up() {
        let mut printer = StatusPrinter::new(UseColor::No);
        let states = BTreeMap::from([
            (Hostname::from("web1"), HostState::Connecting),
            (Hostname::from("web2"), HostState::Connecting),
        ]);
        printer.render_to_string(&states);
        let states = BTreeMap::from([
            (Hostname::from("web1"), HostState::Locked),
            (Hostname::from("web2"), HostState::Connecting),
        ]);
        assert_eq!(
            printer.render_to_string(&states),
            "\x1b[2A\
             \r  web1: locked    \n\
             \r  web2: connecting\n",
        );
    }

    #[test]
    fn status_printer_colors_states() {
        let states = BTreeMap::from([
            (Hostname::from("web1"), HostState::Done),
            (Hostname::from("web2"), HostState::Stale),
        ]);
        let mut printer = StatusPrinter::new(UseColor::Yes);
        assert_eq!(
            printer.render_to_string(&states),
            "\n\
             \r  web1: \x1b[32mdone\x1b[0m\n\
             \r  web2: \x1b[31mstale\x1b[0m\n",
        );
    }

    #[test]
    fn status_printer_aligns_hostnames() {
        let states = BTreeMap::from([
            (Hostname::from("frontend"), HostState::Locked),
            (Hostname::from("backend"), HostState::Connecting),
        ]);
        let mut printer = StatusPrinter::new(UseColor::No);
        assert_eq!(
            printer.render_to_string(&states),
            "\n\
             \r  backend:  connecting\n\
             \r  frontend: locked\n",
        );
    }

    #[test]
    fn log_message_erases_status_block_and_re_renders() {
        let mut printer = StatusPrinter::new(UseColor::No);
        let states = BTreeMap::from([
            (Hostname::from("web1"), HostState::Applying),
            (Hostname::from("web2"), HostState::Done),
        ]);
        printer.render_to_string(&states);

        let mut buf = Vec::new();
        printer
            .render_log_message(
                &mut buf,
                &states,
                &Hostname::from("web1"),
                "app.service activated",
            )
            .expect("render succeeds");
        let output = String::from_utf8(buf).expect("output is utf-8");

        // erase_status_block: 3 lines (blank + 2 hosts), each overwritten
        // with spaces to prev_width (16 = "  web1: applying").
        // Then log message, then re-render with prev_width reset to 0.
        assert_eq!(
            output,
            "\x1b[1A\r                \r\
             \x1b[1A\r                \r\
             \x1b[1A\r                \r\
             \nweb1:\napp.service activated\n\
             \n\
             \r  web1: applying\n\
             \r  web2: done\n",
        );
    }
}

//! Plan display and deploy confirmation prompt.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;

use git2::{Delta, Oid, Repository};

use crate::deploy::{DeployObserver, HostState};
use crate::error::Result;
use crate::plan::{AppDiff, Plan, UnitChanges, diff_enabled};
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
        if host_plan.is_fast_forward {
            writeln!(out, "{host}")?;
        } else {
            writeln!(out, "{host} {}", color.red("(diverged)"))?;
        }
        for (app, diff) in &host_plan.apps {
            match diff {
                AppDiff::Add { new_tree } => {
                    writeln!(out, "  {} {app}", color.green("+"))?;
                    for (prefix, file) in diff_files(&store.repo, empty_tree_oid(), *new_tree)? {
                        writeln!(out, "      {} {file}", color_prefix(color, prefix))?;
                    }
                    let link = store.app_units(*new_tree)?;
                    let enabled = store.enabled_units(*new_tree)?;
                    let units = diff_enabled(&BTreeSet::new(), &enabled);
                    write_unit_actions(out, &units, &link, &BTreeSet::new(), color)?;
                }
                AppDiff::Remove { old_tree } => {
                    writeln!(out, "  {} {app}", color.red("-"))?;
                    let unlink = store.app_units(*old_tree)?;
                    let enabled = store.enabled_units(*old_tree)?;
                    let units = diff_enabled(&enabled, &BTreeSet::new());
                    write_unit_actions(out, &units, &BTreeSet::new(), &unlink, color)?;
                }
                AppDiff::Update { old_tree, new_tree } => {
                    writeln!(out, "  {} {app}", color.yellow("~"))?;
                    for (prefix, file) in diff_files(&store.repo, *old_tree, *new_tree)? {
                        writeln!(out, "      {} {file}", color_prefix(color, prefix))?;
                    }
                    let old_all = store.app_units(*old_tree)?;
                    let new_all = store.app_units(*new_tree)?;
                    let link: BTreeSet<_> = new_all.difference(&old_all).cloned().collect();
                    let unlink: BTreeSet<_> = old_all.difference(&new_all).cloned().collect();
                    let old_enabled = store.enabled_units(*old_tree)?;
                    let new_enabled = store.enabled_units(*new_tree)?;
                    let units = diff_enabled(&old_enabled, &new_enabled);
                    write_unit_actions(out, &units, &link, &unlink, color)?;
                }
            }
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
/// `d` pages through the full file diff for each host sequentially, then
/// re-shows the prompt. Enter or `N` aborts (the default).
pub fn confirm(store: &Store, plan: &Plan, store_path: &Path, color: UseColor) -> Result<Decision> {
    println!();

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

/// Open a pager with the full file diff for each host in the plan.
fn show_diffs(store: &Store, plan: &Plan, store_path: &Path, color: UseColor) -> Result<()> {
    for (host, host_plan) in &plan.hosts {
        let old_oid = host_tree_oid(store, host_plan.expected_current.as_ref(), host)?;
        let new_oid = host_tree_oid(store, Some(&plan.commit), host)?;
        println!("\n{}", color.blue(&host.to_string()));
        Command::new("git")
            .arg("--git-dir")
            .arg(store_path)
            .args(["diff", "--color=always"])
            .arg(old_oid.to_string())
            .arg(new_oid.to_string())
            .status()?;
    }
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
fn write_unit_actions(
    out: &mut impl Write,
    units: &UnitChanges,
    link: &BTreeSet<String>,
    unlink: &BTreeSet<String>,
    color: UseColor,
) -> Result<()> {
    for unit in &units.disable {
        writeln!(out, "      {} {unit}", color.red("disable"))?;
    }
    for unit in unlink {
        writeln!(out, "      {} {unit}", color.red("unlink"))?;
    }
    for unit in link {
        writeln!(out, "      {} {unit}", color.green("link"))?;
    }
    for unit in &units.enable {
        writeln!(out, "      {} {unit}", color.green("enable"))?;
    }
    for unit in &units.restart {
        writeln!(out, "      {} {unit}", color.yellow("restart"))?;
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
}

impl StatusPrinter {
    pub fn new(color: UseColor) -> Self {
        Self {
            color,
            rendered: false,
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
                write!(out, "\x1b[1A\x1b[2K")?;
            }
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
        for (host, state) in states {
            let label = format!("{host}:");
            write!(out, "\x1b[2K")?;
            writeln!(
                out,
                "  {label:<width$} {}",
                self.color_state(state),
                width = name_width + 1,
            )?;
        }
        out.flush()?;
        self.rendered = true;
        Ok(())
    }

    fn color_state(&self, state: &HostState) -> String {
        match state {
            HostState::Done => self.color.green(&state.to_string()),
            HostState::Failed(_) | HostState::Stale | HostState::LockBusy(_) => {
                self.color.red(&state.to_string())
            }
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
    use crate::plan::{AppDiff, HostPlan, Plan};
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
    fn added_app_shows_plus_prefix_with_filenames() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/nginx.conf", b"server {}\n")]);
        let new_tree = app_tree_oid(&t.store.repo, c1, "web1", "nginx");
        let plan = Plan {
            commit: c1,
            hosts: BTreeMap::from([(
                Hostname::from("web1"),
                HostPlan {
                    apps: BTreeMap::from([("nginx".into(), AppDiff::Add { new_tree })]),
                    expected_current: None,
                    is_fast_forward: true,
                },
            )]),
        };
        assert_eq!(
            render(&t.store, &plan)?,
            "\
web1
  + nginx
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
                    apps: BTreeMap::from([("nginx".into(), AppDiff::Add { new_tree })]),
                    expected_current: None,
                    is_fast_forward: true,
                },
            )]),
        };
        assert_eq!(
            render(&t.store, &plan)?,
            "\
web1
  + nginx
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
                    apps: BTreeMap::from([("nginx".into(), AppDiff::Add { new_tree })]),
                    expected_current: None,
                    is_fast_forward: true,
                },
            )]),
        };
        assert_eq!(
            render(&t.store, &plan)?,
            "\
web1
  + nginx
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
                    apps: BTreeMap::from([("nginx".into(), AppDiff::Remove { old_tree })]),
                    expected_current: Some(c1),
                    is_fast_forward: true,
                },
            )]),
        };
        assert_eq!(
            render(&t.store, &plan)?,
            "\
web1
  - nginx
      disable nginx.service
      unlink nginx-reload.timer
      unlink nginx.service
",
        );
        Ok(())
    }

    #[test]
    fn removed_app_shows_minus_prefix_with_disable_action() -> Result<()> {
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
                    apps: BTreeMap::from([("nginx".into(), AppDiff::Remove { old_tree })]),
                    expected_current: Some(c1),
                    is_fast_forward: true,
                },
            )]),
        };
        assert_eq!(
            render(&t.store, &plan)?,
            "\
web1
  - nginx
      disable nginx.service
",
        );
        Ok(())
    }

    #[test]
    fn updated_app_shows_tilde_prefix_with_changed_files() -> Result<()> {
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
                        AppDiff::Update { old_tree, new_tree },
                    )]),
                    expected_current: Some(c1),
                    is_fast_forward: true,
                },
            )]),
        };
        assert_eq!(
            render(&t.store, &plan)?,
            "\
web1
  ~ nginx
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
                        AppDiff::Update { old_tree, new_tree },
                    )]),
                    expected_current: Some(c1),
                    is_fast_forward: true,
                },
            )]),
        };
        assert_eq!(
            render(&t.store, &plan)?,
            "\
web1
  ~ nginx
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
                    apps: BTreeMap::from([("nginx".into(), AppDiff::Add { new_tree })]),
                    expected_current: None,
                    is_fast_forward: false,
                },
            )]),
        };
        assert_eq!(
            render(&t.store, &plan)?,
            "\
web1 (diverged)
  + nginx
      + nginx.conf
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
            "
\x1b[2K  web1: connecting
\x1b[2K  web2: connecting
",
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
            "\
\x1b[2A\x1b[2K  web1: locked
\x1b[2K  web2: connecting
",
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
            "
\x1b[2K  web1: \x1b[32mdone\x1b[0m
\x1b[2K  web2: \x1b[31mstale\x1b[0m
",
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
            "
\x1b[2K  backend:  connecting
\x1b[2K  frontend: locked
",
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

        assert_eq!(
            output,
            "\
\x1b[1A\x1b[2K\x1b[1A\x1b[2K\x1b[1A\x1b[2K
web1:
app.service activated

\x1b[2K  web1: applying\n\
\x1b[2K  web2: done\n",
        );
    }
}

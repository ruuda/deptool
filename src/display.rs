//! Plan display and deploy confirmation prompt.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;

use git2::Repository;

use crate::deploy::HostState;
use crate::error::Result;
use crate::plan::{AppDiff, Plan, UnitChanges, app_enabled_units, diff_enabled};
use crate::prim::{Hostname, Oid};

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
}

/// The Git empty tree object, used as the base for diffs against new hosts.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Write the deployment plan as a diffstat.
pub fn print_plan(
    out: &mut impl Write,
    repo: &Repository,
    plan: &Plan,
    color: UseColor,
) -> Result<()> {
    for (host, host_plan) in &plan.hosts {
        writeln!(out, "{host}")?;
        for (app, diff) in &host_plan.apps {
            match diff {
                AppDiff::Add { new_tree } => {
                    writeln!(out, "  {} {app}", color.green("+"))?;
                    for (prefix, file) in
                        diff_files(repo, empty_tree_oid(), git2::Oid::from(new_tree))?
                    {
                        writeln!(out, "      {} {file}", color_prefix(color, prefix))?;
                    }
                    let units = diff_enabled(
                        &BTreeSet::new(),
                        &app_enabled_units(repo, git2::Oid::from(new_tree))?,
                    );
                    write_unit_actions(out, &units, color)?;
                }
                AppDiff::Remove { old_tree } => {
                    writeln!(out, "  {} {app}", color.red("-"))?;
                    let units = diff_enabled(
                        &app_enabled_units(repo, git2::Oid::from(old_tree))?,
                        &BTreeSet::new(),
                    );
                    write_unit_actions(out, &units, color)?;
                }
                AppDiff::Update { old_tree, new_tree } => {
                    writeln!(out, "  {} {app}", color.yellow("~"))?;
                    for (prefix, file) in
                        diff_files(repo, git2::Oid::from(old_tree), git2::Oid::from(new_tree))?
                    {
                        writeln!(out, "      {} {file}", color_prefix(color, prefix))?;
                    }
                    let units = diff_enabled(
                        &app_enabled_units(repo, git2::Oid::from(old_tree))?,
                        &app_enabled_units(repo, git2::Oid::from(new_tree))?,
                    );
                    write_unit_actions(out, &units, color)?;
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
pub fn confirm(repo: &Repository, plan: &Plan, store: &Path, color: UseColor) -> Result<Decision> {
    let n = plan.hosts.len();
    let noun = if n == 1 { "host" } else { "hosts" };
    loop {
        print!("\nApply to {n} {noun}? [y/N/d] ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        match input.trim() {
            "y" | "Y" => return Ok(Decision::Apply),
            "d" | "D" => show_diffs(repo, plan, store, color)?,
            _ => return Ok(Decision::Abort),
        }
    }
}

/// Open a pager with the full file diff for each host in the plan.
fn show_diffs(repo: &Repository, plan: &Plan, store: &Path, color: UseColor) -> Result<()> {
    for (host, host_plan) in &plan.hosts {
        let old_oid = host_tree_oid(repo, host_plan.expected_current.as_ref(), host)?;
        let new_oid = host_tree_oid(repo, Some(&plan.commit), host)?;
        println!("\n{}", color.blue(&host.to_string()));
        Command::new("git")
            .arg("--git-dir")
            .arg(store)
            .args(["diff", "--color=always"])
            .arg(old_oid.to_string())
            .arg(new_oid.to_string())
            .status()?;
    }
    Ok(())
}

/// Get the tree oid for a host's subtree from a given commit, or the empty tree.
fn host_tree_oid(repo: &Repository, commit: Option<&Oid>, host: &Hostname) -> Result<git2::Oid> {
    match commit {
        None => Ok(empty_tree_oid()),
        Some(oid) => {
            let tree = repo.find_commit(git2::Oid::from(oid))?.tree()?;
            match tree.get_name(&host.0) {
                Some(tree) => Ok(tree.id()),
                None => Ok(empty_tree_oid()),
            }
        }
    }
}

fn empty_tree_oid() -> git2::Oid {
    git2::Oid::from_str(EMPTY_TREE).expect("empty tree oid is a hardcoded valid hex string")
}

fn write_unit_actions(out: &mut impl Write, units: &UnitChanges, color: UseColor) -> Result<()> {
    for unit in &units.enable {
        writeln!(out, "      {} {unit}", color.green("enable"))?;
    }
    for unit in &units.restart {
        writeln!(out, "      {} {unit}", color.yellow("restart"))?;
    }
    for unit in &units.disable {
        writeln!(out, "      {} {unit}", color.red("disable"))?;
    }
    Ok(())
}

/// Diff two app trees, returning (prefix_char, filename) pairs.
fn diff_files(
    repo: &Repository,
    old_oid: git2::Oid,
    new_oid: git2::Oid,
) -> Result<Vec<(char, String)>> {
    let old_tree = repo.find_tree(old_oid)?;
    let new_tree = repo.find_tree(new_oid)?;
    let diff = repo.diff_tree_to_tree(Some(&old_tree), Some(&new_tree), None)?;

    let mut changes = Vec::new();
    diff.foreach(
        &mut |delta, _| {
            let prefix = match delta.status() {
                git2::Delta::Added => '+',
                git2::Delta::Deleted => '-',
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

    fn render(
        &mut self,
        out: &mut impl Write,
        states: &BTreeMap<Hostname, HostState>,
    ) -> Result<()> {
        let n = states.len();
        if self.rendered {
            // Move cursor up to overwrite previous output.
            write!(out, "\x1b[{n}A")?;
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
            HostState::Failed(_) | HostState::Stale | HostState::LockBusy => {
                self.color.red(&state.to_string())
            }
            _ => self.color.yellow(&state.to_string()),
        }
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use super::*;
    use crate::error::Result;
    use crate::plan::{AppDiff, HostPlan, Plan};
    use crate::prim::{Hostname, Oid};
    use crate::testutil::TestRepo;

    fn app_tree_oid(
        repo: &git2::Repository,
        commit_oid: git2::Oid,
        host: &str,
        app: &str,
    ) -> git2::Oid {
        repo.find_commit(commit_oid)
            .unwrap()
            .tree()
            .unwrap()
            .get_path(Path::new(host).join(app).as_ref())
            .expect("app subtree exists in commit")
            .id()
    }

    fn render(repo: &git2::Repository, plan: &Plan) -> Result<String> {
        let mut out = Vec::new();
        print_plan(&mut out, repo, plan, UseColor::No)?;
        Ok(String::from_utf8(out).expect("output is utf-8"))
    }

    #[test]
    fn added_app_shows_plus_prefix_with_filenames() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/nginx.conf", b"server {}\n")]);
        let new_tree: Oid = app_tree_oid(&t.repo, c1, "web1", "nginx").into();
        let plan = Plan {
            commit: c1.into(),
            hosts: BTreeMap::from([(
                Hostname::from("web1"),
                HostPlan {
                    apps: BTreeMap::from([("nginx".into(), AppDiff::Add { new_tree })]),
                    expected_current: None,
                },
            )]),
        };
        assert_eq!(
            render(&t.repo, &plan)?,
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
                "web1/nginx/systemd.json",
                br#"{"units_enabled":["nginx.service"]}"#,
            ),
        ]);
        let new_tree: Oid = app_tree_oid(&t.repo, c1, "web1", "nginx").into();
        let plan = Plan {
            commit: c1.into(),
            hosts: BTreeMap::from([(
                Hostname::from("web1"),
                HostPlan {
                    apps: BTreeMap::from([("nginx".into(), AppDiff::Add { new_tree })]),
                    expected_current: None,
                },
            )]),
        };
        assert_eq!(
            render(&t.repo, &plan)?,
            "\
web1
  + nginx
      + nginx.conf
      + systemd.json
      enable nginx.service
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
                "web1/nginx/systemd.json",
                br#"{"units_enabled":["nginx.service"]}"#,
            ),
        ]);
        let old_tree: Oid = app_tree_oid(&t.repo, c1, "web1", "nginx").into();
        let plan = Plan {
            commit: c1.into(),
            hosts: BTreeMap::from([(
                Hostname::from("web1"),
                HostPlan {
                    apps: BTreeMap::from([("nginx".into(), AppDiff::Remove { old_tree })]),
                    expected_current: Some(c1.into()),
                },
            )]),
        };
        assert_eq!(
            render(&t.repo, &plan)?,
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
        let old_tree: Oid = app_tree_oid(&t.repo, c1, "web1", "nginx").into();
        let new_tree: Oid = app_tree_oid(&t.repo, c2, "web1", "nginx").into();
        let plan = Plan {
            commit: c2.into(),
            hosts: BTreeMap::from([(
                Hostname::from("web1"),
                HostPlan {
                    apps: BTreeMap::from([(
                        "nginx".into(),
                        AppDiff::Update { old_tree, new_tree },
                    )]),
                    expected_current: Some(c1.into()),
                },
            )]),
        };
        assert_eq!(
            render(&t.repo, &plan)?,
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
        let systemd_json = br#"{"units_enabled":["nginx.service"]}"#;
        let c1 = t.commit(&[
            ("web1/nginx/nginx.conf", b"v1"),
            ("web1/nginx/systemd.json", systemd_json),
        ]);
        let c2 = t.commit(&[
            ("web1/nginx/nginx.conf", b"v2"),
            ("web1/nginx/systemd.json", systemd_json),
        ]);
        let old_tree: Oid = app_tree_oid(&t.repo, c1, "web1", "nginx").into();
        let new_tree: Oid = app_tree_oid(&t.repo, c2, "web1", "nginx").into();
        let plan = Plan {
            commit: c2.into(),
            hosts: BTreeMap::from([(
                Hostname::from("web1"),
                HostPlan {
                    apps: BTreeMap::from([(
                        "nginx".into(),
                        AppDiff::Update { old_tree, new_tree },
                    )]),
                    expected_current: Some(c1.into()),
                },
            )]),
        };
        assert_eq!(
            render(&t.repo, &plan)?,
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
    fn status_printer_initial_render() {
        let states = BTreeMap::from([
            (Hostname::from("web1"), HostState::Connecting),
            (Hostname::from("web2"), HostState::Connecting),
        ]);
        let mut printer = StatusPrinter::new(UseColor::No);
        assert_eq!(
            printer.render_to_string(&states),
            "\
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
            "\
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
            "\
\x1b[2K  backend:  connecting
\x1b[2K  frontend: locked
",
        );
    }
}

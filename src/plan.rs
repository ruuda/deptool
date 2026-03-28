use std::collections::BTreeMap;
use std::fmt;

use git2::Repository;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::oid::Oid;
use crate::store::get_host_apps;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Hostname(pub String);

impl From<&str> for Hostname {
    fn from(s: &str) -> Self {
        Hostname(s.to_string())
    }
}

impl fmt::Display for Hostname {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AppDiff {
    Add { new_tree: Oid },
    Remove,
    Update { old_tree: Oid, new_tree: Oid },
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostPlan {
    pub apps: BTreeMap<String, AppDiff>,
    pub expected_current: Option<Oid>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub hosts: BTreeMap<Hostname, HostPlan>,
    pub commit: Oid,
}

/// Diff two sets of app tree oids for a single host.
pub fn diff_apps(
    current: &BTreeMap<String, git2::Oid>,
    target: &BTreeMap<String, git2::Oid>,
) -> BTreeMap<String, AppDiff> {
    let mut changes = BTreeMap::new();

    for (name, target_oid) in target {
        match current.get(name) {
            None => {
                changes.insert(
                    name.clone(),
                    AppDiff::Add {
                        new_tree: (*target_oid).into(),
                    },
                );
            }
            Some(cur_oid) if cur_oid != target_oid => {
                changes.insert(
                    name.clone(),
                    AppDiff::Update {
                        old_tree: (*cur_oid).into(),
                        new_tree: (*target_oid).into(),
                    },
                );
            }
            Some(_) => {}
        }
    }

    for name in current.keys() {
        if !target.contains_key(name) {
            changes.insert(name.clone(), AppDiff::Remove);
        }
    }

    changes
}

/// Build a deployment plan by comparing main against each host's current ref.
///
/// TODO: Currently this is based only on the repository state, which means we
/// need to fetch the remote refs ahead of time. We should split this into two
/// stages: first eliminate hosts that we definitely do not need to touch based
/// on current refs. Then for hosts that do need touching we refresh their refs,
/// and plan again. We could just use the same plan function for that though.
pub fn make_plan(repo: &Repository) -> Result<Plan> {
    let main_commit = repo.find_reference("refs/heads/main")?.peel_to_commit()?;
    let commit = main_commit.id();
    let main_tree = main_commit.tree()?;

    let mut hosts = BTreeMap::new();

    for entry in main_tree.iter() {
        let host = Hostname(entry.name().expect("tree entry name is utf-8").to_string());

        let target_apps = get_host_apps(repo, &main_tree, &host.0)?;

        let (expected_current, current_apps) =
            match repo.find_reference(&format!("refs/remotes/{host}/current")) {
                Err(_) => (None, BTreeMap::new()),
                Ok(r) => {
                    let c = r.peel_to_commit()?;
                    let tree = c.tree()?;
                    (Some(c.id().into()), get_host_apps(repo, &tree, &host.0)?)
                }
            };

        let apps = diff_apps(&current_apps, &target_apps);

        if !apps.is_empty() {
            hosts.insert(
                host,
                HostPlan {
                    apps,
                    expected_current,
                },
            );
        }
    }

    Ok(Plan {
        hosts,
        commit: commit.into(),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use git2::Repository;

    use super::*;
    use crate::error::Result;
    use crate::store::{RefUpdate, set_ref};
    use crate::testutil::{TempDir, commit_dir};

    #[test]
    fn plan_shows_all_apps_as_add_for_new_host() -> Result<()> {
        let input = TempDir::new("input");
        fs::create_dir_all(input.path().join("web1/nginx"))?;
        fs::write(input.path().join("web1/nginx/conf"), "a")?;
        fs::create_dir_all(input.path().join("web1/rofld"))?;
        fs::write(input.path().join("web1/rofld/conf"), "b")?;

        let store = TempDir::new("store");
        let repo = Repository::init_bare(store.path())?;
        commit_dir(&repo, input.path())?;

        let plan = make_plan(&repo)?;
        assert_eq!(plan.hosts.len(), 1);
        let apps = &plan.hosts[&"web1".into()].apps;
        assert_eq!(apps.len(), 2);
        assert!(matches!(apps["rofld"], AppDiff::Add { .. }));
        assert!(matches!(apps["nginx"], AppDiff::Add { .. }));
        Ok(())
    }

    #[test]
    fn plan_detects_updated_and_unchanged_apps() -> Result<()> {
        let input = TempDir::new("input");
        fs::create_dir_all(input.path().join("web1/nginx"))?;
        fs::write(input.path().join("web1/nginx/conf"), "v1")?;
        fs::create_dir_all(input.path().join("web1/rofld"))?;
        fs::write(input.path().join("web1/rofld/conf"), "v1")?;

        let store = TempDir::new("store");
        let repo = Repository::init_bare(store.path())?;
        let c1 = commit_dir(&repo, input.path())?;

        set_ref(
            &repo,
            "refs/remotes/web1/current",
            c1,
            RefUpdate::SetCurrent,
        )?;

        fs::write(input.path().join("web1/nginx/conf"), "v2")?;
        commit_dir(&repo, input.path())?;

        let plan = make_plan(&repo)?;
        let apps = &plan.hosts[&"web1".into()].apps;
        assert_eq!(apps.len(), 1);
        assert!(matches!(apps["nginx"], AppDiff::Update { .. }));
        Ok(())
    }

    #[test]
    fn plan_detects_removed_apps() -> Result<()> {
        let input = TempDir::new("input");
        fs::create_dir_all(input.path().join("web1/nginx"))?;
        fs::write(input.path().join("web1/nginx/conf"), "a")?;
        fs::create_dir_all(input.path().join("web1/rofld"))?;
        fs::write(input.path().join("web1/rofld/conf"), "b")?;

        let store = TempDir::new("store");
        let repo = Repository::init_bare(store.path())?;
        let c1 = commit_dir(&repo, input.path())?;
        set_ref(
            &repo,
            "refs/remotes/web1/current",
            c1,
            RefUpdate::SetCurrent,
        )?;

        fs::remove_dir_all(input.path().join("web1/rofld"))?;
        commit_dir(&repo, input.path())?;

        let plan = make_plan(&repo)?;
        assert_eq!(
            plan.hosts[&"web1".into()].apps,
            BTreeMap::from([("rofld".into(), AppDiff::Remove)]),
        );
        Ok(())
    }

    #[test]
    fn plan_includes_new_host_alongside_up_to_date_host() -> Result<()> {
        let input = TempDir::new("input");
        fs::create_dir_all(input.path().join("web1/nginx"))?;
        fs::write(input.path().join("web1/nginx/conf"), "a")?;

        let store = TempDir::new("store");
        let repo = Repository::init_bare(store.path())?;
        let c1 = commit_dir(&repo, input.path())?;
        set_ref(
            &repo,
            "refs/remotes/web1/current",
            c1,
            RefUpdate::SetCurrent,
        )?;

        fs::create_dir_all(input.path().join("web2/rofld"))?;
        fs::write(input.path().join("web2/rofld/conf"), "b")?;
        commit_dir(&repo, input.path())?;

        let plan = make_plan(&repo)?;
        assert!(!plan.hosts.contains_key(&"web1".into()));
        let apps = &plan.hosts[&"web2".into()].apps;
        assert_eq!(apps.len(), 1);
        assert!(matches!(apps["rofld"], AppDiff::Add { .. }));
        Ok(())
    }

    #[test]
    fn plan_omits_hosts_that_are_up_to_date() -> Result<()> {
        let input = TempDir::new("input");
        fs::create_dir_all(input.path().join("web1/nginx"))?;
        fs::write(input.path().join("web1/nginx/conf"), "a")?;

        let store = TempDir::new("store");
        let repo = Repository::init_bare(store.path())?;
        let c1 = commit_dir(&repo, input.path())?;
        set_ref(
            &repo,
            "refs/remotes/web1/current",
            c1,
            RefUpdate::SetCurrent,
        )?;

        commit_dir(&repo, input.path())?;

        let plan = make_plan(&repo)?;
        assert!(plan.hosts.is_empty());
        Ok(())
    }
}

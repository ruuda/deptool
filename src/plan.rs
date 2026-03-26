use std::fmt;
use std::collections::BTreeMap;

use git2::Repository;

use crate::error::Result;
use crate::store::get_host_profiles;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
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

#[derive(Debug, PartialEq, Eq)]
pub enum ProfileDiff {
    Add {
        new_tree: git2::Oid,
    },
    Remove,
    Update {
        old_tree: git2::Oid,
        new_tree: git2::Oid,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub struct HostPlan {
    pub profiles: BTreeMap<String, ProfileDiff>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    pub hosts: BTreeMap<Hostname, HostPlan>,
}

/// Diff two sets of profile tree oids for a single host.
pub fn diff_profiles(
    current: &BTreeMap<String, git2::Oid>,
    target: &BTreeMap<String, git2::Oid>,
) -> BTreeMap<String, ProfileDiff> {
    let mut changes = BTreeMap::new();

    for (name, target_oid) in target {
        match current.get(name) {
            None => {
                changes.insert(
                    name.clone(),
                    ProfileDiff::Add {
                        new_tree: *target_oid,
                    },
                );
            }
            Some(cur_oid) if cur_oid != target_oid => {
                changes.insert(
                    name.clone(),
                    ProfileDiff::Update {
                        old_tree: *cur_oid,
                        new_tree: *target_oid,
                    },
                );
            }
            Some(_) => {}
        }
    }

    for name in current.keys() {
        if !target.contains_key(name) {
            changes.insert(name.clone(), ProfileDiff::Remove);
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
    let main_tree = main_commit.tree()?;

    let mut hosts = BTreeMap::new();

    for entry in main_tree.iter() {
        let host = Hostname(entry.name().expect("tree entry name is utf-8").to_string());

        let target_profiles = get_host_profiles(repo, &main_tree, &host.0)?;

        let current_profiles = match repo.find_reference(&format!("refs/remotes/{host}/current")) {
            Err(_) => BTreeMap::new(),
            Ok(r) => {
                let tree = r.peel_to_commit()?.tree()?;
                get_host_profiles(repo, &tree, &host.0)?
            }
        };

        let profiles = diff_profiles(&current_profiles, &target_profiles);

        if !profiles.is_empty() {
            hosts.insert(host, HostPlan { profiles });
        }
    }

    Ok(Plan { hosts })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use git2::Repository;

    use super::*;
    use crate::error::Result;
    use crate::store::{RefUpdate, set_ref};
    use crate::testutil::{commit_dir, TempDir};

    #[test]
    fn plan_shows_all_profiles_as_add_for_new_host() -> Result<()> {
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
        let profiles = &plan.hosts[&"web1".into()].profiles;
        assert_eq!(profiles.len(), 2);
        assert!(matches!(profiles["rofld"], ProfileDiff::Add { .. }));
        assert!(matches!(profiles["nginx"], ProfileDiff::Add { .. }));
        Ok(())
    }

    #[test]
    fn plan_detects_updated_and_unchanged_profiles() -> Result<()> {
        let input = TempDir::new("input");
        fs::create_dir_all(input.path().join("web1/nginx"))?;
        fs::write(input.path().join("web1/nginx/conf"), "v1")?;
        fs::create_dir_all(input.path().join("web1/rofld"))?;
        fs::write(input.path().join("web1/rofld/conf"), "v1")?;

        let store = TempDir::new("store");
        let repo = Repository::init_bare(store.path())?;
        let c1 = commit_dir(&repo, input.path())?;

        set_ref(&repo, "refs/remotes/web1/current", c1, RefUpdate::SetCurrent)?;

        fs::write(input.path().join("web1/nginx/conf"), "v2")?;
        commit_dir(&repo, input.path())?;

        let plan = make_plan(&repo)?;
        let profiles = &plan.hosts[&"web1".into()].profiles;
        assert_eq!(profiles.len(), 1);
        assert!(matches!(profiles["nginx"], ProfileDiff::Update { .. }));
        Ok(())
    }

    #[test]
    fn plan_detects_removed_profiles() -> Result<()> {
        let input = TempDir::new("input");
        fs::create_dir_all(input.path().join("web1/nginx"))?;
        fs::write(input.path().join("web1/nginx/conf"), "a")?;
        fs::create_dir_all(input.path().join("web1/rofld"))?;
        fs::write(input.path().join("web1/rofld/conf"), "b")?;

        let store = TempDir::new("store");
        let repo = Repository::init_bare(store.path())?;
        let c1 = commit_dir(&repo, input.path())?;
        set_ref(&repo, "refs/remotes/web1/current", c1, RefUpdate::SetCurrent)?;

        fs::remove_dir_all(input.path().join("web1/rofld"))?;
        commit_dir(&repo, input.path())?;

        let plan = make_plan(&repo)?;
        assert_eq!(
            plan.hosts[&"web1".into()].profiles,
            BTreeMap::from([("rofld".into(), ProfileDiff::Remove)]),
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
        set_ref(&repo, "refs/remotes/web1/current", c1, RefUpdate::SetCurrent)?;

        fs::create_dir_all(input.path().join("web2/rofld"))?;
        fs::write(input.path().join("web2/rofld/conf"), "b")?;
        commit_dir(&repo, input.path())?;

        let plan = make_plan(&repo)?;
        assert!(!plan.hosts.contains_key(&"web1".into()));
        let profiles = &plan.hosts[&"web2".into()].profiles;
        assert_eq!(profiles.len(), 1);
        assert!(matches!(profiles["rofld"], ProfileDiff::Add { .. }));
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
        set_ref(&repo, "refs/remotes/web1/current", c1, RefUpdate::SetCurrent)?;

        commit_dir(&repo, input.path())?;

        let plan = make_plan(&repo)?;
        assert!(plan.hosts.is_empty());
        Ok(())
    }
}

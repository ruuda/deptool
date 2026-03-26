use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use bpaf::{Bpaf, Parser};
use git2::Repository;

#[derive(Debug, Clone, Bpaf)]
enum Cmd {
    /// Record a directory as a new commit in the store.
    #[bpaf(command)]
    Commit {
        /// Path to the bare Git store.
        #[bpaf(positional("STORE"))]
        store: PathBuf,
        /// Directory to commit.
        #[bpaf(positional("DIR"))]
        dir: PathBuf,
    },
    /// Check out a profile for a host from a commit.
    #[bpaf(command)]
    Apply {
        /// Path to the bare Git store.
        #[bpaf(positional("STORE"))]
        store: PathBuf,
        /// Commit hash to check out from.
        #[bpaf(positional("COMMIT"))]
        commit: String,
        /// Host name.
        #[bpaf(positional("HOST"))]
        host: String,
        /// Profile name.
        #[bpaf(positional("PROFILE"))]
        profile: String,
        /// Target directory to check out into.
        #[bpaf(positional("TARGET"))]
        target: PathBuf,
    },
}

#[derive(Debug)]
enum Error {
    Io(std::io::Error),
    Git(git2::Error),
    NonUtf8FileName,
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<git2::Error> for Error {
    fn from(e: git2::Error) -> Self {
        Error::Git(e)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Git(e) => write!(f, "{e}"),
            Error::NonUtf8FileName => write!(f, "non-utf8 file name"),
        }
    }
}

type Result<T> = std::result::Result<T, Error>;

/// Recursively build a Git tree from a directory on disk.
fn build_tree(repo: &Repository, dir: &Path) -> Result<git2::Oid> {
    let mut tb = repo.treebuilder(None)?;

    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<std::result::Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let name = entry.file_name();
        let name = name.to_str().ok_or(Error::NonUtf8FileName)?;

        match entry.file_type()? {
            ft if ft.is_dir() => {
                let oid = build_tree(repo, &entry.path())?;
                tb.insert(name, oid, 0o040000)?;
            }
            ft if ft.is_file() => {
                let contents = fs::read(entry.path())?;
                let oid = repo.blob(&contents)?;
                tb.insert(name, oid, 0o100644)?;
            }
            _ => panic!("Unsupported directory entry: {name}"),
        }
    }

    Ok(tb.write()?)
}

fn commit_tree(repo: &Repository, tree_oid: git2::Oid) -> Result<git2::Oid> {
    let tree = repo.find_tree(tree_oid)?;
    let sig = repo.signature()?;

    let parent = repo
        .find_reference("refs/heads/main")
        .ok()
        .map(|r| r.peel_to_commit())
        .transpose()?;
    let parents: Vec<&git2::Commit> = parent.iter().collect();

    Ok(repo.commit(
        Some("refs/heads/main"),
        &sig,
        &sig,
        "Update cluster state",
        &tree,
        &parents,
    )?)
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Hostname(String);

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
enum ProfileDiff {
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
struct HostPlan {
    profiles: BTreeMap<String, ProfileDiff>,
}

#[derive(Debug, PartialEq, Eq)]
struct Plan {
    hosts: BTreeMap<Hostname, HostPlan>,
}

/// Get the tree entries (name -> oid) one level deep.
fn tree_entries(tree: &git2::Tree) -> BTreeMap<String, git2::Oid> {
    let mut entries = BTreeMap::new();
    for entry in tree.iter() {
        if let Some(name) = entry.name() {
            entries.insert(name.to_string(), entry.id());
        }
    }
    entries
}

/// Diff two sets of profile tree oids for a single host.
fn diff_profiles(
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
fn make_plan(repo: &Repository) -> Result<Plan> {
    let main_commit = repo.find_reference("refs/heads/main")?.peel_to_commit()?;
    let main_tree = main_commit.tree()?;

    let mut hosts = BTreeMap::new();

    for entry in main_tree.iter() {
        let host = Hostname(entry.name().expect("tree entry name is utf-8").to_string());

        let target_host_tree = repo.find_tree(entry.id())?;
        let target_profiles = tree_entries(&target_host_tree);

        let current_profiles = match repo.find_reference(&format!("refs/remotes/{host}/current")) {
            Err(_) => BTreeMap::new(),
            Ok(r) => {
                let tree = r.peel_to_commit()?.tree()?;
                match tree.get_name(&host.0) {
                    None => BTreeMap::new(),
                    Some(e) => tree_entries(&repo.find_tree(e.id())?),
                }
            }
        };

        let profiles = diff_profiles(&current_profiles, &target_profiles);

        if !profiles.is_empty() {
            hosts.insert(host, HostPlan { profiles });
        }
    }

    Ok(Plan { hosts })
}

/// Check out a subtree (host/profile) from a commit into a target directory.
fn apply(
    repo: &Repository,
    commit_oid: git2::Oid,
    host: &str,
    profile: &str,
    target: &Path,
) -> Result<()> {
    let commit = repo.find_commit(commit_oid)?;
    let tree = commit.tree()?;
    let entry = tree.get_path(Path::new(host).join(profile).as_ref())?;
    let subtree = repo.find_tree(entry.id())?;
    let mut cb = git2::build::CheckoutBuilder::new();
    cb.target_dir(target);
    repo.checkout_tree(subtree.as_object(), Some(&mut cb))?;
    Ok(())
}

fn run() -> Result<()> {
    let cmd = cmd().to_options().run();

    match cmd {
        Cmd::Commit { store, dir } => {
            let repo = match Repository::open(&store) {
                Ok(r) => r,
                Err(_) => Repository::init_bare(&store)?,
            };
            let tree_oid = build_tree(&repo, &dir)?;
            let commit_oid = commit_tree(&repo, tree_oid)?;
            println!("{commit_oid}");
        }
        Cmd::Apply {
            store,
            commit,
            host,
            profile,
            target,
        } => {
            let repo = Repository::open(&store)?;
            let oid = git2::Oid::from_str(&commit)?;
            apply(&repo, oid, &host, &profile, &target)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let dir = std::env::temp_dir().join(format!("deptool-test-{pid}-{id}-{label}"));
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Recursively collect all files under a directory as (relative_path, contents).
    fn read_dir_recursive(root: &Path, dir: &Path) -> Result<Vec<(String, Vec<u8>)>> {
        let mut result = Vec::new();
        let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<std::result::Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let ft = entry.file_type()?;
            if ft.is_dir() {
                result.extend(read_dir_recursive(root, &entry.path())?);
            } else if ft.is_file() {
                let rel = entry
                    .path()
                    .strip_prefix(root)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string();
                let contents = fs::read(entry.path())?;
                result.push((rel, contents));
            }
        }
        Ok(result)
    }

    #[test]
    fn apply_after_commit_reproduces_the_profile_files() -> Result<()> {
        let input = TempDir::new("input");
        fs::create_dir_all(input.path().join("web1/nginx/etc/nginx"))?;
        fs::write(
            input.path().join("web1/nginx/etc/nginx/nginx.conf"),
            "server {}",
        )?;
        fs::create_dir_all(input.path().join("web1/rofld/etc/rofld"))?;
        fs::write(
            input.path().join("web1/rofld/etc/rofld/config.toml"),
            "[server]\nport = 8080\n",
        )?;

        let store = TempDir::new("store");
        let repo = Repository::init_bare(store.path())?;

        let tree_oid = build_tree(&repo, input.path())?;
        let commit_oid = commit_tree(&repo, tree_oid)?;

        let output = TempDir::new("output");
        apply(&repo, commit_oid, "web1", "nginx", output.path())?;

        let out = output.path();
        let expected = input.path().join("web1/nginx");
        assert_eq!(
            read_dir_recursive(out, out)?,
            read_dir_recursive(&expected, &expected)?,
        );
        Ok(())
    }

    fn commit_dir(repo: &Repository, dir: &Path) -> Result<git2::Oid> {
        let tree_oid = build_tree(repo, dir)?;
        commit_tree(repo, tree_oid)
    }

    fn set_ref(repo: &Repository, refname: &str, oid: git2::Oid) -> Result<()> {
        repo.reference(refname, oid, true, "")?;
        Ok(())
    }

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

        // We don't have a ref for host `web1` yet, so everything is added.
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

        // Now we advance the `current` ref for host `web1` to indicate that
        // that config has been deployed.
        set_ref(&repo, "refs/remotes/web1/current", c1)?;

        // We update nginx but not rofld.
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
        set_ref(&repo, "refs/remotes/web1/current", c1)?;

        // Remove rofld from the tree.
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
        set_ref(&repo, "refs/remotes/web1/current", c1)?;

        // Add a second host.
        fs::create_dir_all(input.path().join("web2/rofld"))?;
        fs::write(input.path().join("web2/rofld/conf"), "b")?;
        commit_dir(&repo, input.path())?;

        // The profile is added on the second host, the first host remains
        // unaffected.
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
        set_ref(&repo, "refs/remotes/web1/current", c1)?;

        // Commit again with identical content, main moves but tree is the same.
        commit_dir(&repo, input.path())?;

        let plan = make_plan(&repo)?;
        assert!(plan.hosts.is_empty());
        Ok(())
    }

    #[test]
    fn commit_appends_to_main_branch() -> Result<()> {
        let input = TempDir::new("input");
        fs::create_dir_all(input.path().join("web1/app"))?;
        fs::write(input.path().join("web1/app/config"), "v1")?;

        let store = TempDir::new("store");
        let repo = Repository::init_bare(store.path())?;

        let t1 = build_tree(&repo, input.path())?;
        let c1 = commit_tree(&repo, t1)?;

        fs::write(input.path().join("web1/app/config"), "v2")?;
        let t2 = build_tree(&repo, input.path())?;
        let c2 = commit_tree(&repo, t2)?;

        let commit = repo.find_commit(c2)?;
        assert_eq!(commit.parent_count(), 1);
        assert_eq!(commit.parent_id(0)?, c1);
        Ok(())
    }
}

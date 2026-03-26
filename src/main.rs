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

/// Check out a subtree (host/profile) from a commit into a target directory.
fn apply(repo: &Repository, commit_oid: git2::Oid, host: &str, profile: &str, target: &Path) -> Result<()> {
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
        Cmd::Apply { store, commit, host, profile, target } => {
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
                let rel = entry.path().strip_prefix(root).unwrap().to_str().unwrap().to_string();
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
        fs::write(input.path().join("web1/nginx/etc/nginx/nginx.conf"), "server {}")?;
        fs::create_dir_all(input.path().join("web1/myapp/etc/myapp"))?;
        fs::write(input.path().join("web1/myapp/etc/myapp/config.toml"), "[server]\nport = 8080\n")?;

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
}

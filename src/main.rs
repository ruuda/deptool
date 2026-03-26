use std::fmt;
use std::fs;
use std::path::Path;

use git2::Repository;

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

    Ok(repo.commit(
        Some("refs/heads/main"),
        &sig,
        &sig,
        "Update cluster state",
        &tree,
        &[],
    )?)
}

fn run() -> Result<()> {
    let store_path = std::env::args()
        .nth(1)
        .expect("usage: deptool <store-repo> <dir>");
    let dir = std::env::args()
        .nth(2)
        .expect("usage: deptool <store-repo> <dir>");

    let repo = match Repository::open(&store_path) {
        Ok(r) => r,
        Err(_) => Repository::init_bare(&store_path)?,
    };

    let tree_oid = build_tree(&repo, Path::new(&dir))?;
    let commit_oid = commit_tree(&repo, tree_oid)?;
    println!("{commit_oid}");
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

    /// Checkout a commit's tree into a directory.
    fn checkout_to(repo: &Repository, commit_oid: git2::Oid, target: &Path) -> Result<()> {
        let commit = repo.find_commit(commit_oid)?;
        let tree = commit.tree()?;
        let mut cb = git2::build::CheckoutBuilder::new();
        cb.target_dir(target);
        repo.checkout_tree(tree.as_object(), Some(&mut cb))?;
        Ok(())
    }

    /// Recursively collect all files under a directory as (relative_path, contents).
    fn read_tree(root: &Path) -> Result<Vec<(String, Vec<u8>)>> {
        let mut result = Vec::new();
        read_tree_rec(root, root, &mut result)?;
        result.sort();
        Ok(result)
    }

    fn read_tree_rec(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) -> Result<()> {
        let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<std::result::Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let ft = entry.file_type()?;
            if ft.is_dir() {
                read_tree_rec(root, &entry.path(), out)?;
            } else if ft.is_file() {
                let rel = entry.path().strip_prefix(root).unwrap().to_str().unwrap().to_string();
                let contents = fs::read(entry.path())?;
                out.push((rel, contents));
            }
        }
        Ok(())
    }

    #[test]
    fn checkout_after_commit_reproduces_the_original_files() -> Result<()> {
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
        checkout_to(&repo, commit_oid, output.path())?;

        assert_eq!(read_tree(input.path())?, read_tree(output.path())?);
        Ok(())
    }
}

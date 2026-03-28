mod error;
mod plan;
mod store;

#[cfg(test)]
mod testutil;

use std::path::PathBuf;

use bpaf::{Bpaf, Parser};
use git2::Repository;

use error::Result;

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
    /// Show the deployment plan.
    #[bpaf(command)]
    Plan {
        /// Path to the bare Git store.
        #[bpaf(positional("STORE"))]
        store: PathBuf,
    },
    /// Check out an app for a host from a commit.
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
        /// App name.
        #[bpaf(positional("APP"))]
        app: String,
        /// Target directory to check out into.
        #[bpaf(positional("TARGET"))]
        target: PathBuf,
    },
}

fn run() -> Result<()> {
    let cmd = cmd().to_options().run();

    match cmd {
        Cmd::Commit { store, dir } => {
            let repo = match Repository::open(&store) {
                Ok(r) => r,
                Err(_) => Repository::init_bare(&store)?,
            };
            let tree_oid = store::build_tree(&repo, &dir)?;
            let commit_oid = store::commit_tree(&repo, tree_oid)?;
            println!("{commit_oid}");
        }
        Cmd::Plan { store } => {
            let repo = Repository::open(&store)?;
            let plan = plan::make_plan(&repo)?;
            println!("{plan:?}");
        }
        Cmd::Apply {
            store,
            commit,
            host,
            app,
            target,
        } => {
            let repo = Repository::open(&store)?;
            let oid = git2::Oid::from_str(&commit)?;
            store::checkout_app(&repo, oid, &host, &app, &target)?;
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

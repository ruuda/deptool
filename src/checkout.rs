// Deptool -- A declarative configuration deployment tool.
// Copyright 2026 Ruud van Asseldonk

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// A copy of the License has been included in the root of the repository.

//! Host filesystem operations: app checkout, symlink reconciliation, and cleanup.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::{self as unix_fs, PermissionsExt};
use std::path::{Path, PathBuf};

use git2::Oid;

use crate::error::ApplyError;
use crate::plan::{AppDiff, SymlinkChanges};
use crate::prim::Hostname;
use crate::store::Store;

type Result<T> = std::result::Result<T, ApplyError>;

const OID_PREFIX_LEN: usize = 10;

/// Truncate an oid to a short prefix for use in directory names and logs.
pub fn oid_prefix(oid: Oid) -> String {
    let mut buf = oid.to_string();
    buf.truncate(OID_PREFIX_LEN);
    buf
}

#[derive(Clone, Copy)]
pub enum CheckoutMode {
    /// Always re-checkout from the store (forward deploy).
    Fresh,
    /// Reuse existing version dir if present (rollback).
    Reuse,
}

/// Check out an app and update the `current` and `previous` symlinks.
///
/// In `Fresh` mode, any existing version directory is removed and
/// re-checked out (it may be from an interrupted deploy). In `Reuse`
/// mode, an existing directory is trusted and only the symlinks are updated.
///
/// `previous` points to whatever `current` pointed to before this call.
/// After a normal deploy this is the last successful version. After a
/// rollback this is the *failed* version (useful for debugging), not the
/// previous successful one. When `current` already points at the target
/// version (a reapply), both symlinks stay untouched.
fn checkout_app(
    store: &Store,
    commit_oid: Oid,
    host: &Hostname,
    app: &str,
    apps_dir: &Path,
    mode: CheckoutMode,
) -> Result<()> {
    let prefix = oid_prefix(commit_oid);
    let app_dir = apps_dir.join(app);
    let version_dir = app_dir.join(&prefix);

    let needs_checkout = match mode {
        CheckoutMode::Fresh => true,
        CheckoutMode::Reuse => !version_dir.exists(),
    };
    if needs_checkout {
        if version_dir.exists() {
            remove_dir_all(&version_dir)?;
        }
        create_dir_all(&version_dir)?;
        store.checkout_app(commit_oid, host, app, &version_dir)?;
    }

    let next = app_dir.join("next");
    let current = app_dir.join("current");

    // When `current` already points at the target version (a reapply, or a
    // deploy retried after a crash), skip the rotation below: it would point
    // `previous` at the same version as `current`, and the next gc would
    // then delete the real rollback target.
    if fs::read_link(&current).ok().as_deref() == Some(Path::new(&prefix)) {
        return Ok(());
    }

    if next.symlink_metadata().is_ok() {
        remove_file(&next)?;
    }
    symlink(&prefix, &next)?;

    // Rotate current → previous before overwriting current.
    if current.symlink_metadata().is_ok() {
        let previous = app_dir.join("previous");
        rename(&current, &previous)?;
    }
    rename(&next, &current)?;

    Ok(())
}

/// Remove an app: delete its directory under apps_dir.
pub fn remove_app(app: &str, apps_dir: &Path) -> Result<()> {
    let app_dir = apps_dir.join(app);
    if app_dir.exists() {
        remove_dir_all(&app_dir)?;
    }
    Ok(())
}

/// Remove old version directories from all apps under `apps_dir`.
///
/// Keeps only the directories that `current` and `previous` point to.
/// A leftover directory is harmless and will be cleaned up on the next
/// successful deploy.
pub fn gc_old_checkouts(apps_dir: &Path, mut log: impl FnMut(std::fmt::Arguments<'_>)) {
    let entries = match fs::read_dir(apps_dir) {
        Ok(entries) => entries,
        Err(err) => {
            log(format_args!(
                "gc: failed to read {}: {err}",
                apps_dir.display()
            ));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                log(format_args!(
                    "gc: entry error in {}: {err}",
                    apps_dir.display()
                ));
                continue;
            }
        };
        if entry.path().is_dir() {
            gc_app_dir(&entry.path(), &mut log);
        }
    }
}

fn gc_app_dir(app_dir: &Path, log: &mut impl FnMut(std::fmt::Arguments<'_>)) {
    let keep: BTreeSet<PathBuf> = ["current", "previous"]
        .iter()
        .filter_map(|name| fs::read_link(app_dir.join(name)).ok())
        .map(|target| app_dir.join(target))
        .collect();

    let entries = match fs::read_dir(app_dir) {
        Ok(entries) => entries,
        Err(err) => {
            log(format_args!(
                "gc: failed to read {}: {err}",
                app_dir.display()
            ));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();
        // Skip symlinks (current, previous, next) and kept version dirs.
        if path.is_symlink() || keep.contains(&path) {
            continue;
        }
        if path.is_dir() {
            match fs::remove_dir_all(&path) {
                Ok(()) => log(format_args!("gc: removed {}", path.display())),
                Err(err) => log(format_args!(
                    "gc: failed to remove {}: {err}",
                    path.display()
                )),
            }
        }
    }
}

/// Handler for `map_err` that wraps a failure as `CheckoutFailed` for the
/// given app.
fn checkout_failed(app: &str) -> impl FnOnce(ApplyError) -> ApplyError + '_ {
    move |cause| ApplyError::CheckoutFailed {
        app: app.to_string(),
        cause: cause.to_string(),
    }
}

/// Checkout or remove each changed app on the filesystem.
///
/// This is the only part of a deploy that mutates `apps_dir`. It does not
/// touch refs, systemd, or host-level symlinks.
pub fn checkout(
    store: &Store,
    commit_oid: Option<Oid>,
    app_diffs: &BTreeMap<String, AppDiff>,
    host: &Hostname,
    apps_dir: &Path,
    mode: CheckoutMode,
    mut log: impl FnMut(std::fmt::Arguments<'_>),
) -> Result<()> {
    for (app, change) in app_diffs {
        match change {
            AppDiff::Add { .. } => {
                log(format_args!("adding app {app}"));
                let oid = commit_oid.expect("Add/Update requires a target commit");
                checkout_app(store, oid, host, app, apps_dir, mode)
                    .map_err(checkout_failed(app))?;
            }
            AppDiff::Update { .. } => {
                log(format_args!("updating app {app}"));
                let oid = commit_oid.expect("Add/Update requires a target commit");
                checkout_app(store, oid, host, app, apps_dir, mode)
                    .map_err(checkout_failed(app))?;
            }
            AppDiff::Remove { .. } => {
                log(format_args!("removing app {app}"));
                remove_app(app, apps_dir)?;
            }
        }
    }
    Ok(())
}

/// Reconcile managed symlinks in a directory.
///
/// Makes `target_dir` contain exactly the symlinks in `desired`, only
/// touching symlinks that point through an app's `current` symlink (i.e.,
/// ones we created).
pub fn reconcile_managed_symlinks(
    desired: &BTreeMap<String, PathBuf>,
    apps_dir: &Path,
    target_dir: &Path,
) -> Result<()> {
    let actual = collect_managed_symlinks(apps_dir, target_dir)?;

    for name in actual.keys() {
        if !desired.contains_key(name) {
            let link_path = target_dir.join(name);
            remove_file(&link_path)?;
            // Deptool manages files declaratively, so it shouldn't leave an
            // empty `<unit>.d` directory behind once our last drop-in is gone.
            let parent = link_path.parent().expect("a link path has a parent");
            prune_empty_dirs(target_dir, parent);
        }
    }

    for (name, desired_target) in desired {
        let needs_update = match actual.get(name) {
            None => true,
            Some(actual_target) => actual_target != desired_target,
        };
        if needs_update {
            let link_path = target_dir.join(name);
            // `name` may be a drop-in path like `<unit>.d/override.conf`. Create
            // its parent as a real directory so unmanaged drop-ins there coexist
            // with ours.
            let parent = link_path.parent().expect("a link path has a parent");
            create_managed_dir(target_dir, parent)?;
            if link_path.exists() || link_path.symlink_metadata().is_ok() {
                remove_file(&link_path)?;
            }
            symlink(desired_target, &link_path)?;
        }
    }

    Ok(())
}

/// Create `dir` and any missing ancestors up to `stop`, giving every level
/// mode 0o755.
///
/// `create_dir_all` leaves new directories at the umask default, but systemd
/// needs drop-in directories world-readable. So we set the mode on the whole
/// chain from `dir` up to `stop` -- the managed root we own (e.g.
/// /etc/sysusers.d on a fresh system) -- normalizing pre-existing levels too,
/// not just the innermost one `create_dir_all` made.
fn create_managed_dir(stop: &Path, dir: &Path) -> Result<()> {
    assert!(
        dir.starts_with(stop),
        "managed dir {dir:?} must be under the managed root {stop:?}",
    );
    create_dir_all(dir)?;
    let mut level = dir;
    loop {
        set_permissions(level, fs::Permissions::from_mode(0o755))?;
        if level == stop {
            break;
        }
        level = level.parent().expect("a dir under stop has a parent");
    }
    Ok(())
}

/// Remove `dir` and its ancestors below `stop`, as long as each is empty.
///
/// The walk steps up one parent at a time and halts at `stop`, so the managed
/// root is never removed. Best-effort: failing to prune is not worth failing
/// the deploy.
fn prune_empty_dirs(stop: &Path, dir: &Path) {
    assert!(
        dir.starts_with(stop),
        "prune walks only under the managed root {stop:?}, not {dir:?}",
    );
    let mut level = dir;
    loop {
        // `remove_dir` removes a directory only when it is empty; a non-empty
        // directory (one still holding unmanaged drop-ins) returns an error,
        // which ends the walk and leaves it in place.
        if level == stop || fs::remove_dir(level).is_err() {
            break;
        }
        level = level.parent().expect("a dir under stop has a parent");
    }
}

// The functions below wrap std::fs operations to include the failing operation
// and path in the error, so a bare io message like "Is a directory" doesn't
// reach the operator without context. Call the wrapper instead of std::fs
// wherever the error propagates out of the apply; raw std::fs stays fine where
// the caller inspects or discards the error itself.

/// Handler for `map_err` on operations without a wrapper (reads on open
/// files, directory iteration).
fn io_failed<'a>(
    operation: &'static str,
    path: &'a Path,
) -> impl FnOnce(std::io::Error) -> ApplyError + 'a {
    move |err| ApplyError::Io(format!("failed to {operation} {}: {err}", path.display()))
}

fn remove_failed(path: &Path) -> impl FnOnce(std::io::Error) -> ApplyError + '_ {
    move |err| ApplyError::RemoveFailed {
        path: path.display().to_string(),
        cause: err.to_string(),
    }
}

fn create_dir_all(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(io_failed("create directory", path))
}

fn metadata(path: &Path) -> Result<fs::Metadata> {
    fs::metadata(path).map_err(io_failed("inspect", path))
}

fn open(path: &Path) -> Result<fs::File> {
    fs::File::open(path).map_err(io_failed("read", path))
}

/// Open a directory for iteration, or `None` if it does not exist.
fn read_dir_if_exists(path: &Path) -> Result<Option<fs::ReadDir>> {
    match fs::read_dir(path) {
        Ok(entries) => Ok(Some(entries)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(io_failed("read directory", path)(err)),
    }
}

fn read_link(path: &Path) -> Result<PathBuf> {
    fs::read_link(path).map_err(io_failed("read symlink", path))
}

fn remove_dir_all(path: &Path) -> Result<()> {
    fs::remove_dir_all(path).map_err(remove_failed(path))
}

fn remove_file(path: &Path) -> Result<()> {
    fs::remove_file(path).map_err(remove_failed(path))
}

fn rename(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to).map_err(|err| {
        ApplyError::Io(format!(
            "failed to rename {} to {}: {err}",
            from.display(),
            to.display(),
        ))
    })
}

fn set_permissions(path: &Path, perms: fs::Permissions) -> Result<()> {
    fs::set_permissions(path, perms).map_err(io_failed("set permissions on", path))
}

fn symlink(target: impl AsRef<Path>, link: &Path) -> Result<()> {
    unix_fs::symlink(target, link).map_err(|err| ApplyError::SymlinkFailed {
        link: link.display().to_string(),
        cause: err.to_string(),
    })
}

/// Reconcile manifest symlinks on the host filesystem.
///
/// Before removing or overwriting a symlink, verifies it points into
/// `apps_dir` to avoid touching symlinks not created by Deptool.
///
/// The checks and mutations are not atomic (TOCTOU), but we hold the
/// deploy lock and assume the target filesystem is not changing
/// concurrently.
pub fn reconcile_config_symlinks(apps_dir: &Path, changes: &SymlinkChanges<PathBuf>) -> Result<()> {
    for target in &changes.remove {
        if target.symlink_metadata().is_ok() {
            verify_managed(target, apps_dir)?;
            remove_file(target)?;
        }
    }

    for (target, source) in &changes.change {
        if target.symlink_metadata().is_ok() {
            verify_managed(target, apps_dir)?;
            remove_file(target)?;
        }
        create_symlink(source, target)?;
    }

    for (target, source) in &changes.create {
        create_symlink(source, target)?;
    }

    Ok(())
}

/// Create a symlink at `link` pointing to `source`.
///
/// A symlink that already points at `source` is left in place: a failed
/// rollback-unsafe deploy leaves its symlinks behind, and the retry plans
/// the same creates again.
///
/// If a file already exists at `link` with identical contents to `source`,
/// it is replaced with the symlink. This supports incremental adoption:
/// config files already present on the host can be moved under Deptool
/// management without manual intervention.
fn create_symlink(source: &Path, link: &Path) -> Result<()> {
    let link_name = link.display().to_string();
    match unix_fs::symlink(source, link) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            // Classification reads the source as well as the link. Every
            // wrapper error includes its own failing path, so wrapping in
            // the link's error does not misattribute a source failure.
            let state = classify_link(source, link).map_err(|cause| ApplyError::SymlinkFailed {
                link: link_name.clone(),
                cause: cause.to_string(),
            })?;
            match state {
                LinkState::PointsAtSource => Ok(()),
                LinkState::MatchingFile => {
                    remove_file(link)?;
                    symlink(source, link)?;
                    Ok(())
                }
                LinkState::Directory => Err(ApplyError::SymlinkFailed {
                    link: link_name,
                    cause: "a directory already exists; remove it manually and retry".into(),
                }),
                LinkState::Mismatch => Err(ApplyError::SymlinkFailed {
                    link: link_name,
                    cause: "a file with different contents already exists; \
                            remove it manually and retry"
                        .into(),
                }),
            }
        }
        Err(err) => {
            let cause = match err.kind() {
                std::io::ErrorKind::NotFound => match link.parent() {
                    Some(parent) if !parent.exists() => {
                        format!("parent directory {} does not exist", parent.display())
                    }
                    _ => err.to_string(),
                },
                _ => err.to_string(),
            };
            Err(ApplyError::SymlinkFailed {
                link: link_name,
                cause,
            })
        }
    }
}

/// What occupies the link path, relative to the symlink's source.
///
/// The caller derives error messages from this classification rather than
/// from a second look at the filesystem, so a message cannot describe a
/// different state than the one that was checked.
enum LinkState {
    /// A symlink that already points at the source.
    PointsAtSource,
    /// A regular file with contents identical to the source file.
    MatchingFile,
    /// A directory; there are no contents to compare.
    Directory,
    /// Anything else: contents differ, or a side is not a regular file.
    Mismatch,
}

/// Classify what occupies the link path. Contents are compared without
/// reading either file entirely into memory.
fn classify_link(source: &Path, link: &Path) -> Result<LinkState> {
    use std::io::{BufReader, Read};
    if let Ok(target) = fs::read_link(link)
        && target == source
    {
        return Ok(LinkState::PointsAtSource);
    }
    let source_meta = metadata(source)?;
    let link_meta = metadata(link)?;
    if link_meta.is_dir() {
        return Ok(LinkState::Directory);
    }
    if !source_meta.is_file() || !link_meta.is_file() {
        return Ok(LinkState::Mismatch);
    }
    if source_meta.len() != link_meta.len() {
        return Ok(LinkState::Mismatch);
    }
    let mut source_file = BufReader::new(open(source)?);
    let mut link_file = BufReader::new(open(link)?);
    let mut buf_source = [0u8; 8192];
    let mut buf_link = [0u8; 8192];
    loop {
        let n = source_file
            .read(&mut buf_source)
            .map_err(io_failed("read", source))?;
        link_file
            .read_exact(&mut buf_link[..n])
            .map_err(io_failed("read", link))?;
        if buf_source[..n] != buf_link[..n] {
            return Ok(LinkState::Mismatch);
        }
        if n == 0 {
            return Ok(LinkState::MatchingFile);
        }
    }
}

/// Verify a symlink points into `apps_dir` before we remove or overwrite it.
fn verify_managed(link: &Path, apps_dir: &Path) -> Result<()> {
    match fs::read_link(link) {
        Ok(actual) if actual.starts_with(apps_dir) => Ok(()),
        Ok(actual) => Err(ApplyError::SymlinkFailed {
            link: link.display().to_string(),
            cause: format!(
                "refusing to touch: points to {}, not into {}",
                actual.display(),
                apps_dir.display(),
            ),
        }),
        // Dangling or missing -- nothing to protect.
        Err(_) => Ok(()),
    }
}

/// Whether a symlink target points through an app's `current` symlink.
///
/// This distinguishes our symlinks from the ones `systemctl enable` creates:
/// every symlink Deptool creates targets `apps_dir/<app>/current/...`, while
/// systemctl resolves our unit symlink, so its enablement links (in `.wants`
/// and `.requires` directories, and aliases) target the version-pinned
/// checkout directory. Those links are systemd's to manage; treating them as
/// ours would delete them on every reconcile, disabling the unit at the next
/// boot. If Deptool ever creates managed symlinks that don't go through
/// `current`, this check must learn about them.
fn points_through_current(apps_dir: &Path, target: &Path) -> bool {
    let rel = match target.strip_prefix(apps_dir) {
        Ok(rel) => rel,
        Err(_) => return false,
    };
    let mut components = rel.iter();
    let _app = components.next();
    components.next() == Some(OsStr::new("current"))
}

/// Collect symlinks under `dir` that point through an app's `current`
/// symlink, keyed by their path relative to `dir`.
///
/// We create absolute symlinks, so we just check the raw symlink target; no
/// canonicalization needed. Real subdirectories (e.g. `<unit>.d` drop-in
/// dirs) are recursed into to find our drop-in files; unmanaged files and
/// directories alongside them are ignored.
fn collect_managed_symlinks(apps_dir: &Path, dir: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let mut managed = BTreeMap::new();
    collect_managed_symlinks_into(apps_dir, dir, "", &mut managed)?;
    Ok(managed)
}

fn collect_managed_symlinks_into(
    apps_dir: &Path,
    dir: &Path,
    prefix: &str,
    managed: &mut BTreeMap<String, PathBuf>,
) -> Result<()> {
    let entries = match read_dir_if_exists(dir)? {
        Some(entries) => entries,
        None => return Ok(()),
    };

    for entry in entries {
        let entry = entry.map_err(io_failed("read directory", dir))?;
        let path = entry.path();
        // Check is_symlink before is_dir: a symlink to a directory reports
        // is_dir() == true, but we must treat it as a single leaf, not
        // recurse through it into apps_dir.
        if path.is_symlink() {
            let target = read_link(&path)?;
            if points_through_current(apps_dir, &target) {
                let name = entry.file_name();
                let name = name.to_str().ok_or_else(|| ApplyError::SymlinkFailed {
                    link: path.display().to_string(),
                    cause: "file name is not valid UTF-8".into(),
                })?;
                managed.insert(format!("{prefix}{name}"), target);
            }
        } else if path.is_dir() {
            // Our drop-in directories come from git and are always UTF-8; a
            // non-UTF-8 directory can't hold our symlinks, so skip it.
            if let Some(name) = entry.file_name().to_str() {
                collect_managed_symlinks_into(
                    apps_dir,
                    &path,
                    &format!("{prefix}{name}/"),
                    managed,
                )?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{SystemDiff, diff_enabled, diff_host};
    use crate::testutil::{TempDir, TestRepo, assert_dir_contents};

    // Tests do both apply calls (-> ApplyError) and raw git operations
    // (-> git2::Error), so use the top-level Result that accepts both.
    use crate::error::Result;

    #[test]
    fn checkout_app_creates_versioned_checkout_and_current_symlink() -> Result<()> {
        let t = ApplyTest::new();
        let c1 = t.repo.commit(&[("web1/nginx/nginx.conf", b"server {}")]);

        t.checkout("web1", "nginx", c1, CheckoutMode::Fresh)?;

        let prefix = oid_prefix(c1);
        let version_dir = t.apps.path().join("nginx").join(&prefix);
        assert!(version_dir.join("nginx.conf").exists(), "checkout exists");

        let current = t.apps.path().join("nginx/current");
        let target = fs::read_link(&current)?;
        assert_eq!(target.to_str().expect("target is utf-8"), prefix);

        Ok(())
    }

    #[test]
    fn checkout_app_replaces_existing_checkout() -> Result<()> {
        let t = ApplyTest::new();
        let c1 = t.repo.commit(&[("web1/nginx/nginx.conf", b"v1")]);

        // Create a partial/corrupt checkout dir.
        let prefix = oid_prefix(c1);
        let corrupt_dir = t.apps.path().join("nginx").join(&prefix);
        fs::create_dir_all(&corrupt_dir)?;
        fs::write(corrupt_dir.join("garbage"), "bad")?;

        t.checkout("web1", "nginx", c1, CheckoutMode::Fresh)?;

        assert_dir_contents(&corrupt_dir, &[("nginx.conf", b"v1")]);

        Ok(())
    }

    #[test]
    fn reuse_mode_preserves_existing_checkout() -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let t = ApplyTest::new();
        let c1 = t.repo.commit(&[("web1/nginx/nginx.conf", b"v1")]);
        let c2 = t.repo.commit(&[("web1/nginx/nginx.conf", b"v2")]);

        t.checkout("web1", "nginx", c1, CheckoutMode::Fresh)?;
        t.checkout("web1", "nginx", c2, CheckoutMode::Fresh)?;

        let prefix = oid_prefix(c1);
        let c1_conf = t.apps.path().join("nginx").join(prefix).join("nginx.conf");
        let inode_before = fs::metadata(&c1_conf)?.ino();

        // Reuse mode: repoints to c1 without re-checking out.
        t.checkout("web1", "nginx", c1, CheckoutMode::Reuse)?;

        let current = fs::read_link(t.apps.path().join("nginx/current"))?;
        assert_eq!(current.to_str().expect("utf-8"), oid_prefix(c1));

        // Same inode -- the file was not recreated.
        assert_eq!(fs::metadata(&c1_conf)?.ino(), inode_before);

        // Fresh mode re-checkouts even though the dir exists.
        t.checkout("web1", "nginx", c1, CheckoutMode::Fresh)?;
        assert_ne!(fs::metadata(&c1_conf)?.ino(), inode_before);

        Ok(())
    }

    #[test]
    fn checkout_app_swaps_symlink_on_update() -> Result<()> {
        let t = ApplyTest::new();
        let c1 = t.repo.commit(&[("web1/nginx/nginx.conf", b"v1")]);
        let c2 = t.repo.commit(&[("web1/nginx/nginx.conf", b"v2")]);

        let nginx = t.apps.path().join("nginx");

        t.checkout("web1", "nginx", c1, CheckoutMode::Fresh)?;
        assert_eq!(
            fs::read_link(nginx.join("current"))?,
            Path::new(&oid_prefix(c1))
        );
        // No previous on first deploy.
        assert!(!nginx.join("previous").exists());

        t.checkout("web1", "nginx", c2, CheckoutMode::Fresh)?;
        assert_eq!(
            fs::read_link(nginx.join("current"))?,
            Path::new(&oid_prefix(c2))
        );
        assert_eq!(
            fs::read_link(nginx.join("previous"))?,
            Path::new(&oid_prefix(c1))
        );

        Ok(())
    }

    #[test]
    fn checkout_app_keeps_previous_when_current_already_at_target() -> Result<()> {
        let t = ApplyTest::new();
        let c1 = t.repo.commit(&[("web1/nginx/nginx.conf", b"v1")]);
        let c2 = t.repo.commit(&[("web1/nginx/nginx.conf", b"v2")]);

        let nginx = t.apps.path().join("nginx");

        t.checkout("web1", "nginx", c1, CheckoutMode::Fresh)?;
        t.checkout("web1", "nginx", c2, CheckoutMode::Fresh)?;

        // Re-checkout of the version that is already current (a reapply).
        // Rotating here would point `previous` at the same version as
        // `current`, and the next gc would then delete the real rollback
        // target c1.
        t.checkout("web1", "nginx", c2, CheckoutMode::Fresh)?;

        assert_eq!(
            fs::read_link(nginx.join("current"))?,
            Path::new(&oid_prefix(c2))
        );
        assert_eq!(
            fs::read_link(nginx.join("previous"))?,
            Path::new(&oid_prefix(c1)),
            "previous still points at the last distinct version",
        );

        Ok(())
    }

    #[test]
    fn gc_removes_old_version_dirs() -> Result<()> {
        let t = ApplyTest::new();
        let c1 = t.repo.commit(&[("web1/nginx/conf", b"v1")]);
        let c2 = t.repo.commit(&[("web1/nginx/conf", b"v2")]);
        let c3 = t.repo.commit(&[("web1/nginx/conf", b"v3")]);

        t.checkout("web1", "nginx", c1, CheckoutMode::Fresh)?;
        t.checkout("web1", "nginx", c2, CheckoutMode::Fresh)?;
        t.checkout("web1", "nginx", c3, CheckoutMode::Fresh)?;

        // All three version dirs exist before GC.
        let nginx = t.apps.path().join("nginx");
        assert!(nginx.join(oid_prefix(c1)).exists());
        assert!(nginx.join(oid_prefix(c2)).exists());
        assert!(nginx.join(oid_prefix(c3)).exists());

        let ignore_log = |_: std::fmt::Arguments<'_>| {};
        gc_old_checkouts(t.apps.path(), ignore_log);

        // current=c3, previous=c2, c1 is gone.
        assert!(!nginx.join(oid_prefix(c1)).exists());
        assert!(nginx.join(oid_prefix(c2)).exists());
        assert!(nginx.join(oid_prefix(c3)).exists());

        Ok(())
    }

    #[test]
    fn checkout_app_makes_files_readonly() -> Result<()> {
        let t = ApplyTest::new();
        let c1 = t.repo.commit(&[("web1/nginx/nginx.conf", b"server {}")]);

        t.checkout("web1", "nginx", c1, CheckoutMode::Fresh)?;

        let conf = t
            .apps
            .path()
            .join("nginx")
            .join(oid_prefix(c1))
            .join("nginx.conf");
        let mode = fs::metadata(&conf)?.permissions().mode();
        assert_eq!(mode & 0o222, 0, "no write bits set, mode is {mode:o}");

        Ok(())
    }

    #[test]
    fn remove_app_deletes_the_app_directory() -> Result<()> {
        let t = ApplyTest::new();
        let c1 = t.repo.commit(&[("web1/nginx/nginx.conf", b"v1")]);

        t.checkout("web1", "nginx", c1, CheckoutMode::Fresh)?;
        assert!(t.apps.path().join("nginx").exists());

        remove_app("nginx", t.apps.path())?;
        assert!(!t.apps.path().join("nginx").exists());

        Ok(())
    }

    #[test]
    fn remove_app_succeeds_when_app_does_not_exist() -> Result<()> {
        let apps = TempDir::new("apps");
        remove_app("nonexistent", apps.path())?;
        Ok(())
    }

    // reconcile_managed_symlinks tests (which need filesystem access)

    #[test]
    fn reconcile_managed_symlinks_creates_symlink_for_desired_unit() -> Result<()> {
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        let target = apps.path().join("nginx/current/systemd/nginx.service");
        let desired = BTreeMap::from([("nginx.service".to_string(), target)]);

        reconcile_managed_symlinks(&desired, apps.path(), units.path())?;

        assert!(units.path().join("nginx.service").is_symlink());
        Ok(())
    }

    #[test]
    fn reconcile_managed_symlinks_creates_target_dir_if_missing() -> Result<()> {
        let apps = TempDir::new("apps");
        let sysusers_dir = apps.path().join("nonexistent-sysusers.d");
        let target = apps.path().join("myapp/current/sysusers/myapp.conf");
        let desired = BTreeMap::from([("myapp.conf".to_string(), target)]);

        reconcile_managed_symlinks(&desired, apps.path(), &sysusers_dir)?;

        assert!(sysusers_dir.join("myapp.conf").is_symlink());
        Ok(())
    }

    /// The target changes when a file moves between app subdirectories, e.g.
    /// migrating a file into networkd/ when updating to a Deptool version
    /// that manages that directory.
    #[test]
    fn reconcile_managed_symlinks_repoints_symlink_when_target_changes() -> Result<()> {
        let apps = TempDir::new("apps");
        let networkd_dir = TempDir::new("networkd");
        let link = networkd_dir.path().join("wg0.netdev");
        let old_target = apps.path().join("wg/current/wg0.netdev");
        let new_target = apps.path().join("wg/current/networkd/wg0.netdev");
        unix_fs::symlink(&old_target, &link)?;

        let desired = BTreeMap::from([("wg0.netdev".to_string(), new_target.clone())]);
        reconcile_managed_symlinks(&desired, apps.path(), networkd_dir.path())?;

        assert_eq!(std::fs::read_link(&link)?, new_target);
        Ok(())
    }

    #[test]
    fn reconcile_managed_symlinks_removes_symlink_not_in_desired_set() -> Result<()> {
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        let target = apps.path().join("nginx/current/systemd/nginx.service");

        // Create a symlink as if a previous reconcile put it there.
        unix_fs::symlink(&target, units.path().join("nginx.service"))?;

        reconcile_managed_symlinks(&BTreeMap::new(), apps.path(), units.path())?;

        assert!(!units.path().join("nginx.service").exists());
        Ok(())
    }

    #[test]
    fn reconcile_managed_symlinks_leaves_unmanaged_symlinks_intact() -> Result<()> {
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");

        // A symlink that doesn't point into apps_dir, not ours.
        unix_fs::symlink(
            "/usr/lib/systemd/system/sshd.service",
            units.path().join("sshd.service"),
        )?;

        reconcile_managed_symlinks(&BTreeMap::new(), apps.path(), units.path())?;

        assert!(units.path().join("sshd.service").is_symlink());
        Ok(())
    }

    #[test]
    fn reconcile_managed_symlinks_leaves_systemctl_enablement_links_intact() -> Result<()> {
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        let desired = BTreeMap::from([(
            "nginx.service".to_string(),
            apps.path().join("nginx/current/systemd/nginx.service"),
        )]);

        // `systemctl enable` creates enablement symlinks in `.wants` dirs.
        // It resolves `current`, so the link points into apps_dir at the
        // version-pinned path. The link is systemd's, not ours; removing it
        // would disable the unit at boot.
        let wants_dir = units.path().join("multi-user.target.wants");
        fs::create_dir(&wants_dir)?;
        let pinned = apps.path().join("nginx/0123456789/systemd/nginx.service");
        unix_fs::symlink(&pinned, wants_dir.join("nginx.service"))?;

        reconcile_managed_symlinks(&desired, apps.path(), units.path())?;

        assert!(wants_dir.join("nginx.service").is_symlink());
        Ok(())
    }

    #[test]
    fn reconcile_managed_symlinks_indicates_path_when_a_directory_occupies_a_leaf() -> Result<()> {
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        let name = "nginx.service";
        let target = apps.path().join("nginx/current/systemd").join(name);
        let desired = BTreeMap::from([(name.to_string(), target)]);

        // A directory sits where a managed unit symlink should go. We can't
        // `remove_file` a directory; the error must indicate the path so the
        // operator can find and clear the obstruction.
        let link_path = units.path().join(name);
        fs::create_dir(&link_path)?;

        let err = reconcile_managed_symlinks(&desired, apps.path(), units.path()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("failed to remove") && msg.contains(&link_path.display().to_string()),
            "error says removal of the path failed, not symlink creation: {err}",
        );
        Ok(())
    }

    #[test]
    fn reconcile_managed_symlinks_creates_dropin_leaf_with_a_real_0755_parent_dir() -> Result<()> {
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        // Start the managed root restrictive to check we normalize it, and every
        // level we create below, to 0o755 -- which systemd requires.
        fs::set_permissions(units.path(), fs::Permissions::from_mode(0o700))?;
        let rel = "postgresql.service.d/override.conf";
        let target = apps.path().join("postgres/current/systemd").join(rel);
        let desired = BTreeMap::from([(rel.to_string(), target.clone())]);

        reconcile_managed_symlinks(&desired, apps.path(), units.path())?;

        let dropin_dir = units.path().join("postgresql.service.d");
        assert!(
            dropin_dir.is_dir() && !dropin_dir.is_symlink(),
            "the .d parent is a real directory, not a symlink",
        );
        let leaf = units.path().join(rel);
        assert!(leaf.is_symlink(), "the drop-in file is a symlink");
        assert_eq!(fs::read_link(&leaf)?, target);

        let mode = |p: &Path| fs::metadata(p).expect("dir exists").permissions().mode() & 0o777;
        assert_eq!(mode(units.path()), 0o755, "managed root is 0o755");
        assert_eq!(mode(&dropin_dir), 0o755, "created .d dir is 0o755");
        Ok(())
    }

    #[test]
    fn reconcile_managed_symlinks_preserves_unmanaged_sibling_in_dropin_dir() -> Result<()> {
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        let rel = "postgresql.service.d/override.conf";
        let target = apps.path().join("postgres/current/systemd").join(rel);
        let desired = BTreeMap::from([(rel.to_string(), target)]);

        reconcile_managed_symlinks(&desired, apps.path(), units.path())?;
        // A drop-in placed out of band (e.g. `systemctl edit`) in the same dir.
        let sibling = units.path().join("postgresql.service.d/manual.conf");
        fs::write(&sibling, b"[Service]")?;

        reconcile_managed_symlinks(&desired, apps.path(), units.path())?;

        assert!(
            sibling.is_file() && !sibling.is_symlink(),
            "the unmanaged sibling is left untouched",
        );
        assert!(units.path().join(rel).is_symlink(), "our drop-in is intact");
        Ok(())
    }

    #[test]
    fn reconcile_managed_symlinks_prunes_empty_dropin_dir_on_removal() -> Result<()> {
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        let rel = "postgresql.service.d/override.conf";
        let target = apps.path().join("postgres/current/systemd").join(rel);
        let desired = BTreeMap::from([(rel.to_string(), target)]);

        reconcile_managed_symlinks(&desired, apps.path(), units.path())?;
        reconcile_managed_symlinks(&BTreeMap::new(), apps.path(), units.path())?;

        assert!(
            !units.path().join("postgresql.service.d").exists(),
            "the now-empty .d directory is pruned",
        );
        Ok(())
    }

    #[test]
    fn reconcile_managed_symlinks_keeps_dropin_dir_with_unmanaged_sibling_on_removal() -> Result<()>
    {
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        let rel = "postgresql.service.d/override.conf";
        let target = apps.path().join("postgres/current/systemd").join(rel);
        let desired = BTreeMap::from([(rel.to_string(), target)]);

        reconcile_managed_symlinks(&desired, apps.path(), units.path())?;
        let sibling = units.path().join("postgresql.service.d/manual.conf");
        fs::write(&sibling, b"[Service]")?;
        reconcile_managed_symlinks(&BTreeMap::new(), apps.path(), units.path())?;

        assert!(sibling.is_file(), "the unmanaged sibling survives");
        assert!(!units.path().join(rel).exists(), "our drop-in was removed");
        Ok(())
    }

    #[test]
    fn reconcile_managed_symlinks_removes_a_directory_symlink_without_recursing() -> Result<()> {
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        // Deptool no longer symlinks a whole `.d` directory, but a host last
        // deployed by the old version still has such a symlink-to-directory.
        // Collecting must treat it as one managed leaf to remove, not recurse
        // through it into apps_dir (which could delete real files there).
        let target = apps.path().join("app/current/systemd/legacy.service.d");
        fs::create_dir_all(&target)?;
        fs::write(target.join("override.conf"), b"[Service]")?;
        unix_fs::symlink(&target, units.path().join("legacy.service.d"))?;

        reconcile_managed_symlinks(&BTreeMap::new(), apps.path(), units.path())?;

        assert!(
            !units.path().join("legacy.service.d").exists(),
            "the directory symlink is removed as one leaf",
        );
        assert!(
            target.join("override.conf").exists(),
            "nothing was deleted through the symlink",
        );
        Ok(())
    }

    #[test]
    fn reconcile_managed_symlinks_leaves_an_already_synced_symlink_intact() -> Result<()> {
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        let target = apps.path().join("nginx/current/systemd/nginx.service");
        let desired = BTreeMap::from([("nginx.service".to_string(), target.clone())]);

        reconcile_managed_symlinks(&desired, apps.path(), units.path())?;
        reconcile_managed_symlinks(&desired, apps.path(), units.path())?;

        // A second reconcile over an already-synced dir neither breaks nor
        // repoints the symlink.
        assert_eq!(fs::read_link(units.path().join("nginx.service"))?, target);
        Ok(())
    }

    #[test]
    fn diff_enabled_enables_units_only_in_target() {
        let prev = BTreeSet::new();
        let target = BTreeSet::from(["nginx.service".to_string()]);

        let actions = diff_enabled(&prev, &target);

        assert_eq!(actions.enable, vec!["nginx.service"]);
        assert!(actions.disable.is_empty());
    }

    #[test]
    fn diff_enabled_restarts_units_in_both_prev_and_target() {
        let both = BTreeSet::from(["nginx.service".to_string()]);

        let actions = diff_enabled(&both, &both);

        assert!(actions.enable.is_empty());
        assert_eq!(actions.restart, vec!["nginx.service"]);
        assert!(actions.disable.is_empty());
    }

    #[test]
    fn diff_enabled_disables_units_only_in_prev() {
        let prev = BTreeSet::from(["nginx.service".to_string()]);
        let target = BTreeSet::new();

        let actions = diff_enabled(&prev, &target);

        assert!(actions.enable.is_empty());
        assert_eq!(actions.disable, vec!["nginx.service"]);
    }

    /// Test harness for `diff_host` + `checkout` + `reconcile_managed_symlinks`.
    struct ApplyTest {
        repo: TestRepo,
        apps: TempDir,
        units: TempDir,
    }

    impl ApplyTest {
        fn new() -> Self {
            Self {
                repo: TestRepo::new(),
                apps: TempDir::new("apps"),
                units: TempDir::new("units"),
            }
        }

        fn checkout(
            &self,
            host: &str,
            app: &str,
            commit: Oid,
            mode: CheckoutMode,
        ) -> std::result::Result<(), ApplyError> {
            checkout_app(
                &self.repo.store,
                commit,
                &host.into(),
                app,
                self.apps.path(),
                mode,
            )
        }

        fn apply(
            &self,
            commit: git2::Oid,
            current: Option<git2::Oid>,
        ) -> Result<SystemDiff<PathBuf>> {
            let host = &"web1".into();
            let (app_diffs, system) = diff_host(
                &self.repo.store,
                host,
                self.apps.path(),
                current,
                Some(commit),
            )?;
            let ignore_log = |_: std::fmt::Arguments<'_>| {};
            checkout(
                &self.repo.store,
                Some(commit),
                &app_diffs,
                host,
                self.apps.path(),
                CheckoutMode::Fresh,
                ignore_log,
            )?;

            // Reconcile here so tests that check unit symlinks still work.
            let desired = self
                .repo
                .store
                .desired_units(commit, host, self.apps.path())?;
            reconcile_managed_symlinks(&desired, self.apps.path(), self.units.path())?;
            Ok(system)
        }
    }

    #[test]
    fn diff_host_only_enables_units_from_systemd_json() -> Result<()> {
        let t = ApplyTest::new();
        let c1 = t.repo.commit(&[
            ("web1/nginx/systemd/nginx.service", b"[Service]"),
            ("web1/nginx/systemd/nginx-reload.timer", b"[Timer]"),
            (
                "web1/nginx/manifest.json",
                br#"{"systemd": {"units_enabled": ["nginx.service"]}}"#,
            ),
        ]);

        let changes = t.apply(c1, None)?;

        // Both units are symlinked (available).
        assert!(t.units.path().join("nginx.service").is_symlink());
        assert!(t.units.path().join("nginx-reload.timer").is_symlink());
        // Only the enabled one gets an enable action.
        assert_eq!(changes.unit_actions.enable, vec!["nginx.service"]);
        assert!(changes.unit_actions.restart.is_empty());
        assert!(changes.unit_actions.disable.is_empty());
        Ok(())
    }

    #[test]
    fn added_app_with_sysusers_links_and_sets_content_changed() -> Result<()> {
        let t = ApplyTest::new();
        let c1 = t.repo.commit(&[
            ("web1/myapp/sysusers/myapp.conf", b"u myapp -"),
            ("web1/myapp/config.toml", b"key = true"),
        ]);

        let changes = t.apply(c1, None)?;

        assert_eq!(changes.sysusers.link, vec!["myapp.conf"]);
        assert!(changes.sysusers.unlink.is_empty());
        assert!(changes.sysusers.content_changed);
        Ok(())
    }

    #[test]
    fn sysusers_content_changed_when_file_modified() -> Result<()> {
        let t = ApplyTest::new();
        let c1 = t.repo.commit(&[
            ("web1/myapp/sysusers/myapp.conf", b"u myapp -"),
            ("web1/myapp/config.toml", b"v1"),
        ]);
        let c2 = t.repo.commit(&[
            ("web1/myapp/sysusers/myapp.conf", b"u myapp - \"My App\""),
            ("web1/myapp/config.toml", b"v1"),
        ]);

        let changes = t.apply(c2, Some(c1))?;

        // Same set of files, but content changed.
        assert!(changes.sysusers.link.is_empty());
        assert!(changes.sysusers.unlink.is_empty());
        assert!(changes.sysusers.content_changed);
        Ok(())
    }

    #[test]
    fn sysusers_link_makes_deploy_rollback_unsafe() -> Result<()> {
        let t = ApplyTest::new();
        let c1 = t.repo.commit(&[
            ("web1/myapp/sysusers/myapp.conf", b"u myapp -"),
            ("web1/myapp/config.toml", b"v1"),
        ]);

        let changes = t.apply(c1, None)?;

        assert!(!changes.is_rollback_safe());
        Ok(())
    }

    /// Content-only sysusers changes are rollback-safe: deptool can restore
    /// the old config file. The OS won't un-create a user that was already
    /// materialized, but rolling back the config is still useful.
    #[test]
    fn sysusers_content_change_without_link_is_rollback_safe() -> Result<()> {
        let t = ApplyTest::new();
        let c1 = t.repo.commit(&[
            ("web1/myapp/sysusers/myapp.conf", b"u myapp -"),
            ("web1/myapp/config.toml", b"v1"),
        ]);
        let c2 = t.repo.commit(&[
            ("web1/myapp/sysusers/myapp.conf", b"u myapp - \"My App\""),
            ("web1/myapp/config.toml", b"v1"),
        ]);

        let changes = t.apply(c2, Some(c1))?;

        assert!(changes.sysusers.content_changed);
        assert!(changes.is_rollback_safe());
        Ok(())
    }

    #[test]
    fn sysusers_not_changed_when_only_other_files_change() -> Result<()> {
        let t = ApplyTest::new();
        let c1 = t.repo.commit(&[
            ("web1/myapp/sysusers/myapp.conf", b"u myapp -"),
            ("web1/myapp/config.toml", b"v1"),
        ]);
        let c2 = t.repo.commit(&[
            ("web1/myapp/sysusers/myapp.conf", b"u myapp -"),
            ("web1/myapp/config.toml", b"v2"),
        ]);

        let changes = t.apply(c2, Some(c1))?;

        assert!(changes.sysusers.link.is_empty());
        assert!(changes.sysusers.unlink.is_empty());
        assert!(!changes.sysusers.content_changed);
        Ok(())
    }

    /// `content_changed` follows the `quadlets/` subtree, not the whole app
    /// tree: a change to a sibling file (e.g. `config.toml`) must not flip
    /// the signal, while a change to the quadlet itself must.
    #[test]
    fn quadlet_content_changed_tracks_quadlets_subtree() -> Result<()> {
        let t = ApplyTest::new();
        let base = t.repo.commit(&[
            ("web1/myapp/quadlets/myapp.container", b"Image=v1"),
            ("web1/myapp/config.toml", b"v1"),
        ]);
        let only_sibling_changed = t.repo.commit(&[
            ("web1/myapp/quadlets/myapp.container", b"Image=v1"),
            ("web1/myapp/config.toml", b"v2"),
        ]);
        let quadlet_changed = t.repo.commit(&[
            ("web1/myapp/quadlets/myapp.container", b"Image=v2"),
            ("web1/myapp/config.toml", b"v1"),
        ]);

        assert!(
            !t.apply(only_sibling_changed, Some(base))?
                .quadlets
                .content_changed
        );
        assert!(
            t.apply(quadlet_changed, Some(base))?
                .quadlets
                .content_changed
        );
        Ok(())
    }

    #[test]
    fn manifest_symlinks_creates_and_adopts_matching_file() -> Result<()> {
        let dir = TempDir::new("symlinks");
        let apps = TempDir::new("apps");
        let link = dir.path().join("foo.conf");
        let source = apps.path().join("nginx/current/foo.conf");
        fs::create_dir_all(source.parent().expect("source has a parent"))?;
        fs::write(&source, b"config data")?;

        // Fresh creation works.
        let changes = SymlinkChanges {
            create: vec![(link.clone(), source.clone())],
            remove: Vec::new(),
            change: Vec::new(),
        };
        reconcile_config_symlinks(apps.path(), &changes)?;
        assert_eq!(fs::read_link(&link)?, source);

        // Clean up for the next case.
        fs::remove_file(&link)?;

        // A regular file with identical contents is adopted.
        fs::write(&link, b"config data")?;
        reconcile_config_symlinks(apps.path(), &changes)?;
        assert!(link.is_symlink());
        assert_eq!(fs::read_link(&link)?, source);

        // Clean up for the next case.
        fs::remove_file(&link)?;

        // A regular file with different contents is refused.
        fs::write(&link, b"different data")?;
        let err = reconcile_config_symlinks(apps.path(), &changes).unwrap_err();
        assert!(
            err.to_string().contains("different contents"),
            "error mentions the conflict: {err}",
        );

        Ok(())
    }

    #[test]
    fn manifest_symlinks_create_of_existing_directory_symlink_is_a_noop() -> Result<()> {
        let dir = TempDir::new("symlinks");
        let apps = TempDir::new("apps");
        let link = dir.path().join("tools");
        let source = apps.path().join("tools/current/tools");
        fs::create_dir_all(&source)?;

        // A rollback-unsafe deploy that fails after this step leaves the
        // symlink behind, and the retry plans the same create again.
        let changes = SymlinkChanges {
            create: vec![(link.clone(), source.clone())],
            remove: Vec::new(),
            change: Vec::new(),
        };
        reconcile_config_symlinks(apps.path(), &changes)?;
        reconcile_config_symlinks(apps.path(), &changes)?;

        assert_eq!(fs::read_link(&link)?, source);
        Ok(())
    }

    #[test]
    fn manifest_symlinks_refuses_to_adopt_a_directory() -> Result<()> {
        let dir = TempDir::new("symlinks");
        let apps = TempDir::new("apps");
        let link = dir.path().join("tools");
        let source = apps.path().join("tools/current/tools");
        // A real directory occupies the link path; adoption cannot apply.
        fs::create_dir_all(&link)?;
        fs::create_dir_all(&source)?;

        let changes = SymlinkChanges {
            create: vec![(link.clone(), source)],
            remove: Vec::new(),
            change: Vec::new(),
        };
        let err = reconcile_config_symlinks(apps.path(), &changes).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(&link.display().to_string()) && msg.contains("a directory already exists"),
            "error indicates the occupied path and that it is a directory: {err}",
        );
        Ok(())
    }

    #[test]
    fn manifest_symlinks_error_indicates_the_symlink_being_created() -> Result<()> {
        let dir = TempDir::new("symlinks");
        let apps = TempDir::new("apps");
        let link = dir.path().join("tools.conf");
        let source = apps.path().join("tools/current/tools.conf");
        fs::create_dir_all(source.parent().expect("source has a parent"))?;
        fs::write(&source, b"data")?;
        // A dangling symlink occupies the link path. Classifying what it
        // resolves to fails, and the operator must see for which symlink.
        unix_fs::symlink(dir.path().join("missing"), &link)?;

        let changes = SymlinkChanges {
            create: vec![(link.clone(), source)],
            remove: Vec::new(),
            change: Vec::new(),
        };
        let err = reconcile_config_symlinks(apps.path(), &changes).unwrap_err();
        let msg = err.to_string();
        let expected = format!("failed to create symlink at {}", link.display());
        assert!(
            msg.contains(&expected) && msg.contains("failed to inspect"),
            "error indicates the symlink and the failing operation: {msg}",
        );
        Ok(())
    }

    #[test]
    fn manifest_symlinks_removal_of_a_directory_indicates_the_path() -> Result<()> {
        let dir = TempDir::new("symlinks");
        let apps = TempDir::new("apps");
        // The previous manifest had a symlink here, but a real directory
        // occupies the path now. Removal fails; the error must indicate the
        // path so the operator can find the obstruction.
        let stray = dir.path().join("managed.conf");
        fs::create_dir(&stray)?;

        let changes = SymlinkChanges {
            create: Vec::new(),
            remove: vec![stray.clone()],
            change: Vec::new(),
        };
        let err = reconcile_config_symlinks(apps.path(), &changes).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("failed to remove") && msg.contains(&stray.display().to_string()),
            "error indicates the path whose removal failed: {err}",
        );
        Ok(())
    }

    #[test]
    fn manifest_symlinks_removes_managed_but_refuses_unmanaged() -> Result<()> {
        let dir = TempDir::new("symlinks");
        let apps = TempDir::new("apps");
        let managed = dir.path().join("managed.conf");
        let unmanaged = dir.path().join("unmanaged.conf");
        unix_fs::symlink(apps.path().join("nginx/current/x"), &managed)?;
        unix_fs::symlink("/usr/share/something", &unmanaged)?;

        // Managed symlink is removed.
        let changes = SymlinkChanges {
            create: Vec::new(),
            remove: vec![managed.clone()],
            change: Vec::new(),
        };
        reconcile_config_symlinks(apps.path(), &changes)?;
        assert!(managed.symlink_metadata().is_err());

        // Unmanaged symlink is refused.
        let changes = SymlinkChanges {
            create: Vec::new(),
            remove: vec![unmanaged],
            change: Vec::new(),
        };
        let err = reconcile_config_symlinks(apps.path(), &changes).unwrap_err();
        assert!(
            err.to_string().contains("refusing to touch"),
            "error explains the refusal: {err}",
        );
        Ok(())
    }

    #[test]
    fn manifest_symlinks_changes_managed_but_refuses_unmanaged() -> Result<()> {
        let dir = TempDir::new("symlinks");
        let apps = TempDir::new("apps");
        let managed = dir.path().join("managed.conf");
        let unmanaged = dir.path().join("unmanaged.conf");
        let new_source = apps.path().join("nginx/current/new.conf");
        unix_fs::symlink(apps.path().join("nginx/current/old.conf"), &managed)?;
        unix_fs::symlink("/usr/share/something", &unmanaged)?;

        // Managed symlink is updated.
        let changes = SymlinkChanges {
            create: Vec::new(),
            remove: Vec::new(),
            change: vec![(managed.clone(), new_source.clone())],
        };
        reconcile_config_symlinks(apps.path(), &changes)?;
        assert_eq!(fs::read_link(&managed)?, new_source);

        // Unmanaged symlink is refused.
        let changes = SymlinkChanges {
            create: Vec::new(),
            remove: Vec::new(),
            change: vec![(unmanaged, new_source)],
        };
        let err = reconcile_config_symlinks(apps.path(), &changes).unwrap_err();
        assert!(
            err.to_string().contains("refusing to touch"),
            "error explains the refusal: {err}",
        );
        Ok(())
    }
}

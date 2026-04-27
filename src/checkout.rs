// Deptool -- A declarative configuration deployment tool.
// Copyright 2026 Ruud van Asseldonk

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// A copy of the License has been included in the root of the repository.

//! Host filesystem operations: app checkout, symlink reconciliation, and cleanup.

use std::collections::{BTreeMap, BTreeSet};
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
/// `previous` always points to whatever `current` pointed to before this
/// call. After a normal deploy this is the last successful version. After
/// a rollback this is the *failed* version (useful for debugging), not the
/// previous successful one.
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
            fs::remove_dir_all(&version_dir)?;
        }
        fs::create_dir_all(&version_dir)?;
        store.checkout_app(commit_oid, host, app, &version_dir)?;
    }

    let next = app_dir.join("next");
    let current = app_dir.join("current");

    if next.symlink_metadata().is_ok() {
        fs::remove_file(&next)?;
    }
    unix_fs::symlink(&prefix, &next)?;

    // Rotate current → previous before overwriting current.
    if current.symlink_metadata().is_ok() {
        fs::rename(&current, app_dir.join("previous"))?;
    }
    fs::rename(&next, &current)?;

    Ok(())
}

/// Remove an app: delete its directory under apps_dir.
pub fn remove_app(app: &str, apps_dir: &Path) -> Result<()> {
    let app_dir = apps_dir.join(app);
    if app_dir.exists() {
        fs::remove_dir_all(&app_dir)?;
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
                checkout_app(store, oid, host, app, apps_dir, mode)?;
            }
            AppDiff::Update { .. } => {
                log(format_args!("updating app {app}"));
                let oid = commit_oid.expect("Add/Update requires a target commit");
                checkout_app(store, oid, host, app, apps_dir, mode)?;
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
/// touching symlinks that point into `apps_dir` (i.e., ones we created).
/// Returns the set of names whose symlinks were added or changed.
pub fn reconcile_managed_symlinks(
    desired: &BTreeMap<String, PathBuf>,
    apps_dir: &Path,
    target_dir: &Path,
) -> Result<BTreeSet<String>> {
    let actual = collect_managed_symlinks(apps_dir, target_dir)?;
    let mut changed = BTreeSet::new();

    for name in actual.keys() {
        if !desired.contains_key(name) {
            fs::remove_file(target_dir.join(name))?;
        }
    }

    // The directory may not exist yet (e.g. /etc/sysusers.d on a fresh
    // system). Create it on first use rather than requiring provisioning.
    if !desired.is_empty() && !target_dir.exists() {
        fs::create_dir_all(target_dir)?;
        fs::set_permissions(target_dir, fs::Permissions::from_mode(0o755))?;
    }

    for (name, desired_target) in desired {
        let needs_update = match actual.get(name) {
            None => true,
            Some(actual_target) => actual_target != desired_target,
        };
        if needs_update {
            let link_path = target_dir.join(name);
            if link_path.exists() || link_path.symlink_metadata().is_ok() {
                fs::remove_file(&link_path)?;
            }
            unix_fs::symlink(desired_target, &link_path)?;
            changed.insert(name.clone());
        }
    }

    Ok(changed)
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
            fs::remove_file(target)?;
        }
    }

    for (target, source) in &changes.change {
        if target.symlink_metadata().is_ok() {
            verify_managed(target, apps_dir)?;
            fs::remove_file(target)?;
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
/// If a file already exists at `link` with identical contents to `source`,
/// it is replaced with the symlink. This supports incremental adoption:
/// config files already present on the host can be moved under Deptool
/// management without manual intervention.
fn create_symlink(source: &Path, link: &Path) -> Result<()> {
    let link_name = link.display().to_string();
    match unix_fs::symlink(source, link) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            if files_match(source, link)? {
                fs::remove_file(link)?;
                unix_fs::symlink(source, link)?;
                Ok(())
            } else {
                Err(ApplyError::SymlinkFailed {
                    link: link_name,
                    cause: "a file with different contents already exists; \
                            remove it manually and retry"
                        .into(),
                })
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

/// Check whether two files have identical contents, without reading them
/// entirely into memory.
fn files_match(a: &Path, b: &Path) -> Result<bool> {
    use std::io::{BufReader, Read};
    if fs::metadata(a)?.len() != fs::metadata(b)?.len() {
        return Ok(false);
    }
    let mut a = BufReader::new(fs::File::open(a)?);
    let mut b = BufReader::new(fs::File::open(b)?);
    let mut buf_a = [0u8; 8192];
    let mut buf_b = [0u8; 8192];
    loop {
        let n = a.read(&mut buf_a)?;
        b.read_exact(&mut buf_b[..n])?;
        if buf_a[..n] != buf_b[..n] {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
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

/// Collect symlinks in `dir` that point into `apps_dir`.
///
/// We create absolute symlinks, so we just check whether the raw symlink
/// target starts with apps_dir. No canonicalization needed.
fn collect_managed_symlinks(
    apps_dir: &Path,
    dir: &Path,
) -> Result<BTreeMap<String, std::path::PathBuf>> {
    let mut units = BTreeMap::new();

    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(units),
        Err(e) => return Err(e.into()),
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_symlink() {
            continue;
        }
        let target = fs::read_link(&path)?;
        if target.starts_with(apps_dir) {
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| ApplyError::SymlinkFailed {
                link: entry.path().display().to_string(),
                cause: "file name is not valid UTF-8".into(),
            })?;
            units.insert(name.to_string(), target);
        }
    }

    Ok(units)
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

        let changed = reconcile_managed_symlinks(&desired, apps.path(), units.path())?;

        assert_eq!(changed, BTreeSet::from(["nginx.service".to_string()]));
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

    #[test]
    fn reconcile_managed_symlinks_removes_symlink_not_in_desired_set() -> Result<()> {
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        let target = apps.path().join("nginx/current/systemd/nginx.service");

        // Create a symlink as if a previous reconcile put it there.
        unix_fs::symlink(&target, units.path().join("nginx.service"))?;

        let changed = reconcile_managed_symlinks(&BTreeMap::new(), apps.path(), units.path())?;

        assert!(changed.is_empty());
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

        let changed = reconcile_managed_symlinks(&BTreeMap::new(), apps.path(), units.path())?;

        assert!(changed.is_empty());
        assert!(units.path().join("sshd.service").is_symlink());
        Ok(())
    }

    #[test]
    fn reconcile_managed_symlinks_produces_no_changes_when_already_in_sync() -> Result<()> {
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        let target = apps.path().join("nginx/current/systemd/nginx.service");
        let desired = BTreeMap::from([("nginx.service".to_string(), target)]);

        reconcile_managed_symlinks(&desired, apps.path(), units.path())?;
        let changed = reconcile_managed_symlinks(&desired, apps.path(), units.path())?;

        assert!(changed.is_empty());
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
        assert_eq!(changes.units.enable, vec!["nginx.service"]);
        assert!(changes.units.restart.is_empty());
        assert!(changes.units.disable.is_empty());
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

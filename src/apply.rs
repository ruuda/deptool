//! Materialize apps on the target host.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};

use git2::Oid;

use crate::error::ApplyError;
use crate::plan::{AppDiff, SymlinkChanges, SystemDiff, compute_system_diff};
use crate::prim::Hostname;
use crate::store::Store;

type Result<T> = std::result::Result<T, ApplyError>;

/// Map from unit filename to the absolute symlink target path.
pub type DesiredUnits = BTreeMap<String, PathBuf>;

const OID_PREFIX_LEN: usize = 10;

/// Truncate an oid to a short prefix for use in directory names.
fn oid_prefix(oid: Oid) -> String {
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

/// Check out an app and atomically swap the `current` symlink.
///
/// In `Fresh` mode, any existing version directory is removed and
/// re-checked out (it may be from an interrupted deploy). In `Reuse`
/// mode, an existing directory is trusted and only the symlink is swapped.
fn apply_app(
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

    // Atomic symlink swap: create temp symlink, rename over `current`.
    let current = app_dir.join("current");
    let next = app_dir.join("next");
    if next.exists() {
        fs::remove_file(&next)?;
    }
    unix_fs::symlink(&prefix, &next)?;
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

/// Compute per-app diffs and the aggregate system diff for a host deploy.
///
/// Either commit can be None to represent an empty host (no apps).
pub fn diff_host(
    store: &Store,
    host: &Hostname,
    apps_dir: &Path,
    current_commit: Option<Oid>,
    target_commit: Option<Oid>,
) -> Result<(BTreeMap<String, AppDiff>, SystemDiff<PathBuf>)> {
    let get_apps = |oid| -> Result<BTreeMap<String, Oid>> {
        let tree = store.get_commit_tree(oid)?;
        Ok(store.get_host_apps(&tree, host)?)
    };
    let current_apps = current_commit
        .map(get_apps)
        .transpose()?
        .unwrap_or_default();
    let target_apps = target_commit.map(get_apps).transpose()?.unwrap_or_default();
    let app_diffs = crate::plan::diff_apps(&current_apps, &target_apps);

    let mut system = SystemDiff::<PathBuf>::default();
    for (app, change) in &app_diffs {
        let mut resolved = compute_system_diff(store, change)?.resolve_symlinks(app, apps_dir);
        system.append(&mut resolved);
    }

    Ok((app_diffs, system))
}

/// Checkout or remove each changed app on the filesystem.
///
/// This is the only part of a deploy that mutates `apps_dir`. It does not
/// touch refs, systemd, or host-level symlinks.
pub fn apply_checkout(
    store: &Store,
    commit_oid: Option<Oid>,
    app_diffs: &BTreeMap<String, AppDiff>,
    host: &Hostname,
    apps_dir: &Path,
    mode: CheckoutMode,
) -> Result<()> {
    for (app, change) in app_diffs {
        match change {
            AppDiff::Add { .. } | AppDiff::Update { .. } => {
                let oid = commit_oid.expect("Add/Update requires a target commit");
                apply_app(store, oid, host, app, apps_dir, mode)?;
            }
            AppDiff::Remove { .. } => {
                remove_app(app, apps_dir)?;
            }
        }
    }
    Ok(())
}

/// Reconcile unit symlinks: make unit_dir match desired.
///
/// Returns the set of unit names whose symlinks were added or changed.
pub fn reconcile_symlinks(
    desired: &DesiredUnits,
    apps_dir: &Path,
    unit_dir: &Path,
) -> Result<BTreeSet<String>> {
    let actual = collect_actual_units(apps_dir, unit_dir)?;
    let mut changed = BTreeSet::new();

    for name in actual.keys() {
        if !desired.contains_key(name) {
            fs::remove_file(unit_dir.join(name))?;
        }
    }

    for (name, desired_target) in desired {
        let needs_update = match actual.get(name) {
            None => true,
            Some(actual_target) => actual_target != desired_target,
        };
        if needs_update {
            let link_path = unit_dir.join(name);
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
pub fn reconcile_manifest_symlinks(
    apps_dir: &Path,
    changes: &SymlinkChanges<PathBuf>,
) -> Result<()> {
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

/// Collect actual unit symlinks in unit_dir that point into apps_dir.
///
/// We create absolute symlinks, so we just check whether the raw symlink
/// target starts with apps_dir. No canonicalization needed.
fn collect_actual_units(
    apps_dir: &Path,
    unit_dir: &Path,
) -> Result<BTreeMap<String, std::path::PathBuf>> {
    let mut units = BTreeMap::new();

    let entries = match fs::read_dir(unit_dir) {
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
    use crate::plan::diff_enabled;
    use crate::testutil::{TempDir, TestRepo, assert_dir_contents};

    // Tests do both apply calls (-> ApplyError) and raw git operations
    // (-> git2::Error), so use the top-level Result that accepts both.
    use crate::error::Result;

    #[test]
    fn apply_app_creates_versioned_checkout_and_current_symlink() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/nginx.conf", b"server {}")]);

        let apps = TempDir::new("apps");
        apply_app(
            &t.store,
            c1,
            &"web1".into(),
            "nginx",
            apps.path(),
            CheckoutMode::Fresh,
        )?;

        let prefix = oid_prefix(c1);
        let version_dir = apps.path().join("nginx").join(&prefix);
        assert!(version_dir.join("nginx.conf").exists(), "checkout exists");

        let current = apps.path().join("nginx/current");
        let target = fs::read_link(&current)?;
        assert_eq!(target.to_str().expect("target is utf-8"), prefix);

        Ok(())
    }

    #[test]
    fn apply_app_replaces_existing_checkout() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/nginx.conf", b"v1")]);

        let apps = TempDir::new("apps");

        // Create a partial/corrupt checkout dir.
        let prefix = oid_prefix(c1);
        let corrupt_dir = apps.path().join("nginx").join(&prefix);
        fs::create_dir_all(&corrupt_dir)?;
        fs::write(corrupt_dir.join("garbage"), "bad")?;

        apply_app(
            &t.store,
            c1,
            &"web1".into(),
            "nginx",
            apps.path(),
            CheckoutMode::Fresh,
        )?;

        assert_dir_contents(&corrupt_dir, &[("nginx.conf", b"v1")]);

        Ok(())
    }

    #[test]
    fn apply_app_swaps_symlink_on_update() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/nginx.conf", b"v1")]);
        let c2 = t.commit(&[("web1/nginx/nginx.conf", b"v2")]);

        let apps = TempDir::new("apps");
        let current = apps.path().join("nginx/current");

        apply_app(
            &t.store,
            c1,
            &"web1".into(),
            "nginx",
            apps.path(),
            CheckoutMode::Fresh,
        )?;
        let target = fs::read_link(&current)?;
        assert_eq!(target.to_str().expect("target is utf-8"), oid_prefix(c1));

        apply_app(
            &t.store,
            c2,
            &"web1".into(),
            "nginx",
            apps.path(),
            CheckoutMode::Fresh,
        )?;
        let target = fs::read_link(&current)?;
        assert_eq!(target.to_str().expect("target is utf-8"), oid_prefix(c2));

        // Both versions still exist on disk.
        assert!(apps.path().join("nginx").join(oid_prefix(c1)).exists());
        assert!(apps.path().join("nginx").join(oid_prefix(c2)).exists());

        Ok(())
    }

    #[test]
    fn remove_app_deletes_the_app_directory() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/nginx.conf", b"v1")]);

        let apps = TempDir::new("apps");
        apply_app(
            &t.store,
            c1,
            &"web1".into(),
            "nginx",
            apps.path(),
            CheckoutMode::Fresh,
        )?;
        assert!(apps.path().join("nginx").exists());

        remove_app("nginx", apps.path())?;
        assert!(!apps.path().join("nginx").exists());

        Ok(())
    }

    #[test]
    fn remove_app_succeeds_when_app_does_not_exist() -> Result<()> {
        let apps = TempDir::new("apps");
        remove_app("nonexistent", apps.path())?;
        Ok(())
    }

    // reconcile_symlinks tests (which need filesystem access)

    #[test]
    fn reconcile_symlinks_creates_symlink_for_desired_unit() -> Result<()> {
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        let target = apps.path().join("nginx/current/systemd/nginx.service");
        let desired = BTreeMap::from([("nginx.service".to_string(), target)]);

        let changed = reconcile_symlinks(&desired, apps.path(), units.path())?;

        assert_eq!(changed, BTreeSet::from(["nginx.service".to_string()]));
        assert!(units.path().join("nginx.service").is_symlink());
        Ok(())
    }

    #[test]
    fn reconcile_symlinks_removes_symlink_not_in_desired_set() -> Result<()> {
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        let target = apps.path().join("nginx/current/systemd/nginx.service");

        // Create a symlink as if a previous reconcile put it there.
        unix_fs::symlink(&target, units.path().join("nginx.service"))?;

        let changed = reconcile_symlinks(&BTreeMap::new(), apps.path(), units.path())?;

        assert!(changed.is_empty());
        assert!(!units.path().join("nginx.service").exists());
        Ok(())
    }

    #[test]
    fn reconcile_symlinks_leaves_unmanaged_symlinks_intact() -> Result<()> {
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");

        // A symlink that doesn't point into apps_dir, not ours.
        unix_fs::symlink(
            "/usr/lib/systemd/system/sshd.service",
            units.path().join("sshd.service"),
        )?;

        let changed = reconcile_symlinks(&BTreeMap::new(), apps.path(), units.path())?;

        assert!(changed.is_empty());
        assert!(units.path().join("sshd.service").is_symlink());
        Ok(())
    }

    #[test]
    fn reconcile_symlinks_produces_no_changes_when_already_in_sync() -> Result<()> {
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        let target = apps.path().join("nginx/current/systemd/nginx.service");
        let desired = BTreeMap::from([("nginx.service".to_string(), target)]);

        reconcile_symlinks(&desired, apps.path(), units.path())?;
        let changed = reconcile_symlinks(&desired, apps.path(), units.path())?;

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

    /// Test harness for `diff_host` + `apply_checkout` + `reconcile_symlinks`.
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
            apply_checkout(
                &self.repo.store,
                Some(commit),
                &app_diffs,
                host,
                self.apps.path(),
                CheckoutMode::Fresh,
            )?;

            // Reconcile here so tests that check unit symlinks still work.
            let desired = self
                .repo
                .store
                .desired_units(commit, host, self.apps.path())?;
            reconcile_symlinks(&desired, self.apps.path(), self.units.path())?;
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
        reconcile_manifest_symlinks(apps.path(), &changes)?;
        assert_eq!(fs::read_link(&link)?, source);

        // Clean up for the next case.
        fs::remove_file(&link)?;

        // A regular file with identical contents is adopted.
        fs::write(&link, b"config data")?;
        reconcile_manifest_symlinks(apps.path(), &changes)?;
        assert!(link.is_symlink());
        assert_eq!(fs::read_link(&link)?, source);

        // Clean up for the next case.
        fs::remove_file(&link)?;

        // A regular file with different contents is refused.
        fs::write(&link, b"different data")?;
        let err = reconcile_manifest_symlinks(apps.path(), &changes).unwrap_err();
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
        reconcile_manifest_symlinks(apps.path(), &changes)?;
        assert!(managed.symlink_metadata().is_err());

        // Unmanaged symlink is refused.
        let changes = SymlinkChanges {
            create: Vec::new(),
            remove: vec![unmanaged],
            change: Vec::new(),
        };
        let err = reconcile_manifest_symlinks(apps.path(), &changes).unwrap_err();
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
        reconcile_manifest_symlinks(apps.path(), &changes)?;
        assert_eq!(fs::read_link(&managed)?, new_source);

        // Unmanaged symlink is refused.
        let changes = SymlinkChanges {
            create: Vec::new(),
            remove: Vec::new(),
            change: vec![(unmanaged, new_source)],
        };
        let err = reconcile_manifest_symlinks(apps.path(), &changes).unwrap_err();
        assert!(
            err.to_string().contains("refusing to touch"),
            "error explains the refusal: {err}",
        );
        Ok(())
    }
}

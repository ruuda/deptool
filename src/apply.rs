//! Materialize apps on the target host.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs as unix_fs;
use std::path::Path;

use crate::error::Result;
use crate::plan::AppDiff;
use crate::store;

const OID_PREFIX_LEN: usize = 10;

/// Truncate a git2::Oid to a short prefix for use in directory names.
fn oid_prefix(oid: git2::Oid) -> String {
    oid.to_string()[..OID_PREFIX_LEN].to_string()
}

/// Check out an app and atomically swap the `current` symlink.
///
/// The app tree is checked out into `<apps_dir>/<app>/<oid-prefix>/`.
/// If that directory already exists (e.g. interrupted deploy), it is removed
/// and re-checked out. Then `<apps_dir>/<app>/current` is atomically swapped
/// to point to the new checkout.
pub fn apply_app(
    repo: &git2::Repository,
    commit_oid: git2::Oid,
    host: &str,
    app: &str,
    apps_dir: &Path,
) -> Result<()> {
    let prefix = oid_prefix(commit_oid);
    let app_dir = apps_dir.join(app);
    let version_dir = app_dir.join(&prefix);

    // Remove and re-checkout to avoid trusting a potentially incomplete dir.
    if version_dir.exists() {
        fs::remove_dir_all(&version_dir)?;
    }
    fs::create_dir_all(&version_dir)?;
    crate::store::checkout_app(repo, commit_oid, host, app, &version_dir)?;

    // Atomic symlink swap: create temp symlink, rename over `current`.
    let current = app_dir.join("current");
    let tmp = app_dir.join(".current.tmp");
    if tmp.exists() {
        fs::remove_file(&tmp)?;
    }
    unix_fs::symlink(&prefix, &tmp)?;
    fs::rename(&tmp, &current)?;

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

/// Apply a deployment to a host's filesystem.
///
/// Sets the target ref, diffs current vs target apps, checks out or removes
/// each changed app, reconciles unit symlinks, and sets the current ref.
/// Calls `on_app` for each changed app. Returns the list of changed unit names.
pub fn apply_host(
    repo: &git2::Repository,
    commit_oid: git2::Oid,
    actual_current: Option<git2::Oid>,
    host: &str,
    apps_dir: &Path,
    unit_dir: &Path,
    mut on_app: impl FnMut(&str, &AppDiff),
) -> Result<Vec<String>> {
    store::set_ref(repo, "refs/heads/target", commit_oid, store::RefUpdate::SetTarget)?;

    let target_tree = repo.find_commit(commit_oid)?.tree()?;
    let target_apps = store::get_host_apps(repo, &target_tree, host)?;

    let current_apps = match actual_current {
        None => BTreeMap::new(),
        Some(oid) => {
            let tree = repo.find_commit(oid)?.tree()?;
            store::get_host_apps(repo, &tree, host)?
        }
    };

    let diff = crate::plan::diff_apps(&current_apps, &target_apps);

    for (app, change) in &diff {
        match change {
            AppDiff::Add { .. } | AppDiff::Update { .. } => {
                apply_app(repo, commit_oid, host, app, apps_dir)?;
            }
            AppDiff::Remove => {
                remove_app(app, apps_dir)?;
            }
        }
        on_app(app, change);
    }

    let changed_units = reconcile_units(apps_dir, unit_dir)?;

    store::set_ref(repo, "refs/heads/current", commit_oid, store::RefUpdate::SetCurrent)?;

    Ok(changed_units)
}

/// Reconcile systemd unit symlinks.
///
/// Scans `unit_dir` for symlinks pointing into `apps_dir` (these are ours).
/// Collects all unit files from `<apps_dir>/*/current/`. Creates missing
/// symlinks and removes stale ones. Returns the list of unit names that
/// were added or removed (i.e. units that need daemon-reload + restart).
pub fn reconcile_units(apps_dir: &Path, unit_dir: &Path) -> Result<Vec<String>> {
    let mut changed = Vec::new();

    // Desired: all unit files across all apps.
    let desired = collect_desired_units(apps_dir)?;

    // Actual: symlinks in unit_dir that point into apps_dir.
    let actual = collect_actual_units(apps_dir, unit_dir)?;

    // Remove stale symlinks.
    for name in actual.keys() {
        if !desired.contains_key(name) {
            fs::remove_file(unit_dir.join(name))?;
            changed.push(name.clone());
        }
    }

    // Create or update symlinks.
    for (name, desired_target) in &desired {
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
            changed.push(name.clone());
        }
    }

    Ok(changed)
}

/// Collect desired unit files from all apps' `current/` directories.
/// Returns a map from unit filename to the symlink target path.
fn collect_desired_units(apps_dir: &Path) -> Result<BTreeMap<String, std::path::PathBuf>> {
    let mut units = BTreeMap::new();

    let entries = match fs::read_dir(apps_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(units),
        Err(e) => return Err(e.into()),
    };

    for entry in entries {
        let entry = entry?;
        let current = entry.path().join("current");
        if !current.exists() {
            continue;
        }
        let dir_entries = match fs::read_dir(&current) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for file in dir_entries {
            let file = file?;
            let name = file.file_name().to_string_lossy().to_string();
            if is_systemd_unit(&name) {
                units.insert(name, file.path());
            }
        }
    }

    Ok(units)
}

/// Collect actual unit symlinks in unit_dir that point into apps_dir.
///
/// We check the raw symlink target (not canonicalized) because the target
/// may no longer exist (e.g. the app was removed). We resolve just enough
/// to compare path prefixes.
fn collect_actual_units(
    apps_dir: &Path,
    unit_dir: &Path,
) -> Result<BTreeMap<String, std::path::PathBuf>> {
    let mut units = BTreeMap::new();
    let apps_canonical = match apps_dir.canonicalize() {
        Ok(p) => p,
        Err(_) => return Ok(units),
    };

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
        // Resolve relative symlinks against unit_dir, but don't canonicalize
        // (the target may not exist).
        let resolved = if target.is_absolute() {
            target.clone()
        } else {
            unit_dir.join(&target)
        };
        // Normalize by canonicalizing parent components that do exist.
        // A symlink like /units/nginx.service -> /apps/nginx/current/nginx.service
        // has parent /apps/nginx/current which resolves through the symlink.
        // But if the app is removed, even the parent won't exist. In that case,
        // check if the raw path textually starts with apps_dir.
        let is_ours = if let Ok(canonical) = resolved.canonicalize() {
            canonical.starts_with(&apps_canonical)
        } else {
            // Target doesn't exist. Check the raw resolved path against the
            // canonical apps_dir. This handles the case where an app was removed
            // but its unit symlink still lingers.
            resolved.starts_with(&apps_canonical)
        };
        if is_ours {
            let name = entry.file_name().to_string_lossy().to_string();
            units.insert(name, target);
        }
    }

    Ok(units)
}

fn is_systemd_unit(name: &str) -> bool {
    let extensions = [".service", ".timer", ".socket", ".path", ".mount"];
    extensions.iter().any(|ext| name.ends_with(ext))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Result;
    use crate::testutil::{TempDir, commit_files};

    #[test]
    fn apply_app_creates_versioned_checkout_and_current_symlink() -> Result<()> {
        let store = TempDir::new("store");
        let repo = git2::Repository::init_bare(store.path())?;
        let c1 = commit_files(&repo, &[("web1/nginx/nginx.conf", b"server {}")])?;

        let apps = TempDir::new("apps");
        apply_app(&repo, c1, "web1", "nginx", apps.path())?;

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
        let store = TempDir::new("store");
        let repo = git2::Repository::init_bare(store.path())?;
        let c1 = commit_files(&repo, &[("web1/nginx/nginx.conf", b"v1")])?;

        let apps = TempDir::new("apps");

        // Create a partial/corrupt checkout dir.
        let prefix = oid_prefix(c1);
        let corrupt_dir = apps.path().join("nginx").join(&prefix);
        fs::create_dir_all(&corrupt_dir)?;
        fs::write(corrupt_dir.join("garbage"), "bad")?;

        apply_app(&repo, c1, "web1", "nginx", apps.path())?;

        assert!(!corrupt_dir.join("garbage").exists(), "old file is gone");
        let contents = fs::read_to_string(corrupt_dir.join("nginx.conf"))?;
        assert_eq!(contents, "v1");

        Ok(())
    }

    #[test]
    fn apply_app_swaps_symlink_atomically_on_update() -> Result<()> {
        let store = TempDir::new("store");
        let repo = git2::Repository::init_bare(store.path())?;
        let c1 = commit_files(&repo, &[("web1/nginx/nginx.conf", b"v1")])?;
        let c2 = commit_files(&repo, &[("web1/nginx/nginx.conf", b"v2")])?;

        let apps = TempDir::new("apps");
        apply_app(&repo, c1, "web1", "nginx", apps.path())?;
        apply_app(&repo, c2, "web1", "nginx", apps.path())?;

        let current = apps.path().join("nginx/current");
        let target = fs::read_link(&current)?;
        assert_eq!(
            target.to_str().expect("target is utf-8"),
            oid_prefix(c2),
        );

        // Both versions still exist on disk.
        assert!(apps.path().join("nginx").join(oid_prefix(c1)).exists());
        assert!(apps.path().join("nginx").join(oid_prefix(c2)).exists());

        Ok(())
    }

    #[test]
    fn remove_app_deletes_the_app_directory() -> Result<()> {
        let store = TempDir::new("store");
        let repo = git2::Repository::init_bare(store.path())?;
        let c1 = commit_files(&repo, &[("web1/nginx/nginx.conf", b"v1")])?;

        let apps = TempDir::new("apps");
        apply_app(&repo, c1, "web1", "nginx", apps.path())?;
        assert!(apps.path().join("nginx").exists());

        remove_app("nginx", apps.path())?;
        assert!(!apps.path().join("nginx").exists());

        Ok(())
    }

    #[test]
    fn remove_app_is_idempotent() -> Result<()> {
        let apps = TempDir::new("apps");
        remove_app("nonexistent", apps.path())?;
        Ok(())
    }

    /// Set up an app with a current symlink containing the given files.
    fn setup_app(apps_dir: &Path, app: &str, files: &[&str]) {
        let version_dir = apps_dir.join(app).join("v1");
        fs::create_dir_all(&version_dir).unwrap();
        for file in files {
            fs::write(version_dir.join(file), "").unwrap();
        }
        let current = apps_dir.join(app).join("current");
        unix_fs::symlink("v1", &current).unwrap();
    }

    #[test]
    fn reconcile_creates_symlinks_for_new_units() -> Result<()> {
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        setup_app(apps.path(), "nginx", &["nginx.service", "nginx.conf"]);

        let changed = reconcile_units(apps.path(), units.path())?;

        assert_eq!(changed, vec!["nginx.service"]);
        let link = units.path().join("nginx.service");
        assert!(link.is_symlink());
        Ok(())
    }

    #[test]
    fn reconcile_removes_stale_symlinks_for_removed_app() -> Result<()> {
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");

        // Deploy an app, reconcile to create symlinks.
        setup_app(apps.path(), "nginx", &["nginx.service"]);
        reconcile_units(apps.path(), units.path())?;
        assert!(units.path().join("nginx.service").exists());

        // Remove the app, reconcile again.
        fs::remove_dir_all(apps.path().join("nginx"))?;
        let changed = reconcile_units(apps.path(), units.path())?;

        assert_eq!(changed, vec!["nginx.service"]);
        assert!(!units.path().join("nginx.service").exists());
        Ok(())
    }

    #[test]
    fn reconcile_ignores_non_unit_files() -> Result<()> {
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        setup_app(apps.path(), "nginx", &["nginx.conf", "env"]);

        let changed = reconcile_units(apps.path(), units.path())?;

        assert!(changed.is_empty());
        Ok(())
    }

    #[test]
    fn reconcile_ignores_unmanaged_symlinks() -> Result<()> {
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");

        // A symlink that doesn't point into apps_dir — not ours.
        unix_fs::symlink("/usr/lib/systemd/system/sshd.service", units.path().join("sshd.service"))?;

        let changed = reconcile_units(apps.path(), units.path())?;

        assert!(changed.is_empty());
        assert!(units.path().join("sshd.service").is_symlink());
        Ok(())
    }

    #[test]
    fn reconcile_is_convergent() -> Result<()> {
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        setup_app(apps.path(), "nginx", &["nginx.service"]);

        reconcile_units(apps.path(), units.path())?;
        // Running again with no changes returns empty.
        let changed = reconcile_units(apps.path(), units.path())?;

        assert!(changed.is_empty());
        Ok(())
    }

    #[test]
    fn apply_host_sets_target_and_current_refs() -> Result<()> {
        let store = TempDir::new("store");
        let repo = git2::Repository::init_bare(store.path())?;
        let c1 = commit_files(&repo, &[("web1/nginx/conf", b"v1")])?;

        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        apply_host(&repo, c1, None, "web1", apps.path(), units.path(), |_, _| {})?;

        let current = repo.find_reference("refs/heads/current")?.peel_to_commit()?.id();
        let target = repo.find_reference("refs/heads/target")?.peel_to_commit()?.id();
        assert_eq!(current, c1);
        assert_eq!(target, c1);
        Ok(())
    }

    #[test]
    fn apply_host_reports_per_app_changes() -> Result<()> {
        let store = TempDir::new("store");
        let repo = git2::Repository::init_bare(store.path())?;
        let c1 = commit_files(&repo, &[("web1/nginx/nginx.conf", b"v1")])?;

        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        let mut applied = Vec::new();
        apply_host(&repo, c1, None, "web1", apps.path(), units.path(), |app, diff| {
            applied.push((app.to_string(), diff.clone()));
        })?;

        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].0, "nginx");
        assert!(matches!(applied[0].1, AppDiff::Add { .. }));
        Ok(())
    }
}

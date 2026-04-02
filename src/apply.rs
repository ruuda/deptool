//! Materialize apps on the target host.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs as unix_fs;
use std::path::Path;

use crate::error::Result;
use crate::plan::{AppDiff, SystemdConfig, UnitChanges, diff_enabled};
use crate::prim::Hostname;
use crate::store;

const OID_PREFIX_LEN: usize = 10;

/// Truncate an oid to a short prefix for use in directory names.
fn oid_prefix(oid: git2::Oid) -> String {
    let mut buf = oid.to_string();
    buf.truncate(OID_PREFIX_LEN);
    buf
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
    host: &Hostname,
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
    store::checkout_app(repo, commit_oid, host, app, &version_dir)?;

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

/// Apply a deployment to a host's filesystem.
///
/// Sets the target ref, diffs current vs target apps, checks out or removes
/// each changed app, reconciles unit symlinks, and sets the current ref.
/// Calls `on_app` for each changed app.
pub fn apply_host(
    repo: &git2::Repository,
    commit_oid: git2::Oid,
    actual_current: Option<git2::Oid>,
    host: &Hostname,
    apps_dir: &Path,
    unit_dir: &Path,
    mut on_app: impl FnMut(&str, &AppDiff),
) -> Result<UnitChanges> {
    store::set_ref(
        repo,
        "refs/heads/target",
        commit_oid,
        store::RefUpdate::SetTarget,
    )?;

    let target_tree = repo.find_commit(commit_oid)?.tree()?;
    let target_apps = store::get_host_apps(repo, &target_tree, host)?;

    let (current_tree, current_apps) = match actual_current {
        None => (None, BTreeMap::new()),
        Some(oid) => {
            let tree = repo.find_commit(oid)?.tree()?;
            let apps = store::get_host_apps(repo, &tree, host)?;
            (Some(tree), apps)
        }
    };

    let diff = crate::plan::diff_apps(&current_apps, &target_apps);

    for (app, change) in &diff {
        match change {
            AppDiff::Add { .. } | AppDiff::Update { .. } => {
                apply_app(repo, commit_oid, host, app, apps_dir)?;
            }
            AppDiff::Remove { .. } => {
                remove_app(app, apps_dir)?;
            }
        }
        on_app(app, change);
    }

    // Make all units from the target tree available to systemd by
    // symlinking them into the unit dir. This is independent of whether
    // they are enabled or not.
    let desired_units = collect_desired_units(repo, &target_tree, host, apps_dir)?;
    reconcile_symlinks(&desired_units, apps_dir, unit_dir)?;

    // Compute unit lifecycle actions. We collect enabled units only from
    // changed apps: unchanged apps need no action. This means a unit in
    // both prev and target was enabled across a change, so we need to restart
    // it so it picks up its new config.
    let changed_apps: BTreeSet<&str> = diff.keys().map(|app| app.as_str()).collect();
    let prev_enabled = match &current_tree {
        None => BTreeSet::new(),
        Some(tree) => collect_enabled_units(repo, tree, host, &changed_apps)?,
    };
    let target_enabled = collect_enabled_units(repo, &target_tree, host, &changed_apps)?;
    let unit_changes = diff_enabled(&prev_enabled, &target_enabled);

    store::set_ref(
        repo,
        "refs/heads/current",
        commit_oid,
        store::RefUpdate::SetCurrent,
    )?;

    Ok(unit_changes)
}

/// Reconcile unit symlinks: make unit_dir match desired.
///
/// Returns the set of unit names whose symlinks were added or changed.
fn reconcile_symlinks(
    desired: &BTreeMap<String, std::path::PathBuf>,
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

/// Collect desired unit files by walking the Git tree.
///
/// For each app, checks for a `systemd/` subtree and maps unit filenames
/// to their absolute path under `apps_dir/<app>/current/systemd/`.
fn collect_desired_units(
    repo: &git2::Repository,
    config_tree: &git2::Tree,
    host: &Hostname,
    apps_dir: &Path,
) -> Result<BTreeMap<String, std::path::PathBuf>> {
    let apps = store::get_host_apps(repo, config_tree, host)?;
    let mut units = BTreeMap::new();
    for (app, app_tree_oid) in &apps {
        let app_tree = repo.find_tree(*app_tree_oid)?;
        let systemd_entry = match app_tree.get_name("systemd") {
            Some(entry) => entry,
            None => continue,
        };
        let systemd_tree = repo.find_tree(systemd_entry.id())?;
        for entry in systemd_tree.iter() {
            if let Some(name) = entry.name() {
                let target = apps_dir
                    .join(app)
                    .join("current")
                    .join("systemd")
                    .join(name);
                units.insert(name.to_string(), target);
            }
        }
    }
    Ok(units)
}

/// Collect enabled unit names from `systemd.json`, filtered to given apps.
///
/// If an app has no `systemd.json`, none of its units are enabled.
fn collect_enabled_units(
    repo: &git2::Repository,
    config_tree: &git2::Tree,
    host: &Hostname,
    filter_apps: &BTreeSet<&str>,
) -> Result<BTreeSet<String>> {
    let host_apps = store::get_host_apps(repo, config_tree, host)?;
    let mut enabled = BTreeSet::new();
    for (app, app_tree_oid) in &host_apps {
        if !filter_apps.contains(app.as_str()) {
            continue;
        }
        let app_tree = repo.find_tree(*app_tree_oid)?;
        let entry = match app_tree.get_name("systemd.json") {
            Some(entry) => entry,
            None => continue,
        };
        let blob = repo.find_blob(entry.id())?;
        let config: SystemdConfig = serde_json::from_slice(blob.content())?;
        enabled.extend(config.units_enabled);
    }
    Ok(enabled)
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
            let name = entry
                .file_name()
                .to_str()
                .ok_or(crate::error::Error::NonUtf8FileName)?
                .to_string();
            units.insert(name, target);
        }
    }

    Ok(units)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Result;
    use crate::testutil::{TempDir, assert_dir_contents, commit_files};

    #[test]
    fn apply_app_creates_versioned_checkout_and_current_symlink() -> Result<()> {
        let store = TempDir::new("store");
        let repo = git2::Repository::init_bare(store.path())?;
        let c1 = commit_files(&repo, &[("web1/nginx/nginx.conf", b"server {}")])?;

        let apps = TempDir::new("apps");
        apply_app(&repo, c1, &"web1".into(), "nginx", apps.path())?;

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

        apply_app(&repo, c1, &"web1".into(), "nginx", apps.path())?;

        assert_dir_contents(&corrupt_dir, &[("nginx.conf", b"v1")]);

        Ok(())
    }

    #[test]
    fn apply_app_swaps_symlink_on_update() -> Result<()> {
        let store = TempDir::new("store");
        let repo = git2::Repository::init_bare(store.path())?;
        let c1 = commit_files(&repo, &[("web1/nginx/nginx.conf", b"v1")])?;
        let c2 = commit_files(&repo, &[("web1/nginx/nginx.conf", b"v2")])?;

        let apps = TempDir::new("apps");
        let current = apps.path().join("nginx/current");

        apply_app(&repo, c1, &"web1".into(), "nginx", apps.path())?;
        let target = fs::read_link(&current)?;
        assert_eq!(target.to_str().expect("target is utf-8"), oid_prefix(c1));

        apply_app(&repo, c2, &"web1".into(), "nginx", apps.path())?;
        let target = fs::read_link(&current)?;
        assert_eq!(target.to_str().expect("target is utf-8"), oid_prefix(c2));

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
        apply_app(&repo, c1, &"web1".into(), "nginx", apps.path())?;
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

    #[test]
    fn apply_host_sets_target_and_current_refs() -> Result<()> {
        let store = TempDir::new("store");
        let repo = git2::Repository::init_bare(store.path())?;
        let c1 = commit_files(&repo, &[("web1/nginx/conf", b"v1")])?;

        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        let actual_current_commit = None;
        apply_host(
            &repo,
            c1,
            actual_current_commit,
            &"web1".into(),
            apps.path(),
            units.path(),
            |_, _| {},
        )?;

        let current = repo
            .find_reference("refs/heads/current")?
            .peel_to_commit()?
            .id();
        let target = repo
            .find_reference("refs/heads/target")?
            .peel_to_commit()?
            .id();
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
        let actual_current_commit = None;
        apply_host(
            &repo,
            c1,
            actual_current_commit,
            &"web1".into(),
            apps.path(),
            units.path(),
            |app, diff| {
                applied.push((app.to_string(), diff.clone()));
            },
        )?;

        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].0, "nginx");
        assert!(matches!(applied[0].1, AppDiff::Add { .. }));
        Ok(())
    }

    #[test]
    fn apply_host_only_enables_units_from_systemd_json() -> Result<()> {
        let store = TempDir::new("store");
        let repo = git2::Repository::init_bare(store.path())?;
        let systemd_json = br#"{"units_enabled": ["nginx.service"]}"#;
        let c1 = commit_files(
            &repo,
            &[
                ("web1/nginx/systemd/nginx.service", b"[Service]"),
                ("web1/nginx/systemd/nginx-reload.timer", b"[Timer]"),
                ("web1/nginx/systemd.json", systemd_json),
            ],
        )?;

        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        let actual_current_commit = None;
        let changes = apply_host(
            &repo,
            c1,
            actual_current_commit,
            &"web1".into(),
            apps.path(),
            units.path(),
            |_, _| {},
        )?;

        // Both units are symlinked (available).
        assert!(units.path().join("nginx.service").is_symlink());
        assert!(units.path().join("nginx-reload.timer").is_symlink());
        // Only the enabled one gets an enable action.
        assert_eq!(changes.enable, vec!["nginx.service"]);
        assert!(changes.restart.is_empty());
        assert!(changes.disable.is_empty());
        Ok(())
    }
}

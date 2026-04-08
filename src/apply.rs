//! Materialize apps on the target host.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};

use git2::{Oid, Tree};

use crate::error::Result;
use crate::plan::{AppDiff, Changes, SymlinkChanges, diff_enabled, diff_symlinks};
use crate::prim::Hostname;
use crate::store::{self, Store};

/// Map from unit filename to the absolute symlink target path.
pub type DesiredUnits = BTreeMap<String, PathBuf>;

const OID_PREFIX_LEN: usize = 10;

/// Truncate an oid to a short prefix for use in directory names.
fn oid_prefix(oid: Oid) -> String {
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
    store: &Store,
    commit_oid: Oid,
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
    store.checkout_app(commit_oid, host, app, &version_dir)?;

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
/// each changed app, and computes the unit lifecycle and symlink actions.
/// Does *not* reconcile symlinks or touch systemd -- the caller does that
/// in the post-apply phase. Calls `on_app` for each changed app.
pub fn apply_host(
    store: &Store,
    commit_oid: Oid,
    actual_current: Option<Oid>,
    host: &Hostname,
    apps_dir: &Path,
    operator: &str,
    mut on_app: impl FnMut(&str, &AppDiff),
) -> Result<Changes> {
    store.set_ref(
        "refs/heads/target",
        commit_oid,
        store::RefUpdate::SetTarget { operator },
    )?;

    let target_tree = store.get_commit_tree(commit_oid)?;
    let target_apps = store.get_host_apps(&target_tree, host)?;

    let (current_tree, current_apps) = match actual_current {
        None => (None, BTreeMap::new()),
        Some(oid) => {
            let tree = store.get_commit_tree(oid)?;
            let apps = store.get_host_apps(&tree, host)?;
            (Some(tree), apps)
        }
    };

    let diff = crate::plan::diff_apps(&current_apps, &target_apps);

    for (app, change) in &diff {
        match change {
            AppDiff::Add { .. } | AppDiff::Update { .. } => {
                apply_app(store, commit_oid, host, app, apps_dir)?;
            }
            AppDiff::Remove { .. } => {
                remove_app(app, apps_dir)?;
            }
        }
        on_app(app, change);
    }

    // Compute unit lifecycle actions. We collect enabled units only from
    // changed apps: unchanged apps need no action. This means a unit in
    // both prev and target was enabled across a change, so we need to restart
    // it so it picks up its new config.
    let changed_apps: BTreeSet<&str> = diff.keys().map(|app| app.as_str()).collect();
    let prev_enabled = match &current_tree {
        None => BTreeSet::new(),
        Some(tree) => collect_enabled_units(store, tree, host, &changed_apps)?,
    };
    let target_enabled = collect_enabled_units(store, &target_tree, host, &changed_apps)?;
    let units = diff_enabled(&prev_enabled, &target_enabled);

    let target_symlinks = store.desired_symlinks(commit_oid, host, apps_dir)?;
    let prev_symlinks = match actual_current {
        Some(oid) => store.desired_symlinks(oid, host, apps_dir)?,
        None => BTreeMap::new(),
    };
    let symlinks = diff_symlinks(&prev_symlinks, &target_symlinks);

    // We don't update refs/heads/current here. There is more to do (enabling
    // and disabling systemd units), and if that fails, we don't want the
    // `current` ref to say that the deploy was done while it was in fact
    // unfinished. It's better for it to be behind than ahead.

    Ok(Changes { units, symlinks })
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

/// Create a symlink, wrapping errors with the target path for diagnostics.
fn create_symlink(source: &Path, link: &Path) -> Result<()> {
    unix_fs::symlink(source, link).map_err(|err| {
        crate::error::Error::AgentError(format!(
            "cannot create symlink at {}: {err}; \
             if a file already exists there, remove it manually and retry",
            link.display(),
        ))
    })
}

/// Verify a symlink points into `apps_dir` before we remove or overwrite it.
fn verify_managed(link: &Path, apps_dir: &Path) -> Result<()> {
    match fs::read_link(link) {
        Ok(actual) if actual.starts_with(apps_dir) => Ok(()),
        Ok(actual) => Err(crate::error::Error::AgentError(format!(
            "refusing to touch {}: points to {}, not into {}",
            link.display(),
            actual.display(),
            apps_dir.display(),
        ))),
        // Dangling or missing -- nothing to protect.
        Err(_) => Ok(()),
    }
}

/// Collect enabled unit names from manifests, filtered to given apps.
fn collect_enabled_units(
    store: &Store,
    config_tree: &Tree,
    host: &Hostname,
    filter_apps: &BTreeSet<&str>,
) -> Result<BTreeSet<String>> {
    let host_apps = store.get_host_apps(config_tree, host)?;
    let mut enabled = BTreeSet::new();
    for (app, app_tree_oid) in &host_apps {
        if !filter_apps.contains(app.as_str()) {
            continue;
        }
        let manifest = store.read_manifest(*app_tree_oid)?;
        enabled.extend(manifest.systemd.units_enabled);
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
    use crate::testutil::{TempDir, TestRepo, assert_dir_contents};

    #[test]
    fn apply_app_creates_versioned_checkout_and_current_symlink() -> Result<()> {
        let t = TestRepo::new();
        let c1 = t.commit(&[("web1/nginx/nginx.conf", b"server {}")]);

        let apps = TempDir::new("apps");
        apply_app(&t.store, c1, &"web1".into(), "nginx", apps.path())?;

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

        apply_app(&t.store, c1, &"web1".into(), "nginx", apps.path())?;

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

        apply_app(&t.store, c1, &"web1".into(), "nginx", apps.path())?;
        let target = fs::read_link(&current)?;
        assert_eq!(target.to_str().expect("target is utf-8"), oid_prefix(c1));

        apply_app(&t.store, c2, &"web1".into(), "nginx", apps.path())?;
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
        apply_app(&t.store, c1, &"web1".into(), "nginx", apps.path())?;
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

    /// Test harness for `apply_host` calls.
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
            on_app: impl FnMut(&str, &AppDiff),
        ) -> Result<Changes> {
            let host = &"web1".into();
            let operator = "deckard@spinner";
            let changes = apply_host(
                &self.repo.store,
                commit,
                current,
                host,
                self.apps.path(),
                operator,
                on_app,
            )?;
            // Reconcile here so tests that check unit symlinks still work.
            let desired = self
                .repo
                .store
                .desired_units(commit, host, self.apps.path())?;
            reconcile_symlinks(&desired, self.apps.path(), self.units.path())?;
            Ok(changes)
        }
    }

    #[test]
    fn apply_host_sets_target_ref() -> Result<()> {
        let t = ApplyTest::new();
        let c1 = t.repo.commit(&[("web1/nginx/conf", b"v1")]);

        t.apply(c1, None, |_, _| {})?;

        let target = t
            .repo
            .store
            .repo
            .find_reference("refs/heads/target")?
            .peel_to_commit()?
            .id();
        assert_eq!(target, c1);

        // The `current` ref is *not* updated by apply_host, it's only updated
        // all the way at the end after other system mutations.
        assert!(
            t.repo
                .store
                .repo
                .find_reference("refs/heads/current")
                .is_err()
        );

        Ok(())
    }

    #[test]
    fn apply_host_writes_operator_to_reflog() -> Result<()> {
        let t = ApplyTest::new();
        let c1 = t.repo.commit(&[("web1/nginx/conf", b"v1")]);

        t.apply(c1, None, |_, _| {})?;

        let reflog = t.repo.store.repo.reflog("refs/heads/target")?;
        let entry = reflog.get(0).expect("reflog has an entry");
        let message = entry.message().expect("reflog message is valid utf-8");
        assert!(
            message.contains("deckard@spinner"),
            "reflog message should contain operator: {message}",
        );

        Ok(())
    }

    #[test]
    fn apply_host_reports_per_app_changes() -> Result<()> {
        let t = ApplyTest::new();
        let c1 = t.repo.commit(&[("web1/nginx/nginx.conf", b"v1")]);

        let mut applied = Vec::new();
        t.apply(c1, None, |app, diff| {
            applied.push((app.to_string(), diff.clone()));
        })?;

        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].0, "nginx");
        assert!(matches!(applied[0].1, AppDiff::Add { .. }));
        Ok(())
    }

    #[test]
    fn apply_host_only_enables_units_from_systemd_json() -> Result<()> {
        let t = ApplyTest::new();
        let c1 = t.repo.commit(&[
            ("web1/nginx/systemd/nginx.service", b"[Service]"),
            ("web1/nginx/systemd/nginx-reload.timer", b"[Timer]"),
            (
                "web1/nginx/manifest.json",
                br#"{"systemd": {"units_enabled": ["nginx.service"]}}"#,
            ),
        ]);

        let changes = t.apply(c1, None, |_, _| {})?;

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
    fn manifest_symlinks_creates_new_symlink() -> Result<()> {
        let dir = TempDir::new("symlinks");
        let apps = TempDir::new("apps");
        let target = dir.path().join("foo.conf");
        let source = apps.path().join("nginx/current/foo.conf");

        let changes = SymlinkChanges {
            create: vec![(target.clone(), source.clone())],
            remove: Vec::new(),
            change: Vec::new(),
        };
        reconcile_manifest_symlinks(apps.path(), &changes)?;

        assert_eq!(fs::read_link(&target)?, source);
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

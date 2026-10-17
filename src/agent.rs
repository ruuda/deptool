// Deptool -- A declarative configuration deployment tool.
// Copyright 2026 Ruud van Asseldonk

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// A copy of the License has been included in the root of the repository.

//! Agent-side session: handles requests from the driver and applies changes.

use std::collections::BTreeMap;
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use git2::Oid;

use crate::checkout::{self, CheckoutMode, checkout};
use crate::error::{ApplyError, StoreError};
use crate::log::FileLog;
use crate::plan::{AppDiff, DesiredState, SystemDiff, diff_host};
use crate::prim::Hostname;
use crate::protocol::{ApplyMode, Message, Request};
use crate::store::{RefUpdate, STORE_VERSION, STORE_VERSION_CLUSTER_TREE, Store};

/// Configuration for a host-side agent session.
///
/// Directories have a standard location for production use, but we allow
/// overriding them with `DEPTOOL_*` environment variables, primarily to aid
/// testing.
pub struct AgentConfig {
    pub hostname: String,
    pub apps_dir: PathBuf,
    pub networkd_dir: PathBuf,
    pub quadlets_dir: PathBuf,
    pub sysusers_dir: PathBuf,
    pub units_dir: PathBuf,
}

impl AgentConfig {
    pub fn from_env() -> Self {
        use crate::prim::{read_hostname, test_override};
        AgentConfig {
            hostname: test_override("DEPTOOL_HOSTNAME", &read_hostname()),
            apps_dir: PathBuf::from(test_override("DEPTOOL_APPS_DIR", "/var/lib/deptool/apps")),
            networkd_dir: PathBuf::from(test_override(
                "DEPTOOL_NETWORKD_DIR",
                "/etc/systemd/network",
            )),
            quadlets_dir: PathBuf::from(test_override(
                "DEPTOOL_QUADLETS_DIR",
                "/etc/containers/systemd",
            )),
            sysusers_dir: PathBuf::from(test_override("DEPTOOL_SYSUSERS_DIR", "/etc/sysusers.d")),
            units_dir: PathBuf::from(test_override("DEPTOOL_UNITS_DIR", "/etc/systemd/system")),
        }
    }
}

/// Try to acquire an exclusive, non-blocking file lock.
///
/// Returns `Ok(true)` if the lock was acquired, `Ok(false)` if it is
/// already held by another process.
pub fn try_flock_exclusive(file: &File) -> std::io::Result<bool> {
    // LOCK_EX = exclusive lock, LOCK_NB = fail immediately instead of blocking.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    if err.kind() == std::io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(err)
    }
}

/// Post-checkout mutations (symlinks, systemd lifecycle).
///
/// The session invokes this after materializing the checkout, both for a
/// forward apply and for a rollback.
pub trait Activate: Send + Sync {
    fn activate(
        &self,
        mode: ApplyMode,
        desired: &DesiredState,
        changes: &SystemDiff<PathBuf>,
        emit: &mut dyn FnMut(Message),
        log: &mut dyn FnMut(&str),
    ) -> std::result::Result<(), ApplyError>;
}

/// `Activate` implementation that leaves the host untouched, for tests.
struct NoopActivator;

impl Activate for NoopActivator {
    fn activate(
        &self,
        _: ApplyMode,
        _: &DesiredState,
        _: &SystemDiff<PathBuf>,
        _: &mut dyn FnMut(Message),
        _: &mut dyn FnMut(&str),
    ) -> std::result::Result<(), ApplyError> {
        Ok(())
    }
}

pub struct AgentSession {
    pub store: Store,
    pub hostname: Hostname,
    activator: Box<dyn Activate>,
    apps_dir: PathBuf,
    log: LogState,
    state: SessionState,
}

enum LogState {
    Disabled,
    Pending(PathBuf),
    Active(FileLog),
}

enum SessionState {
    Unlocked,
    Locked {
        /// The flock is released when the file is dropped.
        _file: File,
        /// Who initiated the deploy (e.g. "deckard@spinner").
        operator: String,
    },
}

/// State known during an apply, gathered from the lock and the request.
struct ApplyContext {
    operator: String,
    current_commit: Option<Oid>,
    target_commit: Oid,
    mode: ApplyMode,
}

impl AgentSession {
    pub fn new(
        store: Store,
        hostname: Hostname,
        activator: Box<dyn Activate>,
        apps_dir: PathBuf,
        log_path: Option<PathBuf>,
    ) -> Self {
        let log = match log_path {
            Some(path) => LogState::Pending(path),
            None => LogState::Disabled,
        };
        AgentSession {
            store,
            hostname,
            activator,
            apps_dir,
            log,
            state: SessionState::Unlocked,
        }
    }

    /// Create a session for testing that does not touch systemd or symlinks.
    #[doc(hidden)]
    pub fn new_test(repo: git2::Repository, hostname: &str, apps_dir: &std::path::Path) -> Self {
        AgentSession::new(
            Store { repo },
            hostname.into(),
            Box::new(NoopActivator),
            apps_dir.to_path_buf(),
            None,
        )
    }

    fn log(&self, msg: std::fmt::Arguments<'_>) {
        if let LogState::Active(log) = &self.log {
            log.log(msg);
        }
    }

    fn current_commit(&self) -> Option<Oid> {
        self.store.current_commit()
    }

    /// Convert the store from the cluster-tree format to the current one,
    /// see `STORE_VERSION_CLUSTER_TREE`.
    ///
    /// Returns the refusal to report when the deployed state cannot be
    /// converted, `None` once the store is converted. A store without
    /// commits only gets the current version recorded.
    fn convert_store(&self, operator: &str) -> Result<Option<Message>, StoreError> {
        // The conversion picks this host's config out of the cluster tree
        // by name, so under a changed hostname it would pick another
        // host's config or nothing. The hostname recorded at the last
        // apply is the exact check; a state deployed before recording
        // existed is caught by the missing entry below, except for a swap
        // of two names.
        match self.store.get_deployed_hostname()? {
            Some(deployed_as) if deployed_as != self.hostname => {
                return Ok(Some(Message::LockHostnameChanged {
                    deployed_as: Some(deployed_as),
                }));
            }
            _ => {}
        }

        // One signature for both refs: when they point at the same cluster
        // commit, they convert to the same commit and stay equal, as a
        // completed deploy leaves them.
        let sig = self.store.signature()?;
        let refs = [
            ("refs/heads/current", self.store.current_commit()),
            ("refs/heads/target", self.store.target_commit()),
        ];
        for (refname, commit) in refs {
            let commit = match commit {
                Some(oid) => oid,
                None => continue,
            };
            let converted = match self
                .store
                .convert_cluster_commit(&sig, &self.hostname, commit)?
            {
                Some(oid) => oid,
                None => return Ok(Some(Message::LockHostnameChanged { deployed_as: None })),
            };
            self.store
                .set_ref(refname, converted, RefUpdate::Convert { operator })?;
        }
        self.store.set_store_version(STORE_VERSION)?;
        Ok(None)
    }

    fn handle_lock(
        &mut self,
        expected_current_commit: Option<Oid>,
        operator: String,
        emit_message: &mut impl FnMut(Message),
    ) {
        let lock_path = self.store.get_lock_file_path();
        let mut file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
        {
            Ok(f) => f,
            Err(err) => {
                emit_message(Message::ErrorPreApply(ApplyError::Io(format!(
                    "failed to open lock file '{}': {err}",
                    lock_path.display(),
                ))));
                return;
            }
        };

        use std::io::{Read, Seek, Write};

        match try_flock_exclusive(&file) {
            Ok(false) => {
                let mut held_by = String::new();
                file.read_to_string(&mut held_by).ok();
                let held_by = if held_by.is_empty() {
                    None
                } else {
                    Some(held_by)
                };
                emit_message(Message::LockBusy { held_by });
                return;
            }
            Err(err) => {
                emit_message(Message::ErrorPreApply(ApplyError::Io(format!(
                    "failed to lock '{}': {err}",
                    lock_path.display(),
                ))));
                return;
            }
            Ok(true) => {}
        }

        // Write operator identity so concurrent deployers can see who
        // holds the lock. Not atomic, so a racing reader could see a
        // partial write, but this is best-effort operator feedback.
        file.set_len(0).expect("truncating lockfile succeeds");
        file.rewind().expect("seeking lockfile succeeds");
        let _ = write!(&file, "{operator}");

        // Check the store version before interpreting any commit: an older
        // format is converted first, a newer one may lay out its commits
        // differently. Returning releases the lock by dropping `file`.
        match self
            .store
            .get_store_version()
            .expect("host store config is readable")
        {
            None | Some(STORE_VERSION_CLUSTER_TREE) => match self.convert_store(&operator) {
                Ok(None) => {}
                Ok(Some(refusal)) => {
                    emit_message(refusal);
                    return;
                }
                Err(err) => {
                    emit_message(Message::ErrorPreApply(ApplyError::Store(format!(
                        "failed to convert store to the per-host format: {err}"
                    ))));
                    return;
                }
            },
            Some(STORE_VERSION) => {}
            Some(other_version) => {
                emit_message(Message::LockStoreVersionUnsupported {
                    version: other_version,
                });
                return;
            }
        }

        let actual_current_commit = self.current_commit();
        if actual_current_commit != expected_current_commit {
            // Stale: release the lock, the client won't be deploying.
            emit_message(Message::LockStale {
                expected_commit: expected_current_commit,
                actual_commit: actual_current_commit,
            });
        } else {
            // Open the log file now that we hold the lock exclusively.
            if let LogState::Pending(path) = &self.log
                && let Ok(file_log) = FileLog::open(path)
            {
                self.log = LogState::Active(file_log);
            }
            self.log(format_args!("lock acquired by {operator}"));
            self.state = SessionState::Locked {
                _file: file,
                operator,
            };
            emit_message(Message::Locked);
        }
    }

    fn handle_receive_pack(&self, pack_data: &str, emit_message: &mut impl FnMut(Message)) {
        let bytes = BASE64.decode(pack_data).expect("pack_data is valid base64");
        match self.store.write_pack(&bytes) {
            Ok(()) => emit_message(Message::PackReceived),
            Err(err) => {
                let msg = format!("failed to write pack: {err}");
                self.log(format_args!("{msg}"));
                emit_message(Message::ErrorPreApply(ApplyError::Store(msg)));
            }
        }
    }

    pub fn handle_request(&mut self, request: Request, emit_message: &mut impl FnMut(Message)) {
        match request {
            Request::Ping => emit_message(Message::Pong),
            Request::Lock {
                expected_current_commit,
                operator,
            } => self.handle_lock(expected_current_commit, operator, emit_message),
            Request::ReceivePack { ref pack_data } => {
                self.handle_receive_pack(pack_data, emit_message)
            }
            Request::RequestObjects { have_commit } => {
                // The driver only sends RequestObjects after LockStale
                // reported an actual_commit, so we must have one.
                let commit = self
                    .current_commit()
                    .expect("RequestObjects implies a current commit exists");
                match self.store.create_pack(commit, have_commit) {
                    Ok(bytes) => emit_message(Message::SendPack {
                        pack_data: BASE64.encode(&bytes),
                    }),
                    Err(err) => {
                        let msg = format!("failed to create pack: {err}");
                        self.log(format_args!("{msg}"));
                        emit_message(Message::ErrorPreApply(ApplyError::Store(msg)));
                    }
                }
            }
            Request::Apply {
                target_commit,
                mode,
            } => match &self.state {
                SessionState::Locked { operator, .. } => {
                    let ctx = ApplyContext {
                        operator: operator.clone(),
                        current_commit: self.current_commit(),
                        target_commit,
                        mode,
                    };
                    let (app_diffs, system_diff) = match self.prepare_apply(&ctx) {
                        Ok(prepared) => prepared,
                        Err(err) => {
                            self.log(format_args!("pre-apply failed: {err}"));
                            emit_message(Message::ErrorPreApply(err));
                            return;
                        }
                    };
                    match self.apply_modification(&ctx, &app_diffs, system_diff, emit_message) {
                        Ok(()) => {}
                        Err(err) => {
                            self.log(format_args!("apply failed: {err}"));
                            emit_message(Message::ApplyFailed(err));
                        }
                    }
                }
                // The driver always sends Lock before Apply; reaching this
                // arm means a protocol bug, not an expected runtime case.
                SessionState::Unlocked => panic!("Apply request without prior Lock"),
            },
        }
    }

    /// Pre-modification phase: diffs, descendant check, set target ref.
    fn prepare_apply(
        &self,
        ctx: &ApplyContext,
    ) -> Result<(BTreeMap<String, AppDiff>, SystemDiff<PathBuf>), ApplyError> {
        let diff_base = match ctx.mode {
            ApplyMode::Deploy { .. } => {
                self.log(format_args!(
                    "deploying {} -> {}",
                    format_opt_oid(ctx.current_commit),
                    checkout::oid_prefix(ctx.target_commit),
                ));
                ctx.current_commit
            }
            // Diff from the empty tree: reapply exists to repair drift, so
            // it must re-materialize every app even though the target is
            // already the current commit.
            ApplyMode::Reapply => {
                self.log(format_args!(
                    "reapplying {}",
                    checkout::oid_prefix(ctx.target_commit),
                ));
                None
            }
        };

        // Compute all diffs before any mutations, same as the driver's
        // make_plan but aggregated per-host.
        let (app_diffs, system_diff) = diff_host(
            &self.store,
            &self.apps_dir,
            diff_base,
            Some(ctx.target_commit),
        )?;

        match ctx.mode {
            ApplyMode::Deploy { is_rollback_safe } => {
                if let Some(current) = ctx.current_commit {
                    let is_descendant = self
                        .store
                        .repo
                        .graph_descendant_of(ctx.target_commit, current)
                        .map_err(|e| ApplyError::Store(e.to_string()))?;
                    assert!(is_descendant, "target commit descends from current commit");
                }

                assert_eq!(
                    system_diff.is_rollback_safe(),
                    is_rollback_safe,
                    "agent and driver disagree on rollback safety",
                );

                self.store.set_ref(
                    "refs/heads/target",
                    ctx.target_commit,
                    RefUpdate::SetTarget {
                        operator: &ctx.operator,
                    },
                )?;
            }
            ApplyMode::Reapply => {
                // The lock verified the host is at the commit the driver
                // expected, and the driver refuses hosts with a diverged
                // target ref, so both refs already point at the reapply
                // target: there is nothing to move.
                assert_eq!(
                    ctx.current_commit,
                    Some(ctx.target_commit),
                    "reapply target is the host's current commit",
                );
                assert_eq!(
                    self.store.target_commit(),
                    Some(ctx.target_commit),
                    "reapply target is the host's target commit",
                );
            }
        }

        Ok((app_diffs, system_diff))
    }

    /// Collect the desired managed-symlink state for a commit's config.
    fn desired_state(&self, commit_oid: Oid) -> Result<DesiredState, ApplyError> {
        Ok(DesiredState {
            networkd: self.store.desired_networkd(commit_oid, &self.apps_dir)?,
            quadlets: self.store.desired_quadlets(commit_oid, &self.apps_dir)?,
            sysusers: self.store.desired_sysusers(commit_oid, &self.apps_dir)?,
            units: self.store.desired_units(commit_oid, &self.apps_dir)?,
        })
    }

    /// Modification phase. Rollback-safe activation failures trigger a
    /// rollback inline; other errors flow out via `Err`.
    fn apply_modification(
        &self,
        ctx: &ApplyContext,
        app_diffs: &BTreeMap<String, AppDiff>,
        system_diff: SystemDiff<PathBuf>,
        emit_message: &mut impl FnMut(Message),
    ) -> Result<(), ApplyError> {
        checkout(
            &self.store,
            Some(ctx.target_commit),
            app_diffs,
            &self.apps_dir,
            CheckoutMode::Fresh,
            |msg| self.log(msg),
        )?;

        let desired = self.desired_state(ctx.target_commit)?;

        // Only a deploy rolls back, and only when the diff is safe to
        // revert. Reapply is itself the recovery path, and the failure is
        // already visible in the unit status output.
        let rollback_on_failure = match ctx.mode {
            ApplyMode::Deploy { .. } => system_diff.is_rollback_safe(),
            ApplyMode::Reapply => false,
        };

        let mut log_fn = |msg: &str| self.log(format_args!("{msg}"));
        match self
            .activator
            .activate(ctx.mode, &desired, &system_diff, emit_message, &mut log_fn)
        {
            Ok(()) => {}
            Err(err) if rollback_on_failure => {
                self.log(format_args!("activation failed, rolling back: {err}"));
                return self.handle_rollback(err, ctx, emit_message);
            }
            Err(err) => return Err(err),
        }

        // In reapply mode `current` already equals the target (asserted in
        // prepare_apply), and libgit2 skips same-value ref updates
        // entirely, so a reapply never writes this deploy-worded reflog
        // entry.
        self.store.set_ref(
            "refs/heads/current",
            ctx.target_commit,
            RefUpdate::SetCurrent {
                operator: &ctx.operator,
            },
        )?;

        checkout::gc_old_checkouts(&self.apps_dir, |msg| self.log(msg));

        let action = match ctx.mode {
            ApplyMode::Deploy { .. } => "deploy",
            ApplyMode::Reapply => "reapply",
        };
        self.log(format_args!(
            "{action} complete ({})",
            checkout::oid_prefix(ctx.target_commit),
        ));
        emit_message(Message::ApplyComplete {
            commit: ctx.target_commit,
            enabled_units: system_diff.unit_actions.enable,
            restarted_units: system_diff.unit_actions.restart,
            disabled_units: system_diff.unit_actions.disable,
        });
        Ok(())
    }

    /// Roll back to the good commit after a failed activation.
    ///
    /// Re-applies the commit that was `current` before the failed deploy
    /// attempt.
    /// Roll back to the good commit after a failed activation.
    ///
    /// Re-applies the commit that was `current` before the failed deploy
    /// attempt. The original error is included in the `RollingBack` message
    /// so the operator sees it regardless of whether rollback succeeds.
    fn handle_rollback(
        &self,
        error: ApplyError,
        ctx: &ApplyContext,
        emit_message: &mut impl FnMut(Message),
    ) -> std::result::Result<(), ApplyError> {
        self.log(format_args!("rolling back"));
        emit_message(Message::RollingBack);

        if let Err(rollback_error) = self.try_rollback(ctx, emit_message) {
            self.log(format_args!("rollback failed: {rollback_error}"));
            emit_message(Message::RollbackFailed {
                apply_error: error,
                rollback_error,
            });
            return Ok(());
        }

        self.log(format_args!("rollback complete"));
        emit_message(Message::RolledBack { error });
        Ok(())
    }

    fn try_rollback(
        &self,
        ctx: &ApplyContext,
        emit_message: &mut impl FnMut(Message),
    ) -> std::result::Result<(), ApplyError> {
        let (diffs, system_diff) = diff_host(
            &self.store,
            &self.apps_dir,
            Some(ctx.target_commit),
            ctx.current_commit,
        )?;

        checkout(
            &self.store,
            ctx.current_commit,
            &diffs,
            &self.apps_dir,
            CheckoutMode::Reuse,
            |msg| self.log(msg),
        )?;

        let desired = match ctx.current_commit {
            Some(oid) => self.desired_state(oid)?,
            None => DesiredState::default(),
        };
        let mut log_fn = |msg: &str| self.log(format_args!("{msg}"));
        self.activator
            .activate(ctx.mode, &desired, &system_diff, emit_message, &mut log_fn)?;

        match ctx.current_commit {
            Some(good) => self.store.set_ref(
                "refs/heads/target",
                good,
                RefUpdate::Rollback {
                    operator: &ctx.operator,
                },
            )?,
            // First deploy: no commit to point target at. The ref was set
            // to the failed target_commit earlier; leaving it is fine because
            // current was never advanced, so the divergence is visible.
            None => {}
        }
        Ok(())
    }
}

fn format_opt_oid(oid: Option<Oid>) -> String {
    match oid {
        Some(oid) => checkout::oid_prefix(oid),
        None => "(none)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::testutil::{TempDir, TestHost, TestRepo, commit_files};

    fn lock_expecting_current(expected_current_commit: Option<Oid>) -> Request {
        Request::Lock {
            expected_current_commit,
            operator: "deckard@spinner".into(),
        }
    }

    #[test]
    fn ping_returns_pong() {
        let mut host = TestHost::new("web1");
        assert_eq!(host.interact(Request::Ping), vec![Message::Pong]);
    }

    #[test]
    fn lock_succeeds_on_fresh_host_and_records_the_store_version() {
        let mut host = TestHost::new("web1");
        let responses = host.interact(lock_expecting_current(None));
        assert_eq!(responses, vec![Message::Locked]);
        assert_eq!(
            host.session
                .store
                .get_store_version()
                .expect("host store config is readable"),
            Some(STORE_VERSION),
        );
    }

    #[test]
    fn lock_succeeds_when_current_ref_matches_expected() {
        let (mut host, oid) = TestHost::with_commit("web1", &[("nginx/conf", b"v1")]);
        host.set_current(oid);
        let responses = host.interact(lock_expecting_current(Some(oid)));
        assert_eq!(responses, vec![Message::Locked]);
    }

    #[test]
    fn lock_reports_stale_when_current_ref_mismatches() {
        let (mut host, oid) = TestHost::with_commit("web1", &[("nginx/conf", b"v1")]);
        host.set_current(oid);
        let responses = host.interact(lock_expecting_current(None));
        assert_eq!(responses.len(), 1);
        match &responses[0] {
            Message::LockStale {
                expected_commit,
                actual_commit,
            } => {
                assert_eq!(*expected_commit, None);
                assert_eq!(*actual_commit, Some(oid));
            }
            other => panic!("Expected LockStale, got {other:?}"),
        }
    }

    #[test]
    fn lock_reports_busy_with_holder_identity() {
        let mut host = TestHost::new("web1");

        // Simulate another deployer holding the lock.
        let lock_path = host.session.store.get_lock_file_path();
        let lock_holder = File::create(&lock_path).expect("lock file is created");
        assert!(
            try_flock_exclusive(&lock_holder).expect("flock succeeds"),
            "lock is acquired",
        );
        use std::io::Write;
        write!(&lock_holder, "roy@nexus").expect("write succeeds");

        let responses = host.interact(lock_expecting_current(None));
        assert_eq!(
            responses,
            vec![Message::LockBusy {
                held_by: Some("roy@nexus".into()),
            }],
        );

        drop(lock_holder);
    }

    /// A host as Deptool 1.2 left it, with `files` as the cluster tree of
    /// its current commit, deployed under `hostname`.
    fn cluster_tree_host(hostname: &str, files: &[(&str, &[u8])]) -> (TestHost, Oid) {
        let driver = TestRepo::new();
        let cluster = driver.commit(files);
        let host = TestHost::with_cluster_tree_store(&driver, hostname, cluster);
        (host, cluster)
    }

    fn store_version(host: &TestHost) -> Option<u32> {
        host.session
            .store
            .get_store_version()
            .expect("host store config is readable")
    }

    fn parent_of(host: &TestHost, commit: Oid) -> Oid {
        host.session
            .store
            .repo
            .find_commit(commit)
            .expect("commit exists")
            .parent_id(0)
            .expect("commit has a parent")
    }

    #[test]
    fn lock_converts_a_diverged_target_on_its_own() {
        // An incomplete deploy left target ahead of current.
        let (mut host, cluster) = cluster_tree_host("web1", &[("web1/nginx/conf", b"v1")]);
        let target = commit_files(&host.session.store, &[("web1/nginx/conf", b"v2")])
            .expect("commit succeeds");
        host.set_target(target);

        host.interact(lock_expecting_current(Some(cluster)));

        let current = host.get_ref("refs/heads/current").expect("current exists");
        let converted_target = host.get_ref("refs/heads/target").expect("target exists");
        assert_ne!(
            current, converted_target,
            "the incomplete deploy stays visible"
        );
        assert_eq!(parent_of(&host, current), cluster);
        assert_eq!(parent_of(&host, converted_target), target);
    }

    #[test]
    fn lock_brings_an_interrupted_conversion_to_one_commit() {
        // A lock that moved `current` but was interrupted before it
        // recorded the version leaves the store half converted.
        let (mut host, cluster) = cluster_tree_host("web1", &[("web1/nginx/conf", b"v1")]);
        let store = &host.session.store;
        let sig = store.signature().expect("signature is available");
        let half = store
            .convert_cluster_commit(&sig, &"web1".into(), cluster)
            .expect("host store is readable")
            .expect("the cluster tree has an entry for web1");
        store
            .set_ref(
                "refs/heads/current",
                half,
                RefUpdate::Convert {
                    operator: "roy@nexus",
                },
            )
            .expect("ref is set");

        host.interact(lock_expecting_current(Some(cluster)));

        let current = host.get_ref("refs/heads/current").expect("current exists");
        assert_eq!(host.get_ref("refs/heads/target"), Some(current));
        assert_eq!(parent_of(&host, current), cluster);
        assert_eq!(store_version(&host), Some(STORE_VERSION));
    }

    #[test]
    fn lock_infers_a_rename_from_a_missing_entry_when_no_hostname_was_recorded() {
        // Deployed as web1 before Deptool recorded hostnames; the host now
        // reports web2. Without a web2 entry the conversion has nothing to
        // pick, so it refuses.
        let (mut host, cluster) = TestHost::with_commit("web2", &[("web1/nginx/conf", b"v1")]);
        host.set_current(cluster);
        host.session
            .store
            .set_store_version(STORE_VERSION_CLUSTER_TREE)
            .expect("version is recorded");

        assert_eq!(
            host.interact(lock_expecting_current(None)),
            vec![Message::LockHostnameChanged { deployed_as: None }],
        );
        assert_eq!(
            store_version(&host),
            Some(STORE_VERSION_CLUSTER_TREE),
            "nothing was converted"
        );
    }

    #[test]
    fn receive_pack_reports_an_error_for_invalid_pack_data() {
        let mut host = TestHost::new("web1");
        host.interact(lock_expecting_current(None));
        let responses = host.interact(Request::ReceivePack {
            pack_data: BASE64.encode(b"not a valid pack"),
        });
        assert!(
            matches!(
                responses.as_slice(),
                [Message::ErrorPreApply(ApplyError::Store(_))]
            ),
            "{responses:?}",
        );
    }

    /// A representative activation failure for tests that only need a sentinel.
    fn activation_failed() -> ApplyError {
        ApplyError::SystemdActivationFailed {
            units: vec!["nginx.service".into()],
        }
    }

    /// `Activate` stub that returns results from a list, one per call.
    struct SequenceActivator {
        results: Vec<std::result::Result<(), ApplyError>>,
        n_calls: AtomicUsize,
    }

    impl SequenceActivator {
        fn new(results: Vec<std::result::Result<(), ApplyError>>) -> Self {
            SequenceActivator {
                results,
                n_calls: AtomicUsize::new(0),
            }
        }
    }

    impl Activate for SequenceActivator {
        fn activate(
            &self,
            _: ApplyMode,
            _: &DesiredState,
            _: &SystemDiff<PathBuf>,
            _: &mut dyn FnMut(Message),
            _: &mut dyn FnMut(&str),
        ) -> std::result::Result<(), ApplyError> {
            let i = self.n_calls.fetch_add(1, Ordering::SeqCst);
            self.results
                .get(i)
                .expect("unexpected extra activate call")
                .clone()
        }
    }

    /// Create a TestHost with `files` committed to its store.
    fn test_host(
        hostname: &str,
        files: &[(&str, &[u8])],
        activator: SequenceActivator,
    ) -> (TestHost, Oid) {
        let store_dir = TempDir::new("store");
        let apps = TempDir::new("apps");
        let store = Store::open_or_init(store_dir.path()).expect("store is created");
        let oid = commit_files(&store, files).expect("commit succeeds");
        let session = AgentSession::new(
            store,
            hostname.into(),
            Box::new(activator),
            apps.path().to_path_buf(),
            None,
        );
        let host = TestHost::from_parts(session, store_dir, apps);
        (host, oid)
    }

    /// Lock the host and push a pack containing `commit`.
    fn lock_and_push(host: &mut TestHost, commit: Oid) {
        host.interact(lock_expecting_current(None));
        let store = Store::open(host.session.store.path()).expect("store opens");
        let pack = store
            .create_pack(commit, None)
            .expect("pack creation succeeds");
        let encoded = base64::prelude::BASE64_STANDARD.encode(&pack);
        host.interact(Request::ReceivePack { pack_data: encoded });
    }

    #[test]
    fn successful_apply_sets_refs_and_reflog() {
        let (mut host, c1) = TestHost::with_commit("web1", &[("nginx/conf", b"v1")]);
        lock_and_push(&mut host, c1);

        let msgs = host.interact(Request::Apply {
            target_commit: c1,
            mode: ApplyMode::Deploy {
                is_rollback_safe: true,
            },
        });

        assert!(matches!(msgs.as_slice(), [Message::ApplyComplete { .. }]));

        // Both target and current refs point to the deployed commit.
        assert_eq!(host.get_ref("refs/heads/target"), Some(c1));
        assert_eq!(host.get_ref("refs/heads/current"), Some(c1));

        // Reflog records the operator.
        match host.reflog("refs/heads/target").as_slice() {
            [entry] => assert_eq!(*entry, (c1, "deploy by deckard@spinner: set target".into())),
            other => panic!("unexpected target reflog: {other:?}"),
        }
    }

    #[test]
    fn rollback_on_activate_failure() {
        // Two commits: c1 is the good state, c2 is the broken deploy.
        let activator = SequenceActivator::new(vec![Err(activation_failed()), Ok(())]);
        let (mut host, c1) = test_host("web1", &[("nginx/nginx.conf", b"v1")], activator);
        host.set_current(c1);
        let store = Store::open(host.session.store.path()).expect("store opens");
        let c2 =
            commit_files(&store, &[("nginx/nginx.conf", b"v2-broken")]).expect("commit succeeds");

        // Lock and push c2.
        host.interact(lock_expecting_current(Some(c1)));
        let pack = store.create_pack(c2, Some(c1)).expect("pack succeeds");
        let encoded = base64::prelude::BASE64_STANDARD.encode(&pack);
        host.interact(Request::ReceivePack { pack_data: encoded });

        let msgs = host.interact(Request::Apply {
            target_commit: c2,
            mode: ApplyMode::Deploy {
                is_rollback_safe: true,
            },
        });

        assert!(matches!(
            msgs.as_slice(),
            [Message::RollingBack, Message::RolledBack { .. }],
        ));

        assert_eq!(host.get_ref("refs/heads/current"), Some(c1));
        let expected_reflog = [
            // Target was set to c2 for the deploy, then reset to c1 by rollback.
            (
                c1,
                "deploy by deckard@spinner: rollback after failed apply".to_string(),
            ),
            (c2, "deploy by deckard@spinner: set target".to_string()),
        ];
        assert_eq!(host.reflog("refs/heads/target"), expected_reflog);
    }

    #[test]
    fn no_rollback_when_not_rollback_safe() {
        // Data with a newly enabled unit -- not rollback-safe.
        let activator = SequenceActivator::new(vec![Err(activation_failed())]);
        let (mut host, c1) = test_host(
            "web1",
            &[
                ("nginx/nginx.conf", b"v1"),
                (
                    "nginx/manifest.json",
                    br#"{"systemd":{"units_enabled":["nginx.service"]}}"#,
                ),
            ],
            activator,
        );
        lock_and_push(&mut host, c1);

        let msgs = host.interact(Request::Apply {
            target_commit: c1,
            mode: ApplyMode::Deploy {
                is_rollback_safe: false,
            },
        });

        assert!(matches!(msgs.as_slice(), [Message::ApplyFailed(_)]));
    }

    #[test]
    fn rollback_to_empty_on_first_deploy_failure() {
        // First deploy: no current_commit.
        let activator = SequenceActivator::new(vec![Err(activation_failed()), Ok(())]);
        let (mut host, c1) = test_host("web1", &[("nginx/nginx.conf", b"v1")], activator);
        lock_and_push(&mut host, c1);

        let msgs = host.interact(Request::Apply {
            target_commit: c1,
            mode: ApplyMode::Deploy {
                is_rollback_safe: true,
            },
        });

        assert!(matches!(
            msgs.as_slice(),
            [Message::RollingBack, Message::RolledBack { .. }],
        ));

        // After rollback, apps dir should be clean (no app dirs left).
        let apps: Vec<_> = std::fs::read_dir(host.apps_path())
            .expect("apps dir exists")
            .collect();
        assert!(
            apps.is_empty(),
            "apps dir should be empty after rollback to empty: {apps:?}"
        );
    }

    #[test]
    fn failed_rollback_reports_rollback_error_and_logs_original() {
        let activator = SequenceActivator::new(vec![
            Err(activation_failed()),
            Err(ApplyError::Store("rollback IO error".into())),
        ]);
        let (mut host, c1) = test_host("web1", &[("nginx/nginx.conf", b"v1")], activator);
        host.set_current(c1);
        let store = Store::open(host.session.store.path()).expect("store opens");
        let c2 =
            commit_files(&store, &[("nginx/nginx.conf", b"v2-broken")]).expect("commit succeeds");

        host.interact(lock_expecting_current(Some(c1)));
        let pack = store.create_pack(c2, Some(c1)).expect("pack succeeds");
        let encoded = base64::prelude::BASE64_STANDARD.encode(&pack);
        host.interact(Request::ReceivePack { pack_data: encoded });

        let msgs = host.interact(Request::Apply {
            target_commit: c2,
            mode: ApplyMode::Deploy {
                is_rollback_safe: true,
            },
        });

        assert!(matches!(
            msgs.as_slice(),
            [
                Message::RollingBack,
                Message::RollbackFailed {
                    apply_error: ApplyError::SystemdActivationFailed { .. },
                    rollback_error: ApplyError::Store(_),
                },
            ],
        ));

        // Target still points to the failed commit; current was never advanced.
        assert_eq!(host.get_ref("refs/heads/target"), Some(c2));
        assert_eq!(host.get_ref("refs/heads/current"), Some(c1));
    }

    #[test]
    fn reapply_rematerializes_apps_and_enables_all_units() {
        let (mut host, c1) = TestHost::with_commit(
            "web1",
            &[
                ("nginx/nginx.conf", b"v1"),
                (
                    "nginx/manifest.json",
                    br#"{"systemd":{"units_enabled":["nginx.service"]}}"#,
                ),
            ],
        );
        lock_and_push(&mut host, c1);
        host.interact(Request::Apply {
            target_commit: c1,
            mode: ApplyMode::Deploy {
                is_rollback_safe: false,
            },
        });

        // Drift: a file vanished from the checkout.
        let conf = host.apps_path().join("nginx/current/nginx.conf");
        std::fs::remove_file(&conf).expect("checked-out file exists");

        let msgs = host.interact(Request::Apply {
            target_commit: c1,
            mode: ApplyMode::Reapply,
        });

        // The diff runs against the empty tree, so every manifest-enabled
        // unit lands in enabled_units, even though nothing changed.
        match msgs.as_slice() {
            [
                Message::ApplyComplete {
                    commit,
                    enabled_units,
                    restarted_units,
                    disabled_units,
                },
            ] => {
                assert_eq!(*commit, c1);
                assert_eq!(enabled_units, &["nginx.service".to_string()]);
                assert!(restarted_units.is_empty());
                assert!(disabled_units.is_empty());
            }
            other => panic!("expected ApplyComplete, got {other:?}"),
        }

        assert!(conf.exists(), "reapply re-materialized the drifted file");
        assert_eq!(host.get_ref("refs/heads/current"), Some(c1));

        // A reapply moves no refs: the target ref update only runs for
        // deploys, and `current` is re-set to the value it already has,
        // which libgit2 skips entirely, reflog included. So the reflog
        // still holds only the deploy entries.
        assert_eq!(host.reflog("refs/heads/target").len(), 1);
        assert_eq!(host.reflog("refs/heads/current").len(), 1);
    }

    #[test]
    fn reapply_does_not_roll_back_on_activation_failure() {
        // An app without system effects has a rollback-safe diff: a failing
        // deploy would roll back here, a failing reapply must not (a second
        // activate call would panic the sequence with "unexpected extra
        // activate call").
        let activator = SequenceActivator::new(vec![Err(activation_failed())]);
        let (mut host, c1) = test_host("web1", &[("app/conf", b"v1")], activator);
        host.set_current(c1);
        host.session
            .store
            .set_ref(
                "refs/heads/target",
                c1,
                RefUpdate::SetTarget {
                    operator: "deckard@spinner",
                },
            )
            .expect("ref is set");
        host.interact(lock_expecting_current(Some(c1)));

        let msgs = host.interact(Request::Apply {
            target_commit: c1,
            mode: ApplyMode::Reapply,
        });

        assert!(matches!(msgs.as_slice(), [Message::ApplyFailed(_)]));
        assert_eq!(host.get_ref("refs/heads/current"), Some(c1));
    }
}

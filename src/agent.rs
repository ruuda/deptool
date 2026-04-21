//! Agent-side session: handles requests from the driver and applies changes.

use std::collections::BTreeMap;
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use git2::Oid;

use crate::checkout::{self, CheckoutMode, checkout};
use crate::error::ApplyError;
use crate::log::FileLog;
use crate::plan::{AppDiff, DesiredUnits, SystemDiff, diff_host};
use crate::prim::Hostname;
use crate::protocol::{Message, Request};
use crate::store::{RefUpdate, Store};

/// Configuration for a host-side agent session.
///
/// Directories have a standard location for production use, but we allow
/// overriding them with `DEPTOOL_*` environment variables, primarily to aid
/// testing.
pub struct AgentConfig {
    pub hostname: String,
    pub apps_dir: PathBuf,
    pub unit_dir: PathBuf,
}

impl AgentConfig {
    pub fn from_env() -> Self {
        let hostname =
            std::env::var("DEPTOOL_HOSTNAME").unwrap_or_else(|_| crate::prim::read_hostname());
        let apps_dir = PathBuf::from(
            std::env::var("DEPTOOL_APPS_DIR").unwrap_or("/var/lib/deptool/apps".into()),
        );
        let unit_dir = PathBuf::from(
            std::env::var("DEPTOOL_UNIT_DIR").unwrap_or("/etc/systemd/system".into()),
        );
        AgentConfig {
            hostname,
            apps_dir,
            unit_dir,
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

/// Callback for post-checkout mutations (symlinks, systemd lifecycle).
type OnActivate = Box<
    dyn Fn(
            &DesiredUnits,
            &SystemDiff<PathBuf>,
            &mut dyn FnMut(Message),
        ) -> std::result::Result<(), ApplyError>
        + Send
        + Sync,
>;

pub struct AgentSession {
    pub store: Store,
    pub hostname: Hostname,
    apps_dir: PathBuf,
    on_activate: OnActivate,
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
    is_rollback_safe: bool,
}

impl AgentSession {
    pub fn new(
        store: Store,
        hostname: Hostname,
        apps_dir: PathBuf,
        on_activate: OnActivate,
        log_path: Option<PathBuf>,
    ) -> Self {
        let log = match log_path {
            Some(path) => LogState::Pending(path),
            None => LogState::Disabled,
        };
        AgentSession {
            store,
            hostname,
            apps_dir,
            on_activate,
            log,
            state: SessionState::Unlocked,
        }
    }

    /// Create a session for testing that does not touch systemd or symlinks.
    #[doc(hidden)]
    pub fn new_test(repo: git2::Repository, hostname: &str, apps_dir: &std::path::Path) -> Self {
        let on_activate: OnActivate = Box::new(|_, _, _| Ok(()));
        AgentSession::new(
            Store { repo },
            hostname.into(),
            apps_dir.to_path_buf(),
            on_activate,
            None,
        )
    }

    fn log(&self, msg: std::fmt::Arguments<'_>) {
        if let LogState::Active(log) = &self.log {
            log.log(msg);
        }
    }

    fn current_commit(&self) -> Option<Oid> {
        self.store
            .repo
            .find_reference("refs/heads/current")
            .ok()
            .map(|r| r.peel_to_commit().expect("current ref points to a commit"))
            .map(|c| c.id())
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
                emit_message(Message::Error(ApplyError::Io(format!(
                    "failed to open lock file: {err}",
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
                emit_message(Message::Error(ApplyError::Io(format!(
                    "flock failed: {err}",
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

        let actual_current_commit = self.current_commit();
        if actual_current_commit != expected_current_commit {
            // Stale: release the lock, the client won't be deploying.
            emit_message(Message::LockStale {
                expected_commit: expected_current_commit,
                actual_commit: actual_current_commit,
            });
        } else {
            // Open the log file now that we hold the lock exclusively.
            if let LogState::Pending(path) = &self.log {
                if let Ok(file_log) = FileLog::open(path) {
                    self.log = LogState::Active(file_log);
                }
            }
            self.log(format_args!("lock: acquired by {operator}"));
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
                emit_message(Message::Error(ApplyError::Store(msg)));
            }
        }
    }

    pub fn handle_request(&mut self, request: Request, emit_message: &mut impl FnMut(Message)) {
        match request {
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
                        emit_message(Message::Error(ApplyError::Store(msg)));
                    }
                }
            }
            Request::Apply {
                target_commit,
                is_rollback_safe,
            } => match &self.state {
                SessionState::Locked { operator, .. } => {
                    let ctx = ApplyContext {
                        operator: operator.clone(),
                        current_commit: self.current_commit(),
                        target_commit,
                        is_rollback_safe,
                    };
                    if let Err(err) = self.handle_apply(&ctx, emit_message) {
                        self.log(format_args!("apply: failed: {err}"));
                        emit_message(Message::Error(err));
                    }
                }
                SessionState::Unlocked => {
                    emit_message(Message::Error(ApplyError::Store(
                        "Apply request without Lock".into(),
                    )));
                }
            },
        }
    }

    fn handle_apply(
        &self,
        ctx: &ApplyContext,
        emit_message: &mut impl FnMut(Message),
    ) -> std::result::Result<(), ApplyError> {
        self.log(format_args!(
            "apply: current={}, target={}",
            format_opt_oid(ctx.current_commit),
            ctx.target_commit,
        ));

        // Compute all diffs before any mutations, same as the driver's
        // make_plan but aggregated per-host.
        let (app_diffs, system_diff) = diff_host(
            &self.store,
            &self.hostname,
            &self.apps_dir,
            ctx.current_commit,
            Some(ctx.target_commit),
        )?;

        self.log(format_args!("apply: {}", format_diffs(&app_diffs)));

        assert_eq!(
            system_diff.is_rollback_safe(),
            ctx.is_rollback_safe,
            "agent and driver disagree on rollback safety",
        );

        self.log(format_args!("apply: setting target ref"));
        self.store.set_ref(
            "refs/heads/target",
            ctx.target_commit,
            RefUpdate::SetTarget {
                operator: &ctx.operator,
            },
        )?;

        self.log(format_args!("apply: checkout"));
        checkout(
            &self.store,
            Some(ctx.target_commit),
            &app_diffs,
            &self.hostname,
            &self.apps_dir,
            CheckoutMode::Fresh,
        )?;

        self.log(format_args!("apply: activating"));
        let desired_units =
            self.store
                .desired_units(ctx.target_commit, &self.hostname, &self.apps_dir)?;

        match (self.on_activate)(&desired_units, &system_diff, emit_message) {
            Ok(()) => {}
            Err(err) if system_diff.is_rollback_safe() => {
                self.log(format_args!(
                    "apply: activation failed, rolling back: {err}"
                ));
                return self.handle_rollback(err, ctx, emit_message);
            }
            Err(err) => {
                self.log(format_args!("apply: activation failed: {err}"));
                return Err(err);
            }
        }

        self.log(format_args!("apply: setting current ref"));
        self.store.set_ref(
            "refs/heads/current",
            ctx.target_commit,
            RefUpdate::SetCurrent {
                operator: &ctx.operator,
            },
        )?;

        self.log(format_args!("apply: gc"));
        checkout::gc_old_checkouts(&self.apps_dir, |msg| self.log(msg));

        self.log(format_args!(
            "apply: complete, commit={}",
            ctx.target_commit
        ));
        emit_message(Message::ApplyComplete {
            commit: ctx.target_commit,
            enabled_units: system_diff.units.enable,
            restarted_units: system_diff.units.restart,
            disabled_units: system_diff.units.disable,
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
        self.log(format_args!("apply: rolling back"));
        emit_message(Message::RollingBack);

        if let Err(rollback_error) = self.try_rollback(ctx, emit_message) {
            self.log(format_args!("apply: rollback failed: {rollback_error}"));
            emit_message(Message::RollbackFailed {
                apply_error: error,
                rollback_error,
            });
            return Ok(());
        }

        self.log(format_args!("apply: rollback complete"));
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
            &self.hostname,
            &self.apps_dir,
            Some(ctx.target_commit),
            ctx.current_commit,
        )?;

        checkout(
            &self.store,
            ctx.current_commit,
            &diffs,
            &self.hostname,
            &self.apps_dir,
            CheckoutMode::Reuse,
        )?;

        let desired_units = match ctx.current_commit {
            Some(oid) => self
                .store
                .desired_units(oid, &self.hostname, &self.apps_dir)?,
            None => BTreeMap::new(),
        };
        (self.on_activate)(&desired_units, &system_diff, emit_message)?;

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
        Some(oid) => oid.to_string(),
        None => "(none)".to_string(),
    }
}

fn format_diffs(diffs: &BTreeMap<String, AppDiff>) -> String {
    if diffs.is_empty() {
        return "no changes".to_string();
    }
    let mut parts = Vec::with_capacity(diffs.len());
    for (app, diff) in diffs {
        let label = match diff {
            AppDiff::Add { .. } => "add",
            AppDiff::Remove { .. } => "remove",
            AppDiff::Update { .. } => "update",
        };
        parts.push(format!("{app}={label}"));
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{TempDir, TestHost, commit_files};

    #[test]
    fn lock_succeeds_on_fresh_host_with_no_expected_current() {
        let mut host = TestHost::new("web1");
        let responses = host.interact(Request::Lock {
            expected_current_commit: None,
            operator: "deckard@spinner".into(),
        });
        assert_eq!(responses, vec![Message::Locked]);
    }

    #[test]
    fn lock_succeeds_when_current_ref_matches_expected() {
        let (mut host, oid) = TestHost::with_commit("web1", &[("web1/nginx/conf", b"v1")]);
        host.set_current(oid);
        let responses = host.interact(Request::Lock {
            expected_current_commit: Some(oid),
            operator: "deckard@spinner".into(),
        });
        assert_eq!(responses, vec![Message::Locked]);
    }

    #[test]
    fn lock_reports_stale_when_current_ref_mismatches() {
        let (mut host, oid) = TestHost::with_commit("web1", &[("web1/nginx/conf", b"v1")]);
        host.set_current(oid);
        let responses = host.interact(Request::Lock {
            expected_current_commit: None,
            operator: "deckard@spinner".into(),
        });
        assert_eq!(responses.len(), 1);
        match &responses[0] {
            Message::LockStale {
                expected_commit,
                actual_commit,
            } => {
                assert_eq!(*expected_commit, None);
                assert_eq!(*actual_commit, Some(oid.into()));
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

        let responses = host.interact(Request::Lock {
            expected_current_commit: None,
            operator: "deckard@spinner".into(),
        });
        assert_eq!(
            responses,
            vec![Message::LockBusy {
                held_by: Some("roy@nexus".into()),
            }],
        );

        drop(lock_holder);
    }

    /// Build an OnActivate that returns results from a list, one per call.
    fn activate_sequence(results: Vec<std::result::Result<(), ApplyError>>) -> super::OnActivate {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        Box::new(move |_, _, _| {
            let i = calls.fetch_add(1, Ordering::SeqCst);
            results
                .get(i)
                .expect("unexpected extra activate call")
                .clone()
        })
    }

    /// Create a TestHost with a custom activate callback.
    fn test_host(
        hostname: &str,
        files: &[(&str, &[u8])],
        on_activate: super::OnActivate,
    ) -> (TestHost, Oid) {
        let store_dir = TempDir::new("store");
        let apps = TempDir::new("apps");
        let store = Store::open_or_init(store_dir.path()).expect("store is created");
        let oid = commit_files(&store, files).expect("commit succeeds");
        let session = AgentSession::new(
            store,
            hostname.into(),
            apps.path().to_path_buf(),
            on_activate,
            None,
        );
        let host = TestHost::from_parts(session, store_dir, apps);
        (host, oid)
    }

    /// Lock the host and push a pack containing `commit`.
    fn lock_and_push(host: &mut TestHost, commit: Oid) {
        host.interact(Request::Lock {
            expected_current_commit: None,
            operator: "deckard@spinner".into(),
        });
        let store = Store::open(host.session.store.path()).expect("store opens");
        let pack = store
            .create_pack(commit, None)
            .expect("pack creation succeeds");
        let encoded = base64::prelude::BASE64_STANDARD.encode(&pack);
        host.interact(Request::ReceivePack { pack_data: encoded });
    }

    #[test]
    fn successful_apply_sets_refs_and_reflog() {
        let (mut host, c1) = TestHost::with_commit("web1", &[("web1/nginx/conf", b"v1")]);
        lock_and_push(&mut host, c1);

        let msgs = host.interact(Request::Apply {
            target_commit: c1,
            is_rollback_safe: true,
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
        let on_activate = activate_sequence(vec![Err(ApplyError::SystemdActivationFailed), Ok(())]);

        // Two commits: c1 is the good state, c2 is the broken deploy.
        let (mut host, c1) = test_host("web1", &[("web1/nginx/nginx.conf", b"v1")], on_activate);
        host.set_current(c1);
        let store = Store::open(host.session.store.path()).expect("store opens");
        let c2 = commit_files(&store, &[("web1/nginx/nginx.conf", b"v2-broken")])
            .expect("commit succeeds");

        // Lock and push c2.
        host.interact(Request::Lock {
            expected_current_commit: Some(c1),
            operator: "deckard@spinner".into(),
        });
        let pack = store.create_pack(c2, Some(c1)).expect("pack succeeds");
        let encoded = base64::prelude::BASE64_STANDARD.encode(&pack);
        host.interact(Request::ReceivePack { pack_data: encoded });

        let msgs = host.interact(Request::Apply {
            target_commit: c2,
            is_rollback_safe: true,
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
        let on_activate: super::OnActivate =
            Box::new(|_, _, _| Err(ApplyError::SystemdActivationFailed));

        // Data with a newly enabled unit -- not rollback-safe.
        let (mut host, c1) = test_host(
            "web1",
            &[
                ("web1/nginx/nginx.conf", b"v1"),
                (
                    "web1/nginx/manifest.json",
                    br#"{"systemd":{"units_enabled":["nginx.service"]}}"#,
                ),
            ],
            on_activate,
        );
        lock_and_push(&mut host, c1);

        let msgs = host.interact(Request::Apply {
            target_commit: c1,
            is_rollback_safe: false,
        });

        assert!(matches!(msgs.as_slice(), [Message::Error(_)],));
    }

    #[test]
    fn rollback_to_empty_on_first_deploy_failure() {
        let on_activate = activate_sequence(vec![Err(ApplyError::SystemdActivationFailed), Ok(())]);

        // First deploy: no current_commit.
        let (mut host, c1) = test_host("web1", &[("web1/nginx/nginx.conf", b"v1")], on_activate);
        lock_and_push(&mut host, c1);

        let msgs = host.interact(Request::Apply {
            target_commit: c1,
            is_rollback_safe: true,
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
        let on_activate = activate_sequence(vec![
            Err(ApplyError::SystemdActivationFailed),
            Err(ApplyError::Store("rollback IO error".into())),
        ]);

        let (mut host, c1) = test_host("web1", &[("web1/nginx/nginx.conf", b"v1")], on_activate);
        host.set_current(c1);
        let store = Store::open(host.session.store.path()).expect("store opens");
        let c2 = commit_files(&store, &[("web1/nginx/nginx.conf", b"v2-broken")])
            .expect("commit succeeds");

        host.interact(Request::Lock {
            expected_current_commit: Some(c1),
            operator: "deckard@spinner".into(),
        });
        let pack = store.create_pack(c2, Some(c1)).expect("pack succeeds");
        let encoded = base64::prelude::BASE64_STANDARD.encode(&pack);
        host.interact(Request::ReceivePack { pack_data: encoded });

        let msgs = host.interact(Request::Apply {
            target_commit: c2,
            is_rollback_safe: true,
        });

        assert!(matches!(
            msgs.as_slice(),
            [
                Message::RollingBack,
                Message::RollbackFailed {
                    apply_error: ApplyError::SystemdActivationFailed,
                    rollback_error: ApplyError::Store(_),
                },
            ],
        ));

        // Target still points to the failed commit; current was never advanced.
        assert_eq!(host.get_ref("refs/heads/target"), Some(c2));
        assert_eq!(host.get_ref("refs/heads/current"), Some(c1));
    }
}

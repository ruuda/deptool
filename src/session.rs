//! Host-side session logic: handles requests and applies changes.

use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use git2::Oid;

use crate::apply::{DesiredUnits, apply_checkout, diff_host};
use crate::error::ApplyError;
use crate::plan::SystemDiff;
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
type OnPostApply = Box<
    dyn Fn(
            &DesiredUnits,
            &SystemDiff<PathBuf>,
            &mut dyn FnMut(Message),
        ) -> std::result::Result<(), ApplyError>
        + Send
        + Sync,
>;

pub struct HostSession {
    pub store: Store,
    pub hostname: Hostname,
    apps_dir: PathBuf,
    on_post_apply: OnPostApply,
    /// Acquired during lock, held for the session lifetime.
    lock: Option<DeployLock>,
}

struct DeployLock {
    /// The flock is released when the file is dropped, so we hold it
    /// for the session lifetime to prevent concurrent deploys.
    _file: File,
    /// Who initiated the deploy (e.g. "deckard@spinner").
    operator: String,
}

/// State known during an apply, gathered from the lock and the request.
struct ApplyContext {
    operator: String,
    current_commit: Option<Oid>,
    target_commit: Oid,
    is_rollback_safe: bool,
}

impl HostSession {
    pub fn new(
        store: Store,
        hostname: Hostname,
        apps_dir: PathBuf,
        on_post_apply: OnPostApply,
    ) -> Self {
        HostSession {
            store,
            hostname,
            apps_dir,
            on_post_apply,
            lock: None,
        }
    }

    /// Create a session for testing that does not touch systemd or symlinks.
    #[doc(hidden)]
    pub fn new_test(repo: git2::Repository, hostname: &str, apps_dir: &std::path::Path) -> Self {
        let on_post_apply: OnPostApply = Box::new(|_, _, _| Ok(()));
        HostSession::new(
            Store { repo },
            hostname.into(),
            apps_dir.to_path_buf(),
            on_post_apply,
        )
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
            self.lock = Some(DeployLock {
                _file: file,
                operator,
            });
            emit_message(Message::Locked);
        }
    }

    fn handle_receive_pack(&self, pack_data: &str, emit_message: &mut impl FnMut(Message)) {
        let bytes = BASE64.decode(pack_data).expect("pack_data is valid base64");
        match self.store.write_pack(&bytes) {
            Ok(()) => emit_message(Message::PackReceived),
            Err(err) => emit_message(Message::Error(ApplyError::Store(format!(
                "failed to write pack: {err}",
            )))),
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
                    Err(err) => emit_message(Message::Error(ApplyError::Store(format!(
                        "failed to create pack: {err}",
                    )))),
                }
            }
            Request::Apply {
                target_commit,
                is_rollback_safe,
            } => {
                let ctx = ApplyContext {
                    operator: self
                        .lock
                        .as_ref()
                        .expect("lock is held during apply")
                        .operator
                        .clone(),
                    current_commit: self.current_commit(),
                    target_commit,
                    is_rollback_safe,
                };
                if let Err(err) = self.handle_apply(&ctx, emit_message) {
                    emit_message(Message::Error(err));
                }
            }
        }
    }

    fn handle_apply(
        &self,
        ctx: &ApplyContext,
        emit_message: &mut impl FnMut(Message),
    ) -> std::result::Result<(), ApplyError> {
        // Compute all diffs before any mutations, same as the driver's
        // make_plan but aggregated per-host.
        let (app_diffs, system_diff) = diff_host(
            &self.store,
            &self.hostname,
            &self.apps_dir,
            ctx.current_commit,
            ctx.target_commit,
        )?;

        assert_eq!(
            system_diff.is_rollback_safe(),
            ctx.is_rollback_safe,
            "agent and driver disagree on rollback safety",
        );

        self.store.set_ref(
            "refs/heads/target",
            ctx.target_commit,
            RefUpdate::SetTarget {
                operator: &ctx.operator,
            },
        )?;

        apply_checkout(
            &self.store,
            ctx.target_commit,
            &app_diffs,
            &self.hostname,
            &self.apps_dir,
            |app, diff| {
                emit_message(Message::AppliedApp {
                    app: app.to_string(),
                    diff: diff.clone(),
                });
            },
        )?;

        let desired_units =
            self.store
                .desired_units(ctx.target_commit, &self.hostname, &self.apps_dir)?;

        match (self.on_post_apply)(&desired_units, &system_diff, emit_message) {
            Ok(()) => {}
            Err(err) if system_diff.is_rollback_safe() => {
                return self.handle_rollback(err, ctx, emit_message);
            }
            Err(err) => return Err(err),
        }

        self.store.set_ref(
            "refs/heads/current",
            ctx.target_commit,
            RefUpdate::SetCurrent {
                operator: &ctx.operator,
            },
        )?;

        emit_message(Message::ApplyComplete {
            commit: ctx.target_commit,
            enabled_units: system_diff.units.enable,
            restarted_units: system_diff.units.restart,
            disabled_units: system_diff.units.disable,
        });
        Ok(())
    }

    /// Roll back to the good commit after a failed post-apply.
    ///
    /// Re-applies the commit that was `current` before the failed deploy
    /// attempt.
    fn handle_rollback(
        &self,
        error: ApplyError,
        ctx: &ApplyContext,
        emit_message: &mut impl FnMut(Message),
    ) -> std::result::Result<(), ApplyError> {
        // TODO: support rollback to empty (first deploy) by making
        // apply_checkout accept an optional target commit.
        let good_commit = match ctx.current_commit {
            None => unimplemented!("rollback to empty host"),
            Some(commit) => commit,
        };

        emit_message(Message::RollingBack);

        let (diffs, system_diff) = diff_host(
            &self.store,
            &self.hostname,
            &self.apps_dir,
            Some(ctx.target_commit),
            good_commit,
        )?;

        // TODO: Instead of reusing `apply_checkout`, which deletes the checkout
        // dir and recreates it if it already exists (which it does in this
        // case), we can just update the app's `current` symlinks. That's fewer
        // operations so less likely to fail, and it preserves the older ctimes
        // on the existing checkout which can be valuable debugging signals for
        // operators.
        apply_checkout(
            &self.store,
            good_commit,
            &diffs,
            &self.hostname,
            &self.apps_dir,
            |app, diff| {
                emit_message(Message::AppliedApp {
                    app: app.to_string(),
                    diff: diff.clone(),
                });
            },
        )?;

        let desired_units =
            self.store
                .desired_units(good_commit, &self.hostname, &self.apps_dir)?;
        (self.on_post_apply)(&desired_units, &system_diff, emit_message)?;

        self.store.set_ref(
            "refs/heads/target",
            good_commit,
            RefUpdate::Rollback {
                operator: &ctx.operator,
            },
        )?;

        emit_message(Message::RolledBack { error });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TestHost;

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
            expected_current_commit: Some(oid.into()),
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

    /// Create a TestHost with a custom post_apply callback.
    fn test_host(
        hostname: &str,
        files: &[(&str, &[u8])],
        on_post_apply: super::OnPostApply,
    ) -> (crate::testutil::TestHost, Oid) {
        use crate::testutil::TempDir;
        let store_dir = TempDir::new("store");
        let apps = TempDir::new("apps");
        let repo = git2::Repository::init_bare(store_dir.path()).expect("repo is created");
        let store = Store { repo };
        let oid = crate::testutil::commit_files(&store, files).expect("commit succeeds");
        let session = HostSession::new(
            store,
            hostname.into(),
            apps.path().to_path_buf(),
            on_post_apply,
        );
        let host = crate::testutil::TestHost::from_parts(session, store_dir, apps);
        (host, oid)
    }

    /// Lock the host and push a pack containing `commit`.
    fn lock_and_push(host: &mut crate::testutil::TestHost, commit: Oid) {
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
    fn rollback_on_post_apply_failure() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Fail on first post_apply call, succeed on second (rollback).
        let call_count = Arc::new(AtomicUsize::new(0));
        let count = call_count.clone();
        let on_post_apply: super::OnPostApply = Box::new(move |_, _, _| {
            if count.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(ApplyError::SystemdActivationFailed)
            } else {
                Ok(())
            }
        });

        // Two commits: c1 is the good state, c2 is the broken deploy.
        let (mut host, c1) = test_host("web1", &[("web1/nginx/nginx.conf", b"v1")], on_post_apply);
        host.set_current(c1);
        let store = Store::open(host.session.store.path()).expect("store opens");
        let c2 = crate::testutil::commit_files(&store, &[("web1/nginx/nginx.conf", b"v2-broken")])
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

        // Forward apply emits AppliedApp, then post_apply fails, then rollback.
        assert!(matches!(
            msgs.as_slice(),
            [
                Message::AppliedApp { .. },
                Message::RollingBack,
                Message::AppliedApp { .. },
                Message::RolledBack { .. },
            ],
        ));
    }

    #[test]
    fn no_rollback_when_not_rollback_safe() {
        let on_post_apply: super::OnPostApply =
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
            on_post_apply,
        );
        lock_and_push(&mut host, c1);

        let msgs = host.interact(Request::Apply {
            target_commit: c1,
            is_rollback_safe: false,
        });

        assert!(matches!(
            msgs.as_slice(),
            [Message::AppliedApp { .. }, Message::Error(_)],
        ));
    }
}

//! Host-side session logic: handles requests and applies changes.

use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use git2::Repository;

use crate::prim::{Hostname, Oid};
use crate::protocol::{Message, Request};

/// Try to acquire an exclusive, non-blocking file lock.
///
/// Returns `Ok(true)` if the lock was acquired, `Ok(false)` if it is
/// already held by another process.
fn try_flock_exclusive(file: &File) -> std::io::Result<bool> {
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

pub struct HostSession {
    pub repo: Repository,
    hostname: Hostname,
    apps_dir: PathBuf,
    unit_dir: PathBuf,
    on_units_changed: Box<dyn Fn(&crate::plan::UnitChanges) -> crate::error::Result<()>>,
    /// Held for the session lifetime to prevent concurrent deploys.
    lock_file: Option<File>,
}

impl HostSession {
    pub fn new(
        repo: Repository,
        hostname: Hostname,
        apps_dir: PathBuf,
        unit_dir: PathBuf,
        on_units_changed: Box<dyn Fn(&crate::plan::UnitChanges) -> crate::error::Result<()>>,
    ) -> Self {
        HostSession {
            repo,
            hostname,
            apps_dir,
            unit_dir,
            on_units_changed,
            lock_file: None,
        }
    }

    /// Create a session for testing that does not touch systemd.
    #[cfg(test)]
    pub fn new_test(
        repo: Repository,
        hostname: &str,
        apps_dir: &std::path::Path,
        unit_dir: &std::path::Path,
    ) -> Self {
        // In tests, skip the daemon-reload + restart step.
        let on_units_changed = Box::new(|_: &_| Ok(()));
        HostSession::new(
            repo,
            hostname.into(),
            apps_dir.to_path_buf(),
            unit_dir.to_path_buf(),
            on_units_changed,
        )
    }

    /// Send a request and collect all responses. Test-only convenience.
    #[cfg(test)]
    pub fn handle_collect(
        &mut self,
        request: crate::protocol::Request,
    ) -> Vec<crate::protocol::Message> {
        let mut responses = Vec::new();
        self.handle_request(request, &mut |r| responses.push(r));
        responses
    }

    /// Wrap this session into a boxed `Connection` for deploy tests.
    #[cfg(test)]
    pub fn into_test_connection(self) -> Box<dyn crate::deploy::Connection> {
        crate::testutil::session_into_connection(self)
    }

    fn current_commit(&self) -> Option<Oid> {
        self.repo
            .find_reference("refs/heads/current")
            .ok()
            .map(|r| r.peel_to_commit().expect("current ref points to a commit"))
            .map(|c| c.id().into())
    }

    fn handle_lock(
        &mut self,
        expected_current_commit: Option<crate::prim::Oid>,
        emit_message: &mut impl FnMut(Message),
    ) {
        let lock_path = self.repo.path().join("deptool.lock");
        let file = match File::create(&lock_path) {
            Ok(f) => f,
            Err(err) => {
                emit_message(Message::Error {
                    message: format!("failed to open lock file: {err}"),
                });
                return;
            }
        };

        match try_flock_exclusive(&file) {
            Ok(false) => {
                emit_message(Message::LockBusy);
                return;
            }
            Err(err) => {
                emit_message(Message::Error {
                    message: format!("flock failed: {err}"),
                });
                return;
            }
            Ok(true) => {}
        }

        let actual_current_commit = self.current_commit();
        if actual_current_commit != expected_current_commit {
            // Stale: release the lock, the client won't be deploying.
            emit_message(Message::LockStale {
                expected_commit: expected_current_commit,
                actual_commit: actual_current_commit,
            });
        } else {
            // Hold the lock for the session lifetime.
            self.lock_file = Some(file);
            emit_message(Message::Locked);
        }
    }

    fn handle_receive_pack(&self, pack_data: &str, emit_message: &mut impl FnMut(Message)) {
        let bytes = BASE64.decode(pack_data).expect("pack_data is valid base64");
        match crate::store::write_pack(&self.repo, &bytes) {
            Ok(()) => emit_message(Message::PackReceived),
            Err(err) => emit_message(Message::Error {
                message: format!("failed to write pack: {err}"),
            }),
        }
    }

    pub fn handle_request(&mut self, request: Request, emit_message: &mut impl FnMut(Message)) {
        match request {
            Request::Lock {
                expected_current_commit,
            } => self.handle_lock(expected_current_commit, emit_message),
            Request::ReceivePack { ref pack_data } => {
                self.handle_receive_pack(pack_data, emit_message)
            }
            Request::RequestObjects { have_commit } => {
                // The driver only sends RequestObjects after LockStale
                // reported an actual_commit, so we must have one.
                let commit = self
                    .current_commit()
                    .expect("RequestObjects implies a current commit exists");
                let git_oid = git2::Oid::from(&commit);
                let have = have_commit.as_ref().map(git2::Oid::from);
                match crate::store::create_pack(&self.repo, git_oid, have) {
                    Ok(bytes) => emit_message(Message::SendPack {
                        pack_data: BASE64.encode(&bytes),
                    }),
                    Err(err) => emit_message(Message::Error {
                        message: format!("failed to create pack: {err}"),
                    }),
                }
            }
            Request::Apply { target_commit } => {
                let current_commit = self.current_commit();

                let result = crate::apply::apply_host(
                    &self.repo,
                    git2::Oid::from(&target_commit),
                    current_commit.map(git2::Oid::from),
                    &self.hostname,
                    &self.apps_dir,
                    &self.unit_dir,
                    |app, diff| {
                        emit_message(Message::AppliedApp {
                            app: app.to_string(),
                            diff: diff.clone(),
                        });
                    },
                );

                let unit_changes = match result {
                    Ok(changes) => changes,
                    Err(err) => {
                        emit_message(Message::Error {
                            message: format!("apply failed: {err}"),
                        });
                        return;
                    }
                };

                if !unit_changes.is_empty() {
                    if let Err(err) = (self.on_units_changed)(&unit_changes) {
                        emit_message(Message::Error {
                            message: format!("systemd restart failed: {err}"),
                        });
                        return;
                    }
                }

                emit_message(Message::ApplyComplete {
                    commit: target_commit,
                    enabled_units: unit_changes.enable,
                    restarted_units: unit_changes.restart,
                    disabled_units: unit_changes.disable,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::AppDiff;
    use crate::testutil::TestHost;

    #[test]
    fn apply_checks_out_app_and_emits_per_app_messages() {
        let (mut host, oid) = TestHost::with_commit(&[("web1/nginx/nginx.conf", b"server {}")]);
        let responses = host.collect(Request::Apply {
            target_commit: oid.into(),
        });

        assert_eq!(responses.len(), 2);
        match &responses[0] {
            Message::AppliedApp { app, diff } => {
                assert_eq!(app, "nginx");
                assert!(matches!(diff, AppDiff::Add { .. }));
            }
            other => panic!("Expected AppliedApp, got {other:?}"),
        }
        assert!(matches!(&responses[1], Message::ApplyComplete { .. }));
    }

    #[test]
    fn lock_succeeds_on_fresh_host_with_no_expected_current() {
        let mut host = TestHost::new();
        let responses = host.collect(Request::Lock {
            expected_current_commit: None,
        });
        assert_eq!(responses, vec![Message::Locked]);
    }

    #[test]
    fn lock_succeeds_when_current_ref_matches_expected() {
        let (mut host, oid) = TestHost::with_commit(&[("web1/nginx/conf", b"v1")]);
        crate::store::set_ref(
            &host.session.repo,
            "refs/heads/current",
            oid,
            crate::store::RefUpdate::SetCurrent,
        )
        .expect("ref is set");
        let responses = host.collect(Request::Lock {
            expected_current_commit: Some(oid.into()),
        });
        assert_eq!(responses, vec![Message::Locked]);
    }

    #[test]
    fn lock_reports_stale_when_current_ref_mismatches() {
        let (mut host, oid) = TestHost::with_commit(&[("web1/nginx/conf", b"v1")]);
        crate::store::set_ref(
            &host.session.repo,
            "refs/heads/current",
            oid,
            crate::store::RefUpdate::SetCurrent,
        )
        .expect("ref is set");
        let responses = host.collect(Request::Lock {
            expected_current_commit: None,
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
    fn lock_reports_busy_when_already_held() {
        let mut host = TestHost::new();

        // Acquire the lock from another file descriptor.
        let lock_path = host.session.repo.path().join("deptool.lock");
        let lock_holder = File::create(&lock_path).expect("lock file is created");
        assert!(
            try_flock_exclusive(&lock_holder).expect("flock succeeds"),
            "lock is acquired",
        );

        let responses = host.collect(Request::Lock {
            expected_current_commit: None,
        });
        assert_eq!(responses, vec![Message::LockBusy]);

        drop(lock_holder);
    }
}

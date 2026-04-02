//! Host-side session logic: handles requests and applies changes.

use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use git2::Repository;

use crate::prim::Hostname;
use crate::prim::Oid;
use crate::protocol::{Message, Request};

pub struct HostSession {
    repo: Repository,
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
    fn new_test(
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

    fn current_commit(&self) -> Option<Oid> {
        self.repo
            .find_reference("refs/heads/current")
            .ok()
            .map(|r| r.peel_to_commit().expect("current ref points to a commit"))
            .map(|c| c.id().into())
    }

    fn handle_lock(
        &mut self,
        expected_current_commit: Option<Oid>,
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

        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                emit_message(Message::LockBusy);
            } else {
                emit_message(Message::Error {
                    message: format!("flock failed: {err}"),
                });
            }
            return;
        }

        self.lock_file = Some(file);

        let actual_current_commit = self.current_commit();
        if actual_current_commit != expected_current_commit {
            emit_message(Message::LockStale {
                expected_commit: expected_current_commit,
                actual_commit: actual_current_commit,
            });
        } else {
            emit_message(Message::Locked);
        }
    }

    fn handle_receive_pack(
        &self,
        pack_data: &str,
        emit_message: &mut impl FnMut(Message),
    ) {
        let bytes = match BASE64.decode(pack_data) {
            Ok(b) => b,
            Err(err) => {
                emit_message(Message::Error {
                    message: format!("invalid base64: {err}"),
                });
                return;
            }
        };
        let odb = match self.repo.odb() {
            Ok(odb) => odb,
            Err(err) => {
                emit_message(Message::Error {
                    message: format!("failed to open odb: {err}"),
                });
                return;
            }
        };
        let mut writer = match odb.packwriter() {
            Ok(w) => w,
            Err(err) => {
                emit_message(Message::Error {
                    message: format!("failed to create packwriter: {err}"),
                });
                return;
            }
        };
        if let Err(err) = writer.write_all(&bytes) {
            emit_message(Message::Error {
                message: format!("failed to write pack: {err}"),
            });
            return;
        }
        if let Err(err) = writer.commit() {
            emit_message(Message::Error {
                message: format!("failed to commit pack: {err}"),
            });
            return;
        }
        emit_message(Message::PackReceived);
    }

    pub fn handle_request(&mut self, request: Request, emit_message: &mut impl FnMut(Message)) {
        match request {
            Request::Lock {
                expected_current_commit,
            } => self.handle_lock(expected_current_commit, emit_message),
            Request::ReceivePack { ref pack_data } => {
                self.handle_receive_pack(pack_data, emit_message)
            }
            Request::Apply {
                expected_current_commit,
                target_commit,
            } => {
                let actual_current_commit = self.current_commit();
                if actual_current_commit != expected_current_commit {
                    emit_message(Message::Stale {
                        expected_commit: expected_current_commit,
                        actual_commit: actual_current_commit,
                    });
                    return;
                }

                let result = crate::apply::apply_host(
                    &self.repo,
                    git2::Oid::from(&target_commit),
                    actual_current_commit.map(git2::Oid::from),
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
    use std::os::unix::io::AsRawFd;

    use super::*;
    use crate::plan::AppDiff;
    use crate::testutil::{TempDir, commit_files};

    struct TestEnv {
        session: HostSession,
        _store: TempDir,
        _apps: TempDir,
        _units: TempDir,
    }

    fn test_env() -> TestEnv {
        let store = TempDir::new("store");
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        let repo = Repository::init_bare(store.path()).expect("repo is created");
        let session = HostSession::new_test(repo, "web1", apps.path(), units.path());
        TestEnv {
            session,
            _store: store,
            _apps: apps,
            _units: units,
        }
    }

    fn test_env_with_commit(files: &[(&str, &[u8])]) -> (TestEnv, git2::Oid) {
        let store = TempDir::new("store");
        let apps = TempDir::new("apps");
        let units = TempDir::new("units");
        let repo = Repository::init_bare(store.path()).expect("repo is created");
        let oid = commit_files(&repo, files).expect("commit succeeds");
        let session = HostSession::new_test(repo, "web1", apps.path(), units.path());
        (
            TestEnv {
                session,
                _store: store,
                _apps: apps,
                _units: units,
            },
            oid,
        )
    }

    fn collect(session: &mut HostSession, request: Request) -> Vec<Message> {
        let mut responses = Vec::new();
        session.handle_request(request, &mut |r| responses.push(r));
        responses
    }

    #[test]
    fn apply_reports_stale_when_expected_current_mismatches() {
        let mut env = test_env();
        let commit: Oid = "0000000000000000000000000000000000000000".into();
        let fake_current: Oid = "1111111111111111111111111111111111111111".into();
        let req = Request::Apply {
            target_commit: commit,
            expected_current_commit: Some(fake_current),
        };
        let responses = collect(&mut env.session, req);
        assert_eq!(responses.len(), 1);
        assert!(matches!(&responses[0], Message::Stale { .. }));
    }

    #[test]
    fn apply_checks_out_app_and_emits_per_app_messages() {
        let (mut env, oid) = test_env_with_commit(&[("web1/nginx/nginx.conf", b"server {}")]);
        let req = Request::Apply {
            target_commit: oid.into(),
            expected_current_commit: None,
        };
        let responses = collect(&mut env.session, req);

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
        let mut env = test_env();
        let req = Request::Lock {
            expected_current_commit: None,
        };
        let responses = collect(&mut env.session, req);
        assert_eq!(responses, vec![Message::Locked]);
    }

    #[test]
    fn lock_succeeds_when_current_ref_matches_expected() {
        let (mut env, oid) = test_env_with_commit(&[("web1/nginx/conf", b"v1")]);
        crate::store::set_ref(
            &env.session.repo,
            "refs/heads/current",
            oid,
            crate::store::RefUpdate::SetCurrent,
        )
        .expect("ref is set");
        let req = Request::Lock {
            expected_current_commit: Some(oid.into()),
        };
        let responses = collect(&mut env.session, req);
        assert_eq!(responses, vec![Message::Locked]);
    }

    #[test]
    fn lock_reports_stale_when_current_ref_mismatches() {
        let (mut env, oid) = test_env_with_commit(&[("web1/nginx/conf", b"v1")]);
        crate::store::set_ref(
            &env.session.repo,
            "refs/heads/current",
            oid,
            crate::store::RefUpdate::SetCurrent,
        )
        .expect("ref is set");
        let req = Request::Lock {
            expected_current_commit: None,
        };
        let responses = collect(&mut env.session, req);
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
        let mut env = test_env();

        // Acquire the lock from another file descriptor.
        let lock_path = env.session.repo.path().join("deptool.lock");
        let lock_holder = File::create(&lock_path).expect("lock file is created");
        let rc =
            unsafe { libc::flock(lock_holder.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(rc, 0, "lock is acquired");

        let req = Request::Lock {
            expected_current_commit: None,
        };
        let responses = collect(&mut env.session, req);
        assert_eq!(responses, vec![Message::LockBusy]);

        drop(lock_holder);
    }
}

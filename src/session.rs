//! Host-side session logic: handles requests and applies changes.

use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use git2::Oid;

use crate::apply::apply_host;
use crate::error::Result;
use crate::plan::UnitChanges;
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

pub struct HostSession {
    pub store: Store,
    pub hostname: Hostname,
    apps_dir: PathBuf,
    unit_dir: PathBuf,
    on_units_changed: Box<dyn Fn(&UnitChanges, &mut dyn FnMut(Message)) -> Result<()>>,
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

impl HostSession {
    pub fn new(
        store: Store,
        hostname: Hostname,
        apps_dir: PathBuf,
        unit_dir: PathBuf,
        on_units_changed: Box<dyn Fn(&UnitChanges, &mut dyn FnMut(Message)) -> Result<()>>,
    ) -> Self {
        HostSession {
            store,
            hostname,
            apps_dir,
            unit_dir,
            on_units_changed,
            lock: None,
        }
    }

    /// Create a session for testing that does not touch systemd.
    #[doc(hidden)]
    pub fn new_test(
        repo: git2::Repository,
        hostname: &str,
        apps_dir: &std::path::Path,
        unit_dir: &std::path::Path,
    ) -> Self {
        let on_units_changed = Box::new(|_: &_, _: &mut dyn FnMut(Message)| Ok(()));
        HostSession::new(
            Store { repo },
            hostname.into(),
            apps_dir.to_path_buf(),
            unit_dir.to_path_buf(),
            on_units_changed,
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
                // TODO: Read operator identity from the lockfile so we can
                // report *who* holds the lock in the LockBusy message.
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
            Err(err) => emit_message(Message::Error {
                message: format!("failed to write pack: {err}"),
            }),
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
                    Err(err) => emit_message(Message::Error {
                        message: format!("failed to create pack: {err}"),
                    }),
                }
            }
            Request::Apply { target_commit } => {
                let current_commit = self.current_commit();

                let operator = &self
                    .lock
                    .as_ref()
                    .expect("lock is held during apply")
                    .operator;
                let result = apply_host(
                    &self.store,
                    target_commit,
                    current_commit,
                    &self.hostname,
                    &self.apps_dir,
                    &self.unit_dir,
                    operator,
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
                    if let Err(err) = (self.on_units_changed)(&unit_changes, emit_message) {
                        emit_message(Message::Error {
                            message: format!("systemd unit change failed: {err}"),
                        });
                        return;
                    }
                }

                self.store
                    .set_ref(
                        "refs/heads/current",
                        target_commit,
                        RefUpdate::SetCurrent { operator },
                    )
                    .expect("updating current ref succeeds while we hold the lock");

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
    fn lock_reports_busy_when_already_held() {
        let mut host = TestHost::new("web1");

        // Acquire the lock from another file descriptor.
        let lock_path = host.session.store.get_lock_file_path();
        let lock_holder = File::create(&lock_path).expect("lock file is created");
        assert!(
            try_flock_exclusive(&lock_holder).expect("flock succeeds"),
            "lock is acquired",
        );

        let responses = host.interact(Request::Lock {
            expected_current_commit: None,
            operator: "deckard@spinner".into(),
        });
        assert_eq!(responses, vec![Message::LockBusy]);

        drop(lock_holder);
    }
}

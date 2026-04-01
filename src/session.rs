//! Host-side session logic: handles requests and applies changes.

use std::path::PathBuf;

use git2::Repository;

use crate::oid::Oid;
use crate::protocol::{Message, Request};

pub struct HostSession {
    repo: Repository,
    hostname: String,
    apps_dir: PathBuf,
    unit_dir: PathBuf,
    on_units_changed: Box<dyn Fn(&crate::apply::UnitChanges) -> crate::error::Result<()>>,
}

impl HostSession {
    pub fn new(
        repo: Repository,
        hostname: String,
        apps_dir: PathBuf,
        unit_dir: PathBuf,
        on_units_changed: Box<dyn Fn(&crate::apply::UnitChanges) -> crate::error::Result<()>>,
    ) -> Self {
        HostSession {
            repo,
            hostname,
            apps_dir,
            unit_dir,
            on_units_changed,
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
            hostname.to_string(),
            apps_dir.to_path_buf(),
            unit_dir.to_path_buf(),
            on_units_changed,
        )
    }

    pub fn handle_request(&self, request: Request, emit_message: &mut impl FnMut(Message)) {
        match request {
            Request::Apply {
                expected_current_commit,
                target_commit,
            } => {
                let actual_current_commit: Option<Oid> = self
                    .repo
                    .find_reference("refs/heads/current")
                    .ok()
                    .map(|r| r.peel_to_commit().expect("current ref points to a commit"))
                    .map(|c| c.id().into());

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

    fn collect(session: &HostSession, request: Request) -> Vec<Message> {
        let mut responses = Vec::new();
        session.handle_request(request, &mut |r| responses.push(r));
        responses
    }

    #[test]
    fn apply_reports_stale_when_expected_current_mismatches() {
        let env = test_env();
        let commit: Oid = "0000000000000000000000000000000000000000".into();
        let fake_current: Oid = "1111111111111111111111111111111111111111".into();
        let req = Request::Apply {
            target_commit: commit,
            expected_current_commit: Some(fake_current),
        };
        let responses = collect(&env.session, req);
        assert_eq!(responses.len(), 1);
        assert!(matches!(&responses[0], Message::Stale { .. }));
    }

    #[test]
    fn apply_checks_out_app_and_emits_per_app_messages() {
        let (env, oid) = test_env_with_commit(&[("web1/nginx/nginx.conf", b"server {}")]);
        let req = Request::Apply {
            target_commit: oid.into(),
            expected_current_commit: None,
        };
        let responses = collect(&env.session, req);

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
}

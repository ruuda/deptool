//! Host-side session logic: handles requests and applies changes.

use git2::Repository;

use crate::oid::Oid;
use crate::protocol::{Message, Request};

pub struct HostSession {
    repo: Repository,
}

impl HostSession {
    pub fn new(repo: Repository) -> Self {
        HostSession { repo }
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

                // TODO: Actually apply the commit using store::apply,
                // emitting per-app events along the way.
                emit_message(Message::ApplyComplete {
                    commit: target_commit,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn test_session() -> HostSession {
        let store = TempDir::new("store");
        let repo = Repository::init_bare(store.path()).expect("repo is created");
        std::mem::forget(store);
        HostSession::new(repo)
    }

    fn collect(session: &HostSession, request: Request) -> Vec<Message> {
        let mut responses = Vec::new();
        session.handle_request(request, &mut |r| responses.push(r));
        responses
    }

    #[test]
    fn apply_reports_stale_when_expected_current_mismatches() {
        let session = test_session();
        let commit: Oid = "0000000000000000000000000000000000000000".into();
        let fake_current: Oid = "1111111111111111111111111111111111111111".into();
        let req = Request::Apply {
            target_commit: commit,
            expected_current_commit: Some(fake_current),
        };
        let responses = collect(&session, req);
        assert_eq!(responses.len(), 1);
        assert!(matches!(&responses[0], Message::Stale { .. }));
    }

    #[test]
    fn apply_emit_messages_applied_with_same_commit() {
        let session = test_session();
        let commit = Oid::from(
            git2::Oid::from_str("0000000000000000000000000000000000000000").expect("oid is valid"),
        );
        let req = Request::Apply {
            target_commit: commit.clone(),
            expected_current_commit: None,
        };
        let responses = collect(&session, req);
        assert_eq!(responses.len(), 1);
        match &responses[0] {
            Message::ApplyComplete { commit: c } => assert_eq!(c, &commit),
            other => panic!("Expected Applied, got {other:?}"),
        }
    }
}

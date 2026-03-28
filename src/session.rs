use git2::Repository;

use crate::protocol::{Request, Response};

pub struct HostSession {
    repo: Repository,
}

impl HostSession {
    pub fn new(repo: Repository) -> Self {
        HostSession { repo }
    }

    pub fn handle_request(&self, request: Request, emit_message: &mut impl FnMut(Response)) {
        match request {
            Request::Apply { commit } => {
                // TODO: Actually apply the commit using store::apply,
                // emit_messageting per-app events along the way.
                let _ = &self.repo;
                emit_message(Response::Applied { commit });
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
        let repo = git2::Repository::init_bare(store.path()).expect("repo is created");
        std::mem::forget(store);
        HostSession::new(repo)
    }

    fn collect(session: &HostSession, request: Request) -> Vec<Response> {
        let mut responses = Vec::new();
        session.handle_request(request, &mut |r| responses.push(r));
        responses
    }

    #[test]
    fn apply_emit_messages_applied_with_same_commit() {
        let session = test_session();
        let commit = crate::oid::Oid::from(
            git2::Oid::from_str("0000000000000000000000000000000000000000").expect("oid is valid"),
        );
        let req = Request::Apply {
            commit: commit.clone(),
        };
        let responses = collect(&session, req);
        assert_eq!(responses.len(), 1);
        match &responses[0] {
            Response::Applied { commit: c } => assert_eq!(c, &commit),
            other => panic!("Expected Applied, got {other:?}"),
        }
    }
}

// Deptool -- A declarative configuration deployment tool.
// Copyright 2026 Ruud van Asseldonk

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// A copy of the License has been included in the root of the repository.

//! The `deptool ping` command: measure round-trip latency to each host.

use std::path::Path;
use std::time::Instant;

use crate::deploy::{self, DeployProgress, HostState};
use crate::error::{HostError, Result};
use crate::plan::HostFilter;
use crate::prim::Hostname;
use crate::protocol::{Message, Request};
use crate::setup::HostConnector;
use crate::store::Store;

/// List the hosts to ping from a config tree, applying `filter`.
pub fn select_hosts_to_ping(
    store: &Store,
    dir: &Path,
    filter: &HostFilter,
) -> Result<Vec<Hostname>> {
    let tree_oid = store.build_tree(dir)?;
    let mut hosts = store.host_trees(tree_oid)?;
    filter.apply(&mut hosts)?;
    Ok(hosts.into_keys().collect())
}

/// Connect to each host, measure the Ping/Pong round-trip, report via progress.
///
/// One thread per host. The measured RTT covers the agent session's request
/// dispatch and serialization round-trip; SSH connection setup happens
/// inside `try_connect` and is not included.
pub fn run_ping(hosts: &[Hostname], connector: &dyn HostConnector, progress: &DeployProgress) {
    std::thread::scope(|s| {
        for host in hosts {
            s.spawn(move || match ping_host(host, connector, progress) {
                Ok(()) => {}
                Err(err) => progress.update(host, err),
            });
        }
    });
}

fn ping_host(
    host: &Hostname,
    connector: &dyn HostConnector,
    progress: &DeployProgress,
) -> std::result::Result<(), HostError> {
    let mut conn = match deploy::try_connect(host, connector, progress) {
        Some(c) => c,
        None => return Ok(()),
    };

    progress.update(host, HostState::Pinging);
    // `Instant` is monotonic, so this RTT measurement is unaffected by
    // wall-clock adjustments on the operator machine.
    let start = Instant::now();
    conn.send_request(&Request::Ping)?;
    match conn.read_message()? {
        Some(Message::Pong) => {
            let rtt = start.elapsed();
            progress.update(host, HostState::Pinged { rtt });
            Ok(())
        }
        other => Err(HostError::ProtocolError(format!(
            "unexpected response to Ping: {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{TestHost, test_connector, test_progress};

    #[test]
    fn ping_reports_pinged_state_for_each_host() {
        let web1 = TestHost::new("web1");
        let web2 = TestHost::new("web2");
        let hosts = vec![Hostname::from("web1"), Hostname::from("web2")];
        let progress = test_progress(&["web1", "web2"]);
        let connector = test_connector(&[&web1, &web2]);

        run_ping(&hosts, &connector, &progress);

        assert!(matches!(*progress.state("web1"), HostState::Pinged { .. }));
        assert!(matches!(*progress.state("web2"), HostState::Pinged { .. }));
    }

    #[test]
    fn ping_reports_failure_for_unreachable_host() {
        let web1 = TestHost::new("web1");
        let hosts = vec![Hostname::from("web1"), Hostname::from("web2")];
        let progress = test_progress(&["web1", "web2"]);
        // web2 is in the plan but has no connection factory: connect fails.
        let connector = test_connector(&[&web1]);

        run_ping(&hosts, &connector, &progress);

        assert!(matches!(*progress.state("web1"), HostState::Pinged { .. }));
        assert!(matches!(*progress.state("web2"), HostState::Failed(_)));
    }
}

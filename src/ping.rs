// Deptool -- A declarative configuration deployment tool.
// Copyright 2026 Ruud van Asseldonk

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// A copy of the License has been included in the root of the repository.

//! The `deptool ping` command: measure round-trip latency to each host.

use std::fmt;
use std::time::{Duration, Instant};

use crate::deploy::{self, DeployProgress, HostState};
use crate::error::HostError;
use crate::prim::Hostname;
use crate::protocol::{Message, Request};
use crate::setup::HostConnector;

/// Below this many samples, p95 is essentially `max` and would mislead the
/// operator about tail behavior, so we hide it until N grows.
const P95_MIN_SAMPLES: usize = 20;

/// We send 150 pings spaced 200 ms apart, so a full run takes 30 seconds. The
/// first second already gives meaningful min and p50, and p95 stabilizes
/// within a few seconds. The operator can Ctrl+C an in-progress run, so
/// erring on the long side is fine.
pub const PING_COUNT: u32 = 150;
pub const PING_PERIOD: Duration = Duration::from_millis(200);

/// Summary of an ongoing or completed ping run for a single host.
#[derive(Debug, PartialEq, Eq)]
pub struct PingStats {
    pub count: usize,
    pub min: Option<Duration>,
    pub p50: Option<Duration>,
    pub p95: Option<Duration>,
}

impl PingStats {
    /// Compute order statistics over `samples` using the nearest-rank method.
    pub fn compute(samples: &[Duration]) -> Self {
        let mut sorted = samples.to_vec();
        sorted.sort();
        let n = sorted.len();
        // Nearest-rank percentile: the p-th percentile is the smallest sample
        // with at least p% of samples at or below it, at 0-indexed position
        // `ceil(n * p / 100) - 1`. `saturating_sub` and `.get()` together
        // turn the empty-samples case into `None` instead of an underflow.
        let pct = |p: usize| sorted.get((n * p).div_ceil(100).saturating_sub(1)).copied();
        Self {
            count: n,
            min: sorted.first().copied(),
            p50: pct(50),
            p95: pct(95).filter(|_| n >= P95_MIN_SAMPLES),
        }
    }
}

impl fmt::Display for PingStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_slot(self.min, f)?;
        f.write_str(" | ")?;
        fmt_slot(self.p50, f)?;
        f.write_str(" | ")?;
        fmt_slot(self.p95, f)?;
        write!(f, "  (min/p50/p95 rtt, n={})", self.count)
    }
}

/// Width 5 keeps three-digit ms aligned; the `--` placeholder right-aligns
/// to the same width via the same format spec.
fn fmt_slot(d: Option<Duration>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match d {
        Some(d) => write!(f, "{:>5.1} ms", d.as_secs_f64() * 1000.0),
        None => write!(f, "{:>5} ms", "--"),
    }
}

/// Connect to each host, run a ping series, report stats live via progress.
///
/// One thread per host; hosts run independently. RTT covers the agent
/// session's request/response round-trip; SSH connection setup is in
/// `try_connect` and is not included.
pub fn run_ping(
    count: u32,
    period: Duration,
    hosts: &[Hostname],
    connector: &dyn HostConnector,
    progress: &DeployProgress,
) {
    std::thread::scope(|s| {
        for host in hosts {
            s.spawn(
                move || match ping_host(count, period, host, connector, progress) {
                    Ok(()) => {}
                    Err(err) => progress.update(host, err),
                },
            );
        }
    });
}

fn ping_host(
    count: u32,
    period: Duration,
    host: &Hostname,
    connector: &dyn HostConnector,
    progress: &DeployProgress,
) -> std::result::Result<(), HostError> {
    let mut conn = match deploy::try_connect(host, connector, progress) {
        Some(c) => c,
        None => return Ok(()),
    };

    // `Instant` is monotonic, so RTT and inter-ping cadence are unaffected
    // by wall-clock adjustments.
    let mut samples = Vec::with_capacity(count as usize);
    let start = Instant::now();
    for i in 0..count {
        let deadline = start + period * i;
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
        let t0 = Instant::now();
        conn.send_request(&Request::Ping)?;
        match conn.read_message()? {
            Some(Message::Pong) => samples.push(t0.elapsed()),
            other => {
                return Err(HostError::ProtocolError(format!(
                    "unexpected response to Ping: {other:?}"
                )));
            }
        }
        progress.update(
            host,
            HostState::Pinging {
                stats: PingStats::compute(&samples),
            },
        );
    }
    progress.update(
        host,
        HostState::Pinged {
            stats: PingStats::compute(&samples),
        },
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_stats_picks_min_p50_at_nearest_rank_p95_gated_by_count() {
        // 19 samples [1..=19] ms. Below threshold so p95 is hidden.
        // p50: ceil(19 * 50 / 100) - 1 = 9. sorted[9] = 10 ms.
        let samples: Vec<Duration> = (1..=19).map(Duration::from_millis).collect();
        let stats = PingStats::compute(&samples);
        assert_eq!(stats.count, 19);
        assert_eq!(stats.min, Some(Duration::from_millis(1)));
        assert_eq!(stats.p50, Some(Duration::from_millis(10)));
        assert_eq!(stats.p95, None);

        // 20 samples [1..=20] ms. At threshold so p95 appears.
        // p50: ceil(20 * 50 / 100) - 1 = 9. sorted[9] = 10 ms.
        // p95: ceil(20 * 95 / 100) - 1 = 18. sorted[18] = 19 ms.
        let samples: Vec<Duration> = (1..=20).map(Duration::from_millis).collect();
        let stats = PingStats::compute(&samples);
        assert_eq!(stats.count, 20);
        assert_eq!(stats.min, Some(Duration::from_millis(1)));
        assert_eq!(stats.p50, Some(Duration::from_millis(10)));
        assert_eq!(stats.p95, Some(Duration::from_millis(19)));

        // Empty: count 0, all None.
        assert_eq!(PingStats::compute(&[]).count, 0);
        assert_eq!(PingStats::compute(&[]).min, None);
    }

    #[test]
    fn ping_stats_display_aligns_three_digits_and_pending_p95() {
        let stats = PingStats {
            count: 12,
            min: Some(Duration::from_micros(800)), // sub-1: "  0.8"
            p50: Some(Duration::from_micros(127_500)), // 3-digit: "127.5"
            p95: None,                             // pending: "   --"
        };
        assert_eq!(
            stats.to_string(),
            "  0.8 ms | 127.5 ms |    -- ms  (min/p50/p95 rtt, n=12)",
        );
    }
}

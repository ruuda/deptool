// Deptool -- A declarative configuration deployment tool.
// Copyright 2026 Ruud van Asseldonk

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// A copy of the License has been included in the root of the repository.

//! The `deptool ping` command: measure round-trip latency to each host.

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

/// Sparkline alphabet, ascending bar height. We skip the full block `█`
/// because it visually clips into the row above. Empty bins render as
/// `⣀` (Braille dots-7-8) — a ghost baseline that's lighter than `▁` but
/// still keeps the bin slot visible. Any non-empty bin shows at least
/// `▁` so a thin tail to the left of the peak doesn't vanish.
const BARS: [char; 7] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇'];

/// Character drawn for empty bins; see `BARS` for rationale.
const EMPTY_BIN: char = '⣀';

/// Number of histogram bins per sparkline. 25 keeps the line under 100
/// columns for typical hostnames while giving N=150 ~6 samples per bin.
const BAR_COUNT: usize = 25;

/// Order statistics over a set of ping samples.
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

/// Render the full ping status line: stats + sparkline + legend.
///
/// `x_scale` sets the sparkline's X-axis upper bound. Bar heights are
/// normalized to this host's own peak bin, so the sparkline shape is
/// comparable across hosts even when one is much busier than another.
pub fn render(samples: &[Duration], x_scale: Duration) -> String {
    let stats = PingStats::compute(samples);
    format!(
        "{} ms | {} {}  {}  (p50/min/p95 rtt, n={})",
        fmt_ms(stats.p50),
        fmt_ms(stats.min),
        render_sparkline(samples, x_scale),
        fmt_ms(stats.p95),
        stats.count,
    )
}

/// Width 5 keeps three-digit ms aligned; the `--` placeholder right-aligns
/// to the same width via the same format spec.
fn fmt_ms(d: Option<Duration>) -> String {
    match d {
        Some(d) => format!("{:>5.1}", d.as_secs_f64() * 1000.0),
        None => format!("{:>5}", "--"),
    }
}

/// Bin samples over `[0, x_scale)` into `BAR_COUNT` bins; render bin counts
/// as sparkline characters with this host's peak bin at the top of `BARS`.
/// Empty bins render as `EMPTY_BIN`; non-empty bins are at least `BARS[0]`
/// so a thin tail to the left of the peak doesn't get rounded to invisible.
fn render_sparkline(samples: &[Duration], x_scale: Duration) -> String {
    let mut bins = [0usize; BAR_COUNT];
    if !x_scale.is_zero() {
        let scale_secs = x_scale.as_secs_f64();
        for s in samples {
            let idx = ((s.as_secs_f64() / scale_secs) * BAR_COUNT as f64) as usize;
            bins[idx.min(BAR_COUNT - 1)] += 1;
        }
    }
    // When all bins are empty `max` is 0, but the `c == 0` branch handles
    // every cell before we'd divide by it.
    let max = bins.iter().copied().max().expect("BAR_COUNT > 0");
    bins.iter()
        .map(|&c| {
            if c == 0 {
                EMPTY_BIN
            } else {
                BARS[c * (BARS.len() - 1) / max]
            }
        })
        .collect()
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
        progress.update(host, HostState::Pinging { samples: samples.clone() });
    }
    progress.update(host, HostState::Pinged { samples });
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
    fn render_sparkline_distributes_samples_into_bins() {
        // 100 ms scale, 25 bins → 4 ms per bin. One sample at 0 ms (bin 0),
        // three at 50 ms (bin 12), one at 99 ms (bin 24). Max bin count = 3,
        // so bin 12 reaches the top of the alphabet (▇) and bins 0 and 24
        // sit at one third (▃). Empty bins render as the `⣀` baseline.
        let samples = [0, 50, 50, 50, 99].map(Duration::from_millis);
        assert_eq!(
            render_sparkline(&samples, Duration::from_millis(100)),
            "▃⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀▇⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀▃",
        );
    }

    #[test]
    fn render_sparkline_baseline_for_empty_or_zero_scale() {
        let baseline = "⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀";
        assert_eq!(render_sparkline(&[], Duration::from_millis(100)), baseline);
        assert_eq!(
            render_sparkline(&[Duration::from_millis(5)], Duration::ZERO),
            baseline,
        );
    }

    #[test]
    fn render_sparkline_keeps_thin_tail_visible() {
        // 100 samples at 50 ms (bin 12, peak) and 1 sample at 0 ms (bin 0).
        // Naïve `1 * 6 / 100 = 0` would round the lone tail sample down to
        // an empty bin and hide it; the floor at `BARS[0]` keeps it as `▁`.
        let mut samples = vec![Duration::from_millis(50); 100];
        samples.push(Duration::from_millis(0));
        assert_eq!(
            render_sparkline(&samples, Duration::from_millis(100)),
            "▁⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀▇⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀",
        );
    }

    #[test]
    fn render_full_line_format() {
        // 30 samples all at 5 ms; scale 10 ms (bin width 0.4 ms) puts every
        // sample in bin 12. p50 = min = p95 = 5 ms.
        let samples = vec![Duration::from_millis(5); 30];
        assert_eq!(
            render(&samples, Duration::from_millis(10)),
            "  5.0 ms |   5.0 ⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀▇⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀    5.0  (p50/min/p95 rtt, n=30)",
        );
    }

    #[test]
    fn render_pending_p95_uses_dash_placeholder() {
        // 8 samples (below the p95 threshold), all at 5 ms.
        let samples = vec![Duration::from_millis(5); 8];
        assert_eq!(
            render(&samples, Duration::from_millis(10)),
            "  5.0 ms |   5.0 ⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀▇⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀     --  (p50/min/p95 rtt, n=8)",
        );
    }
}

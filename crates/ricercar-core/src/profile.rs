//! Opt-in timing traces for performance work. With `RICERCAR_PROFILE=1`,
//! measurements are printed to stderr as `[profile] <what>: <value>` so
//! scripts (`scripts/perf.sh`) can collect them whatever `RUST_LOG` says.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

static START: OnceLock<Instant> = OnceLock::new();

/// Remember when the process started; call first thing in `main`.
pub fn mark_start() {
    START.get_or_init(Instant::now);
}

/// Time since `mark_start` (or since the first call).
pub fn since_start() -> Duration {
    START.get_or_init(Instant::now).elapsed()
}

pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("RICERCAR_PROFILE").is_ok_and(|v| v == "1"))
}

/// Print a duration in milliseconds.
pub fn record(what: &str, d: Duration) {
    if enabled() {
        eprintln!("[profile] {what}: {:.1} ms", d.as_secs_f64() * 1e3);
    }
}

/// Print any other measurement.
pub fn note(what: &str, value: impl std::fmt::Display) {
    if enabled() {
        eprintln!("[profile] {what}: {value}");
    }
}

/// Times a scope and records it when dropped.
pub struct Span {
    what: String,
    start: Instant,
}

impl Drop for Span {
    fn drop(&mut self) {
        record(&self.what, self.start.elapsed());
    }
}

/// `let _p = profile::span("albums page");` — a no-op when profiling is off.
pub fn span(what: impl Into<String>) -> Option<Span> {
    enabled().then(|| Span {
        what: what.into(),
        start: Instant::now(),
    })
}

/// Resident memory of this process in MiB: (current, peak).
pub fn memory_mib() -> Option<(f64, f64)> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let field = |name: &str| -> Option<f64> {
        let line = status.lines().find(|l| l.starts_with(name))?;
        let kb: f64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb / 1024.0)
    };
    Some((field("VmRSS:")?, field("VmHWM:")?))
}

/// Median and 95th percentile of a set of samples.
pub fn percentiles(samples: &mut [Duration]) -> (Duration, Duration) {
    if samples.is_empty() {
        return (Duration::ZERO, Duration::ZERO);
    }
    samples.sort();
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q).round() as usize];
    (at(0.5), at(0.95))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_of_samples() {
        let mut s: Vec<Duration> = (1..=100).map(Duration::from_millis).collect();
        s.reverse();
        let (p50, p95) = percentiles(&mut s);
        assert_eq!(p50, Duration::from_millis(51));
        assert_eq!(p95, Duration::from_millis(95));
        assert_eq!(percentiles(&mut []), (Duration::ZERO, Duration::ZERO));
        assert!(memory_mib().is_some_and(|(rss, peak)| rss > 0.0 && peak >= rss));
    }
}

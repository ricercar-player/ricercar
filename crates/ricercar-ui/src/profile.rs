//! `RICERCAR_PROFILE=1` traces tied to rendering: time to the first frame,
//! and from a user action (page change, sort…) to the frame that shows it.

use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

pub use ricercar_core::profile::{enabled, memory_mib, note, record, span};

thread_local! {
    static FIRST_FRAME: Cell<bool> = const { Cell::new(false) };
    static PENDING: RefCell<Vec<(String, Instant)>> = const { RefCell::new(Vec::new()) };
    static TOTALS: RefCell<Vec<(&'static str, u32, Duration, Duration)>> = const { RefCell::new(Vec::new()) };
}

/// The next rendered frame completes `what`.
pub fn expect_frame(what: impl Into<String>) {
    if enabled() {
        PENDING.with(|p| p.borrow_mut().push((what.into(), Instant::now())));
    }
}

/// Called after each rendered frame.
pub fn frame_rendered() {
    if !enabled() {
        return;
    }
    if !FIRST_FRAME.with(|f| f.replace(true)) {
        record(
            "startup: first frame",
            ricercar_core::profile::since_start(),
        );
    }
    for (what, t) in PENDING.with(|p| std::mem::take(&mut *p.borrow_mut())) {
        record(&format!("{what}: frame"), t.elapsed());
    }
}

/// Accumulate a recurring cost (reported by `report_totals`).
pub fn add(what: &'static str, d: Duration) {
    if !enabled() {
        return;
    }
    TOTALS.with(|t| {
        let mut t = t.borrow_mut();
        match t.iter_mut().find(|e| e.0 == what) {
            Some(e) => {
                e.1 += 1;
                e.2 += d;
                e.3 = e.3.max(d);
            }
            None => t.push((what, 1, d, d)),
        }
    });
}

pub fn report_totals() {
    TOTALS.with(|t| {
        for (what, n, total, max) in t.borrow().iter() {
            note(
                what,
                format!(
                    "{n} calls, {:.1} ms total, {:.2} ms max",
                    total.as_secs_f64() * 1e3,
                    max.as_secs_f64() * 1e3
                ),
            );
        }
    });
}

pub fn memory(what: &str) {
    if let Some((rss, peak)) = memory_mib() {
        note(what, format!("{rss:.0} MiB (peak {peak:.0} MiB)"));
    }
}

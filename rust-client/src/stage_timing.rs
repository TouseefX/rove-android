//! Bounded aggregate timings; no samples or payloads are retained.
use std::time::Instant;
#[derive(Default, serde::Serialize)]
pub(crate) struct StageTiming {
    calls: u64,
    total_us: u64,
    max_us: u64,
}
impl StageTiming {
    pub(crate) fn observe(&mut self, started: Instant) {
        let us = started.elapsed().as_micros().min(u64::MAX as u128) as u64;
        self.calls += 1;
        self.total_us = self.total_us.saturating_add(us);
        self.max_us = self.max_us.max(us);
    }
}

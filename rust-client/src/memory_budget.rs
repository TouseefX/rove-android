//! A growing soft budget. Raising it does not reserve or touch unused RAM.

pub const DEFAULT_MEMORY_BYTES: u64 = 128_000_000;
pub const MEMORY_BUFFER_BYTES: u64 = 10_000_000;

impl Default for GrowingMemoryBudget {
    fn default() -> Self {
        Self {
            limit: DEFAULT_MEMORY_BYTES,
            buffer: MEMORY_BUFFER_BYTES,
        }
    }
}

/// Tracks headroom without rounding usage down or shrinking after collection.
#[derive(Clone, Copy, Debug)]
pub struct GrowingMemoryBudget {
    limit: u64,
    buffer: u64,
}

impl GrowingMemoryBudget {
    pub fn new(initial_bytes: u64, buffer_bytes: u64) -> Result<Self, &'static str> {
        if buffer_bytes == 0 || initial_bytes == 0 {
            return Err("memory budget and buffer must be positive");
        }
        Ok(Self {
            limit: initial_bytes,
            buffer: buffer_bytes,
        })
    }

    pub fn limit_bytes(&self) -> u64 {
        self.limit
    }

    pub fn buffer_bytes(&self) -> u64 {
        self.buffer
    }

    /// Read the whole current process (world, networking, VM and native data).
    /// An unavailable OS reading is never interpreted as zero usage.
    pub fn sample_process(&mut self) -> Option<u64> {
        let used = rove_client_core::memory::current_process_rss_bytes().ok()?;
        self.observe(used);
        Some(used)
    }

    /// At exactly one buffer of remaining space, grant another buffer. Large
    /// jumps catch up in one calculation instead of one increment per sample.
    pub fn observe(&mut self, used_bytes: u64) -> u64 {
        let needed = used_bytes.saturating_add(self.buffer);
        if needed >= self.limit {
            let steps = (needed - self.limit) / self.buffer;
            self.limit = self
                .limit
                .saturating_add(steps.saturating_add(1).saturating_mul(self.buffer));
        }
        self.limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const MB: u64 = 1_000_000;

    #[test]
    fn grows_at_the_buffer_boundary_and_keeps_the_granted_space() {
        let mut budget = GrowingMemoryBudget::default();
        assert_eq!(budget.buffer_bytes(), 10 * MB);
        assert_eq!(budget.limit_bytes(), 128 * MB);
        assert_eq!(budget.observe(118 * MB - 1), 128 * MB);
        assert_eq!(budget.observe(118 * MB), 138 * MB);
        assert_eq!(budget.observe(118 * MB), 138 * MB);
        assert_eq!(budget.observe(128 * MB), 148 * MB);
        assert_eq!(budget.observe(20 * MB), 148 * MB);
    }

    #[test]
    fn catches_large_jumps_without_overflow() {
        let mut budget = GrowingMemoryBudget::new(128 * MB, 10 * MB).unwrap();
        assert_eq!(budget.observe(200 * MB), 218 * MB);
        assert_eq!(budget.observe(u64::MAX), u64::MAX);
        assert_eq!(budget.observe(u64::MAX), u64::MAX);
    }

    #[test]
    fn rejects_an_empty_buffer_or_budget() {
        assert!(GrowingMemoryBudget::new(128, 0).is_err());
        assert!(GrowingMemoryBudget::new(0, 10).is_err());
    }
}

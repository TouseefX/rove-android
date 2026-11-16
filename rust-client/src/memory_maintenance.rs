//! Return wholly unused allocator pages between batches on their owner thread.
//! Live allocations and VM memory stay intact; this does not evict live pages.

#[cfg(feature = "allocator-mimalloc-native")]
pub(crate) fn between_batches() {
    use std::{cell::Cell, time::{Duration, Instant}};
    thread_local! { static LAST: Cell<Option<Instant>> = const { Cell::new(None) }; }
    let now = Instant::now();
    let due = LAST.with(|last| {
        if last.get().is_some_and(|before| now.duration_since(before) < Duration::from_millis(250)) {
            false
        } else {
            last.set(Some(now));
            true
        }
    });
    if due {
        #[link(name = "mimalloc", kind = "static")]
        unsafe extern "C" { fn mi_collect(force: bool); }
        // SAFETY: the pinned allocator exposes this no-argument heap operation.
        // It collects the calling thread's delayed frees and only unused pages.
        unsafe { mi_collect(false) };
    }
}

#[cfg(not(feature = "allocator-mimalloc-native"))]
pub(crate) fn between_batches() {}

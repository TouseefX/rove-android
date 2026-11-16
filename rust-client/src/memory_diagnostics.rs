//! Opt-in allocator accounting; never visits live payload contents.

pub(crate) fn heap(label: &str) {
    if std::env::var_os("ROVE_MEMORY_DIAGNOSTICS").is_none() { return; }
    #[cfg(feature = "allocator-mimalloc-native")]
    native::report(label);
    #[cfg(not(feature = "allocator-mimalloc-native"))]
    let _ = label;
}

#[cfg(feature = "allocator-mimalloc-native")]
mod native {
    use std::ffi::c_void;
    #[repr(C)]
    struct Area {
        blocks: *mut c_void,
        reserved: usize,
        committed: usize,
        used: usize,
        block_size: usize,
        full_block_size: usize,
        heap_tag: i32,
    }
    type Visitor = unsafe extern "C" fn(*const c_void, *const Area, *mut c_void, usize, *mut c_void) -> bool;
    // Utility binaries do not all install MiMalloc as their Rust allocator.
    // Link the same pinned native archive for these diagnostic entry points.
    #[link(name = "mimalloc", kind = "static")]
    unsafe extern "C" {
        fn mi_heap_get_default() -> *mut c_void;
        fn mi_heap_visit_blocks(heap: *const c_void, visit_blocks: bool, visitor: Visitor, arg: *mut c_void) -> bool;
    }
    #[derive(Clone, Copy, Default)]
    struct Row { size: usize, areas: usize, used: usize, committed: usize, reserved: usize }
    struct Rows { rows: [Row; 128], overflow: usize }
    unsafe extern "C" fn visit(_: *const c_void, area: *const Area, _: *mut c_void, _: usize, arg: *mut c_void) -> bool {
        // Called synchronously on the owning thread. No allocation, formatting,
        // or lock acquisition is allowed while the allocator is being visited.
        let (area, rows) = unsafe { (&*area, &mut *arg.cast::<Rows>()) };
        if let Some(row) = rows.rows.iter_mut().find(|r| r.areas == 0 || r.size == area.full_block_size) {
            row.size = area.full_block_size;
            row.areas += 1;
            row.used += area.used * area.full_block_size;
            row.committed += area.committed;
            row.reserved += area.reserved;
        } else { rows.overflow += 1; }
        true
    }
    pub(super) fn report(label: &str) {
        let mut rows = Rows { rows: [Row::default(); 128], overflow: 0 };
        // The default heap belongs to this thread; the callback borrows stack
        // storage only for this synchronous call.
        let complete = unsafe { mi_heap_visit_blocks(mi_heap_get_default(), false, visit, (&mut rows as *mut Rows).cast()) };
        let used = rows.rows.iter().map(|r| r.used).sum::<usize>();
        let committed = rows.rows.iter().map(|r| r.committed).sum::<usize>();
        let mut classes = rows.rows.iter().filter(|r| r.areas != 0).collect::<Vec<_>>();
        classes.sort_unstable_by_key(|r| std::cmp::Reverse(r.used));
        eprintln!("MEMORY_DIAG_HEAP {}", serde_json::json!({"label":label,"complete":complete,
            "overflow_areas":rows.overflow,"used_bytes":used,"committed_bytes":committed,
            "rss":rove_client_core::memory::current_process_rss_bytes().ok(),
            "size_classes":classes.iter().map(|r| serde_json::json!({"block":r.size,"areas":r.areas,"used":r.used,"committed":r.committed,"reserved":r.reserved})).collect::<Vec<_>>() }));
    }
}

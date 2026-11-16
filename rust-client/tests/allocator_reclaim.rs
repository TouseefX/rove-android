#![cfg(feature = "allocator-mimalloc")]

use std::alloc::{GlobalAlloc, Layout};

struct Block {
    pointer: *mut u8,
    layout: Layout,
    pattern: u8,
}

// A block has a single owner; it is never read while another thread frees it.
unsafe impl Send for Block {}

impl Block {
    fn new(size: usize, alignment: usize, pattern: u8) -> Self {
        let layout = Layout::from_size_align(size, alignment).unwrap();
        let pointer = unsafe { mimalloc::MiMalloc.alloc(layout) };
        assert!(!pointer.is_null());
        assert_eq!(pointer as usize % alignment, 0);
        unsafe { pointer.write_bytes(pattern, size) };
        Self { pointer, layout, pattern }
    }
    fn check(&self) {
        let bytes = unsafe { std::slice::from_raw_parts(self.pointer, self.layout.size()) };
        assert!(bytes.iter().all(|byte| *byte == self.pattern));
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        unsafe { mimalloc::MiMalloc.dealloc(self.pointer, self.layout) };
    }
}

fn collect_with(force: bool) {
    unsafe extern "C" { fn mi_collect(force: bool); }
    unsafe { mi_collect(force) };
}

fn collect() { collect_with(true); }

#[test]
fn reclaimed_pages_preserve_live_blocks_recommit_and_cross_thread_frees() {
    let threads = (0..4).map(|thread| std::thread::spawn(move || {
        let guards = (0..128).map(|i| Block::new(48 + i * 17, 16, (i % 251 + 1) as u8)).collect::<Vec<_>>();
        for round in 0..6 {
            let transient = (0..512).map(|i| {
                let size = [64, 512, 4096, 32768][i % 4];
                Block::new(size, 16, ((i + thread + round) % 251 + 1) as u8)
            }).collect::<Vec<_>>();
            for block in &transient { block.check(); }
            drop(transient);
            collect_with(false);
            for guard in &guards { guard.check(); }
            collect();
            for guard in &guards { guard.check(); }
            // Zeroed allocations must work after discarded pages are reused.
            let layout = Layout::from_size_align(128 * 1024, 4096).unwrap();
            let p = unsafe { mimalloc::MiMalloc.alloc_zeroed(layout) };
            assert!(!p.is_null());
            assert_eq!(p as usize % 4096, 0);
            assert!(unsafe { std::slice::from_raw_parts(p, layout.size()) }.iter().all(|v| *v == 0));
            unsafe { mimalloc::MiMalloc.dealloc(p, layout) };
        }
        guards
    })).collect::<Vec<_>>();
    for thread in threads {
        let guards = thread.join().unwrap();
        collect();
        for guard in &guards { guard.check(); }
        drop(guards);
        collect();
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64", feature = "allocator-mimalloc-native"))]
#[test]
fn small_native_pages_preserve_alignment_reallocation_and_large_blocks() {
    unsafe extern "C" {
        fn rove_mimalloc_small_page_bytes() -> usize;
        fn rove_mimalloc_medium_page_bytes() -> usize;
        fn rove_mimalloc_segment_bytes() -> usize;
    }
    assert_eq!(unsafe { rove_mimalloc_small_page_bytes() }, 16 * 1024);
    assert_eq!(unsafe { rove_mimalloc_medium_page_bytes() }, 64 * 1024);
    assert_eq!(unsafe { rove_mimalloc_segment_bytes() }, 8 * 1024 * 1024);
    for size in [1, 7, 24, 192, 640, 2047, 2048, 2049, 4096, 8191, 8192, 8193, 16383, 16384, 16385, 65536, 131072, 1048576, 4194304, 8388608] {
        for alignment in [8, 64, 4096, 16384] {
            let block = Block::new(size, alignment, 0xA7);
            block.check();
            collect();
            block.check();
            let new_size = size + size / 2 + 1;
            let pointer = unsafe { mimalloc::MiMalloc.realloc(block.pointer, block.layout, new_size) };
            assert!(!pointer.is_null());
            assert_eq!(pointer as usize % alignment, 0);
            assert!(unsafe { std::slice::from_raw_parts(pointer, size) }.iter().all(|v| *v == 0xA7));
            // realloc consumed the old allocation; transfer ownership exactly once.
            std::mem::forget(block);
            let expanded = Block { pointer, layout: Layout::from_size_align(new_size, alignment).unwrap(), pattern: 0x3D };
            unsafe { expanded.pointer.write_bytes(expanded.pattern, new_size) };
            expanded.check();
            drop(expanded);
            collect();
        }
    }
}

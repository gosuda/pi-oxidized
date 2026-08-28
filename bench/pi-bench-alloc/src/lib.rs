//! Counting global allocator for render-churn benchmarks.
//!
//! Wraps `std::alloc::System` and atomically counts bytes allocated and
//! deallocated.  The benchmark binary installs this as `#[global_allocator]`
//! and reads the counters before/after each scenario to measure allocation
//! churn (bytes allocated per frame), mirroring the V8 sampling heap profiler
//! approach used in the upstream TypeScript benchmark.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

static ALLOCATED: AtomicU64 = AtomicU64::new(0);
static DEALLOCATED: AtomicU64 = AtomicU64::new(0);

/// Counting allocator wrapper around `System`.
pub struct CountingAllocator;

/// Snapshot of allocation counters at a point in time.
#[derive(Debug, Clone, Copy, Default)]
pub struct AllocCounters {
    /// Total bytes allocated since process start.
    pub allocated: u64,
    /// Total bytes deallocated since process start.
    pub deallocated: u64,
}

impl CountingAllocator {
    /// Read the current allocation counters.
    #[must_use]
    pub fn read() -> AllocCounters {
        AllocCounters {
            allocated: ALLOCATED.load(Ordering::Relaxed),
            deallocated: DEALLOCATED.load(Ordering::Relaxed),
        }
    }

    /// Reset counters to zero.
    pub fn reset() {
        ALLOCATED.store(0, Ordering::Relaxed);
        DEALLOCATED.store(0, Ordering::Relaxed);
    }
}

impl AllocCounters {
    /// Bytes allocated between an earlier snapshot (`before`) and `self`.
    #[must_use]
    pub fn bytes_since(&self, before: AllocCounters) -> u64 {
        self.allocated.saturating_sub(before.allocated)
    }
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            ALLOCATED.fetch_add(
                u64::try_from(layout.size()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        DEALLOCATED.fetch_add(
            u64::try_from(layout.size()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            ALLOCATED.fetch_add(
                u64::try_from(layout.size()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            DEALLOCATED.fetch_add(
                u64::try_from(layout.size()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
            ALLOCATED.fetch_add(
                u64::try_from(new_size).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        }
        new_ptr
    }
}

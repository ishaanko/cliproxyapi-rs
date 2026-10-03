//! Counting `GlobalAlloc` wrapper used by `cpa-bench` to report allocations and bytes per request.
//!
//! One-line hook in a binary (wraps any allocator, e.g. `System` or `MiMalloc`):
//!
//! ```ignore
//! #[global_allocator]
//! static ALLOC: cpa_allocstats::Counting<std::alloc::System> = cpa_allocstats::Counting::new(std::alloc::System);
//! // first line of main():
//! cpa_allocstats::serve_from_env();
//! ```
//!
//! `serve_from_env` starts a thread that answers every connection on the abstract unix socket
//! named by `CPA_ALLOC_STATS_SOCK` (no file, so no path length limits) with one line `allocs=<n> frees=<n> bytes=<n> live=<n>` (a no-op when the
//! variable is unset). The harness reads it before and after a measured window and subtracts.

use std::alloc::{GlobalAlloc, Layout};
use std::cell::Cell;
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

/// Counter shards; threads are spread over them so hot allocation paths do not fight over one
/// cache line (which would distort the timing of the very thing being measured).
const SHARDS: usize = 32;

/// One shard of counters, padded to its own cache lines.
#[repr(align(128))]
struct Shard {
    allocs: AtomicUsize,
    frees: AtomicUsize,
    bytes: AtomicUsize,
    /// Bytes freed; live = bytes - freed_bytes.
    freed_bytes: AtomicUsize,
}

#[allow(clippy::declare_interior_mutable_const)]
const ZERO: Shard = Shard { allocs: AtomicUsize::new(0), frees: AtomicUsize::new(0), bytes: AtomicUsize::new(0), freed_bytes: AtomicUsize::new(0) };
static COUNTERS: [Shard; SHARDS] = [ZERO; SHARDS];
static NEXT_SLOT: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    // `const` init: no lazy allocation, so touching this inside the allocator is safe.
    static SLOT: Cell<usize> = const { Cell::new(usize::MAX) };
}

/// This thread's shard, assigned round-robin on first use.
fn shard() -> &'static Shard {
    let slot = SLOT.try_with(|s| {
        if s.get() == usize::MAX {
            s.set(NEXT_SLOT.fetch_add(1, Relaxed) % SHARDS);
        }
        s.get()
    });
    // A thread in TLS teardown falls back to shard 0.
    &COUNTERS[slot.unwrap_or(0)]
}

/// Allocation totals since process start.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// alloc, alloc_zeroed and realloc calls.
    pub allocs: usize,
    pub frees: usize,
    /// Bytes requested by allocs (a realloc counts its new size).
    pub bytes: usize,
    /// Bytes currently allocated.
    pub live: usize,
}

/// Sums the shards.
pub fn stats() -> Stats {
    let (mut allocs, mut frees, mut bytes, mut freed) = (0usize, 0usize, 0usize, 0usize);
    for s in &COUNTERS {
        allocs = allocs.wrapping_add(s.allocs.load(Relaxed));
        frees = frees.wrapping_add(s.frees.load(Relaxed));
        bytes = bytes.wrapping_add(s.bytes.load(Relaxed));
        freed = freed.wrapping_add(s.freed_bytes.load(Relaxed));
    }
    Stats { allocs, frees, bytes, live: bytes.wrapping_sub(freed) }
}

/// Wraps allocator `A`, counting every call.
pub struct Counting<A>(A);

impl<A> Counting<A> {
    pub const fn new(inner: A) -> Self {
        Counting(inner)
    }
}

// SAFETY: every method forwards to the wrapped allocator unchanged; the counters are only
// atomic adds on statics and never allocate.
unsafe impl<A: GlobalAlloc> GlobalAlloc for Counting<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { self.0.alloc(layout) };
        if !p.is_null() {
            let s = shard();
            s.allocs.fetch_add(1, Relaxed);
            s.bytes.fetch_add(layout.size(), Relaxed);
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { self.0.alloc_zeroed(layout) };
        if !p.is_null() {
            let s = shard();
            s.allocs.fetch_add(1, Relaxed);
            s.bytes.fetch_add(layout.size(), Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { self.0.dealloc(ptr, layout) };
        let s = shard();
        s.frees.fetch_add(1, Relaxed);
        s.freed_bytes.fetch_add(layout.size(), Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { self.0.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            let s = shard();
            s.allocs.fetch_add(1, Relaxed);
            s.frees.fetch_add(1, Relaxed);
            s.bytes.fetch_add(new_size, Relaxed);
            s.freed_bytes.fetch_add(layout.size(), Relaxed);
        }
        p
    }
}

/// Starts the stats thread when `CPA_ALLOC_STATS_SOCK` is set; call first thing in `main`.
pub fn serve_from_env() {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixListener};
    let Some(name) = std::env::var("CPA_ALLOC_STATS_SOCK").ok() else { return };
    let Ok(addr) = SocketAddr::from_abstract_name(name.as_bytes()) else { return };
    let Ok(listener) = UnixListener::bind_addr(&addr) else { return };
    let _ = std::thread::Builder::new().name("alloc-stats".into()).spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            let s = stats();
            let _ = writeln!(conn, "allocs={} frees={} bytes={} live={}", s.allocs, s.frees, s.bytes, s.live);
        }
    });
}

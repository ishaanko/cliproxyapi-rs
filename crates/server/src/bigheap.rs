//! Global allocator front end for large blocks (Linux).
//!
//! Request bodies and their copies are multi-megabyte `Vec`s that live for milliseconds while
//! worker threads allocate and free them concurrently. mimalloc keeps freed memory in per-thread
//! page/arena state for up to a second (`purge_delay`), so under load the resident set ended up
//! ~2x the live heap. Blocks above [`MIN_SIZE`] (8 KiB: request bodies, their copies and the
//! long strings of parsed trees) are instead carved from one reserved virtual
//! region in size classes (12.5% steps) with a shared free list per class:
//!
//! - a freed slot is reused by whichever thread asks next (most recent first, so it is still warm
//!   and resident), which keeps the footprint near the *global* peak instead of the sum of
//!   per-thread peaks;
//! - a background sweeper returns the pages of slots that stayed free for [`AGE_TICKS`] sweeps
//!   (50-100 ms) to the OS
//!   (`MADV_DONTNEED`; the address range stays reserved), so an idle process drops back.
//!
//! No `mmap`/`munmap` happens after startup, so no process-wide VM write lock is taken on the
//! hot path. Everything else (and any failure to reserve the region or exhaust it) goes to
//! mimalloc, and `dealloc` tells the two apart by address.
//!
//! Set `CPA_BIGHEAP=0` to bypass the front end.

use std::alloc::{GlobalAlloc, Layout};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Mutex, Once};
use std::thread;
use std::time::Duration;

/// Virtual address space reserved for slots (untouched pages cost nothing).
const REGION: usize = 16 << 30;
/// Requests larger than this (and at most [`MAX_SIZE`]) are served from the region.
const MIN_SIZE: usize = 8 * 1024;
const MAX_SIZE: usize = 1 << 31;
/// Free slots that sat unused for this many sweeps have their pages returned to the OS. A tick
/// counter instead of a clock: stamping a slot on every free must stay cheap.
const AGE_TICKS: u32 = 2;
/// Sweeper period while slots are waiting to age out, and while everything is clean.
const SWEEP_BUSY: Duration = Duration::from_millis(50);
const SWEEP_IDLE: Duration = Duration::from_millis(500);

/// Size classes: 8 per power of two, from 8 KiB up. Slot sizes are multiples of 4 KiB (so the
/// smallest classes have slack above 12.5%).
const FIRST_HB: usize = 13;
const NCLASS: usize = (31 - FIRST_HB) * 8;

/// Start of the region, 0 until initialised (and forever if reserving it failed).
static BASE: AtomicUsize = AtomicUsize::new(0);
/// Bump offset for slots that no free list could supply; slots are recycled, never returned.
static NEXT: AtomicUsize = AtomicUsize::new(0);
/// Free slots whose pages are still resident.
static DIRTY: AtomicUsize = AtomicUsize::new(0);
/// Advanced once per sweep.
static TICK: AtomicU32 = AtomicU32::new(0);
static INIT: Once = Once::new();

#[derive(Clone, Copy)]
struct Slot {
    off: usize,
    /// [`TICK`] when the slot was freed.
    freed: u32,
    /// Pages may still be resident (cleared by the sweeper).
    dirty: bool,
}

static FREE: [Mutex<Vec<Slot>>; NCLASS] = [const { Mutex::new(Vec::new()) }; NCLASS];

/// Class index and slot size for a request of `size` bytes (`size` > [`MIN_SIZE`]).
fn class_of(size: usize) -> (usize, usize) {
    let n = size - 1;
    let hb = (usize::BITS - 1 - n.leading_zeros()) as usize;
    let shift = hb - 3;
    let q = n >> shift; // 8..=15
    ((hb - FIRST_HB) * 8 + (q & 7), ((q + 1) << shift).next_multiple_of(4096))
}

/// Slot size of class `idx`.
fn class_size(idx: usize) -> usize {
    let hb = idx / 8 + FIRST_HB;
    ((idx % 8 + 9) << (hb - 3)).next_multiple_of(4096)
}

fn init() {
    INIT.call_once(|| {
        if std::env::var_os("CPA_BIGHEAP").is_some_and(|v| v == "0") {
            return;
        }
        // SAFETY: anonymous private mapping with no fixed address; the result is checked.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                REGION,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return;
        }
        if thread::Builder::new().name("bigheap-sweep".into()).spawn(sweep_loop).is_err() {
            // SAFETY: nothing was handed out from the region yet.
            unsafe { libc::munmap(p, REGION) };
            return;
        }
        BASE.store(p as usize, Ordering::Release);
    });
}

fn lock(idx: usize) -> std::sync::MutexGuard<'static, Vec<Slot>> {
    FREE[idx].lock().unwrap_or_else(|e| e.into_inner())
}

/// Returns the pages of the slot at region offset `off` to the OS.
fn release(off: usize, cap: usize) {
    let base = BASE.load(Ordering::Relaxed);
    // SAFETY: the slot is owned by the caller (taken off its free list) and lies inside the
    // region; MADV_DONTNEED on private anonymous memory only discards its contents.
    unsafe { libc::madvise((base + off) as *mut libc::c_void, cap, libc::MADV_DONTNEED) };
}

fn sweep_loop() {
    loop {
        thread::sleep(if DIRTY.load(Ordering::Relaxed) > 0 { SWEEP_BUSY } else { SWEEP_IDLE });
        let now = TICK.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        for idx in 0..NCLASS {
            let cap = class_size(idx);
            let mut old = Vec::new();
            {
                let mut free = lock(idx);
                let mut i = 0;
                while i < free.len() {
                    if free[i].dirty && now.wrapping_sub(free[i].freed) >= AGE_TICKS {
                        old.push(free.swap_remove(i));
                    } else {
                        i += 1;
                    }
                }
            }
            if old.is_empty() {
                continue;
            }
            // Off the free list while released, so no thread can be handed a slot mid-discard.
            for s in &mut old {
                release(s.off, cap);
                s.dirty = false;
            }
            DIRTY.fetch_sub(old.len() * cap, Ordering::Relaxed);
            lock(idx).extend(old);
        }
    }
}

fn in_region(ptr: *mut u8) -> bool {
    let base = BASE.load(Ordering::Relaxed);
    base != 0 && (ptr as usize).wrapping_sub(base) < REGION
}

fn eligible(layout: &Layout) -> bool {
    layout.size() > MIN_SIZE && layout.size() < MAX_SIZE && layout.align() <= 4096
}

/// A slot for `size` bytes, or null when the region is not available or full.
fn region_alloc(size: usize) -> *mut u8 {
    let base = BASE.load(Ordering::Acquire);
    if base == 0 {
        return std::ptr::null_mut();
    }
    let (idx, cap) = class_of(size);
    let off = match lock(idx).pop() {
        Some(s) => {
            if s.dirty {
                DIRTY.fetch_sub(cap, Ordering::Relaxed);
            }
            s.off
        }
        None => {
            let off = NEXT.fetch_add(cap, Ordering::Relaxed);
            if off + cap > REGION {
                return std::ptr::null_mut();
            }
            off
        }
    };
    (base + off) as *mut u8
}

fn region_free(ptr: *mut u8, size: usize) {
    let (idx, cap) = class_of(size);
    let off = ptr as usize - BASE.load(Ordering::Relaxed);
    DIRTY.fetch_add(cap, Ordering::Relaxed);
    lock(idx).push(Slot { off, freed: TICK.load(Ordering::Relaxed), dirty: true });
}

/// mimalloc, with large blocks served from the region.
pub struct Tiered<A>(pub A);

// SAFETY: blocks are either forwarded to the wrapped allocator or are disjoint slots of the
// reserved region, each handed out to one caller at a time and sized for the request (a slot is
// at least the class size for the layout it is allocated with, and `dealloc`/`realloc` recompute
// the class from the layout the caller must pass back unchanged).
unsafe impl<A: GlobalAlloc> GlobalAlloc for Tiered<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() > MIN_SIZE {
            if !INIT.is_completed() {
                init();
            }
            if eligible(&layout) {
                let p = region_alloc(layout.size());
                if !p.is_null() {
                    return p;
                }
            }
        }
        // SAFETY: same contract as ours.
        unsafe { self.0.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if layout.size() > MIN_SIZE && INIT.is_completed() && eligible(&layout) {
            let p = region_alloc(layout.size());
            if !p.is_null() {
                // Recycled slots hold old data.
                // SAFETY: `p` is valid for `layout.size()` bytes.
                unsafe { std::ptr::write_bytes(p, 0, layout.size()) };
                return p;
            }
        }
        // SAFETY: same contract as ours.
        unsafe { self.0.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // Region blocks always have a layout above MIN_SIZE, so small frees skip the address test.
        if layout.size() > MIN_SIZE && in_region(ptr) {
            region_free(ptr, layout.size());
        } else {
            // SAFETY: not ours, so it came from the wrapped allocator with this layout.
            unsafe { self.0.dealloc(ptr, layout) };
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller guarantees `new_size` is valid for `layout.align()`.
        if layout.size() <= MIN_SIZE && new_size <= MIN_SIZE {
            // SAFETY: same contract as ours.
            return unsafe { self.0.realloc(ptr, layout, new_size) };
        }
        let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        if layout.size() > MIN_SIZE && in_region(ptr) && eligible(&new_layout) && class_of(new_size).0 == class_of(layout.size()).0 {
            return ptr;
        }
        let new = unsafe { self.alloc(new_layout) };
        if !new.is_null() {
            // SAFETY: both blocks are valid for the copied length and do not overlap.
            unsafe {
                std::ptr::copy_nonoverlapping(ptr, new, layout.size().min(new_size));
                self.dealloc(ptr, layout);
            }
        }
        new
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::System;

    #[test]
    fn class_sizes_cover_requests_and_are_page_multiples() {
        for size in [MIN_SIZE + 1, 9_000, 20_000, 70_000, 100_000, 1 << 20, (2 << 20) + 5, 3_000_000, (1 << 30) + 1, MAX_SIZE - 1] {
            let (idx, cap) = class_of(size);
            assert!(cap >= size && cap - size <= size / 8 + 4096, "{size} -> {cap}");
            assert_eq!(cap % 4096, 0);
            assert_eq!(class_size(idx), cap);
            assert!(idx < NCLASS);
        }
    }

    #[test]
    fn blocks_keep_their_contents_across_realloc_and_reuse() {
        let a = Tiered(System);
        let layout = Layout::from_size_align(2_200_000, 8).unwrap();
        unsafe {
            let p = a.alloc(layout);
            assert!(!p.is_null() && in_region(p));
            std::ptr::write_bytes(p, 0xab, layout.size());
            // Same class: stays in place. Larger class: moves, contents follow.
            assert_eq!(a.realloc(p, layout, 2_250_000), p);
            let grown = a.realloc(p, Layout::from_size_align(2_250_000, 8).unwrap(), 9_000_000);
            assert!(in_region(grown));
            assert!(std::slice::from_raw_parts(grown, 2_200_000).iter().all(|&b| b == 0xab));
            // Shrinking below the threshold hands the block back to the wrapped allocator.
            let small = a.realloc(grown, Layout::from_size_align(9_000_000, 8).unwrap(), 1000);
            assert!(!in_region(small));
            assert!(std::slice::from_raw_parts(small, 1000).iter().all(|&b| b == 0xab));
            a.dealloc(small, Layout::from_size_align(1000, 8).unwrap());
            // A recycled slot is cleared when zeroed memory is requested.
            let q = a.alloc(layout);
            std::ptr::write_bytes(q, 0xcd, layout.size());
            a.dealloc(q, layout);
            let z = a.alloc_zeroed(layout);
            assert!(std::slice::from_raw_parts(z, layout.size()).iter().all(|&b| b == 0));
            a.dealloc(z, layout);
        }
    }
}

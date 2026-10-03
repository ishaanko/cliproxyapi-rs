//! Global allocator front end for large blocks (64-bit Linux).
//!
//! Request bodies and their copies are multi-megabyte `Vec`s that live for milliseconds while
//! worker threads allocate and free them concurrently. mimalloc keeps freed memory in per-thread
//! page/arena state and never hands it back while the process stays busy or goes idle, so under
//! load the resident set ended up ~2x the live heap. Blocks above [`MIN_SIZE`] (8 KiB: request
//! bodies, their copies and the long strings of parsed trees) are instead carved from one
//! reserved virtual region in size classes (12.5% steps) with a shared free list per class:
//!
//! - a freed slot is reused by whichever thread asks next (most recently freed first, so it is
//!   still warm and resident), which keeps the footprint near the *global* peak instead of the
//!   sum of per-thread peaks;
//! - a background sweeper returns the pages of slots that stayed free for [`AGE_TICKS`] sweeps
//!   to the OS (`MADV_DONTNEED`; the address range stays reserved): 50-100 ms while the process
//!   is busy, up to ~0.6 s after a quiet spell (the sweeper polls slowly when nothing is
//!   waiting). An idle process drops back.
//!
//! The allocator never allocates from itself: free lists are intrusive stacks over a side table
//! (two `u32`s per 4 KiB page of the region, mapped once at [`init`]), and every lock is held
//! only for a few loads and stores. The region is reserved `PROT_NONE` and made accessible in
//! [`COMMIT_CHUNK`] steps as the bump frontier advances, so strict-overcommit systems are charged
//! only for what is used. Everything else, requests made before [`init`] and any failure to
//! reserve or exhaust the region go to the wrapped allocator, and `dealloc` tells the two apart
//! by address.
//!
//! Set `CPA_BIGHEAP=0` to bypass the front end.

use std::alloc::{GlobalAlloc, Layout};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Mutex, Once};
use std::thread;
use std::time::Duration;

/// Virtual address space reserved for slots.
const REGION: usize = 16 << 30;
/// The region is made readable/writable this much at a time.
const COMMIT_CHUNK: usize = 64 << 20;
pub(crate) const PAGE: usize = 4096;
const NPAGES: usize = REGION / PAGE;
/// Requests larger than this (and smaller than [`MAX_SIZE`]) are served from the region.
pub(crate) const MIN_SIZE: usize = 8 * 1024;
pub(crate) const MAX_SIZE: usize = 1 << 31;
/// A free slot that sat unused for this many sweeps has its pages returned to the OS. A tick
/// counter instead of a clock: stamping a slot on every free must stay cheap.
const AGE_TICKS: u32 = 2;
/// Sweeper period while slots are waiting to age out, and while everything is clean.
const SWEEP_BUSY: Duration = Duration::from_millis(50);
const SWEEP_IDLE: Duration = Duration::from_millis(500);
/// End of a free list.
const NIL: u32 = u32::MAX;

/// Classes up to 32 KiB step by one page; above that 8 per power of two. Slot sizes are page
/// multiples.
const SMALL_CLASSES: usize = 6;
pub(crate) const NCLASS: usize = SMALL_CLASSES + (31 - 15) * 8;

/// Start of the region, 0 until [`init`] finished (and forever if it failed or was disabled).
static BASE: AtomicUsize = AtomicUsize::new(0);
/// Side table: `[next page; NPAGES]` then `[tick freed; NPAGES]`, indexed by a slot's first page.
static META: AtomicUsize = AtomicUsize::new(0);
/// Bump offset for slots no free list could supply; slots are recycled, never returned.
static BUMP: AtomicUsize = AtomicUsize::new(0);
/// Bytes of the region made accessible so far.
static COMMITTED: AtomicUsize = AtomicUsize::new(0);
static GROW: Mutex<()> = Mutex::new(());
/// Free slots whose pages may still be resident.
static WARM_SLOTS: AtomicUsize = AtomicUsize::new(0);
/// Advanced once per sweep.
static TICK: AtomicU32 = AtomicU32::new(0);
static INIT: Once = Once::new();

/// Heads (page indexes) of a class's free stacks.
struct Lists {
    /// Recently freed slots, pages possibly resident; most recent first.
    warm: u32,
    /// Slots the sweeper already released: pages are zero and not resident.
    cold: u32,
}

static FREE: [Mutex<Lists>; NCLASS] = [const { Mutex::new(Lists { warm: NIL, cold: NIL }) }; NCLASS];

/// Slot size and class index for a request of `size` bytes (`MIN_SIZE` < `size` < `MAX_SIZE`).
pub(crate) fn class_of(size: usize) -> (usize, usize) {
    let n = size - 1;
    let hb = (usize::BITS - 1 - n.leading_zeros()) as usize;
    let shift = hb - 3;
    let cap = (((n >> shift) + 1) << shift).next_multiple_of(PAGE);
    (class_index(cap), cap)
}

/// Class of a slot size produced by [`class_of`] (so sizes that round to the same slot share it).
pub(crate) fn class_index(cap: usize) -> usize {
    if cap <= 32 * 1024 {
        return cap / PAGE - 3;
    }
    let n = cap - 1;
    let hb = (usize::BITS - 1 - n.leading_zeros()) as usize;
    SMALL_CLASSES + (hb - 15) * 8 + ((n >> (hb - 3)) & 7)
}

/// Slot size of class `idx`.
pub(crate) fn class_size(idx: usize) -> usize {
    if idx < SMALL_CLASSES {
        return (idx + 3) * PAGE;
    }
    let j = idx - SMALL_CLASSES;
    ((j % 8 + 9) << (15 + j / 8 - 3)).next_multiple_of(PAGE)
}

/// Reserves the region and starts the sweeper. Call first thing in `main`: allocations made
/// before it finishes (and from other threads while it runs) are served by the wrapped
/// allocator. Does nothing when `CPA_BIGHEAP=0` or when the address space cannot be reserved.
pub fn init() {
    INIT.call_once(|| {
        if std::env::var_os("CPA_BIGHEAP").is_some_and(|v| v == "0") {
            return;
        }
        // SAFETY: anonymous private mappings at kernel-chosen addresses; results are checked.
        let (region, meta) = unsafe {
            let region = libc::mmap(std::ptr::null_mut(), REGION, libc::PROT_NONE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE, -1, 0);
            let meta = libc::mmap(
                std::ptr::null_mut(),
                NPAGES * 8,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            );
            (region, meta)
        };
        let undo = |region: *mut libc::c_void, meta: *mut libc::c_void| {
            // SAFETY: unmapping what this function mapped; nothing was handed out from it yet.
            unsafe {
                if region != libc::MAP_FAILED {
                    libc::munmap(region, REGION);
                }
                if meta != libc::MAP_FAILED {
                    libc::munmap(meta, NPAGES * 8);
                }
            }
        };
        if region == libc::MAP_FAILED || meta == libc::MAP_FAILED {
            undo(region, meta);
            return;
        }
        // Transparent huge pages would make a slot's resident size 2 MiB granular and split on
        // every release.
        // SAFETY: advice on the mapping created above.
        unsafe { libc::madvise(region, REGION, libc::MADV_NOHUGEPAGE) };
        META.store(meta as usize, Ordering::Release);
        if thread::Builder::new().name("bigheap-sweep".into()).spawn(sweep_loop).is_err() {
            META.store(0, Ordering::Release);
            undo(region, meta);
            return;
        }
        BASE.store(region as usize, Ordering::Release);
    });
}

fn base() -> usize {
    BASE.load(Ordering::Acquire)
}

fn meta() -> &'static [AtomicU32] {
    // SAFETY: `META` points to a live `2 * NPAGES` u32 mapping (zero-filled, which is a valid
    // `AtomicU32`) once it is non-zero, and the mapping is never unmapped after `BASE` is set.
    unsafe { std::slice::from_raw_parts(META.load(Ordering::Acquire) as *const AtomicU32, 2 * NPAGES) }
}

fn lock(idx: usize) -> std::sync::MutexGuard<'static, Lists> {
    FREE[idx].lock().unwrap_or_else(|e| e.into_inner())
}

/// Makes the region accessible up to `end`; false if the kernel refuses.
fn ensure_committed(base: usize, end: usize) -> bool {
    if COMMITTED.load(Ordering::Acquire) >= end {
        return true;
    }
    let _grow = GROW.lock().unwrap_or_else(|e| e.into_inner());
    let done = COMMITTED.load(Ordering::Relaxed);
    if done >= end {
        return true;
    }
    let new = end.next_multiple_of(COMMIT_CHUNK).min(REGION);
    // SAFETY: the range lies inside the reserved region.
    let ok = unsafe { libc::mprotect((base + done) as *mut libc::c_void, new - done, libc::PROT_READ | libc::PROT_WRITE) } == 0;
    if ok {
        COMMITTED.store(new, Ordering::Release);
    }
    ok
}

/// Returns the pages of every slot on the detached warm chain starting at `head` to the OS and
/// moves the released ones to the cold stack of class `idx`. A slot whose pages the kernel
/// refuses to drop (`madvise` fails on locked memory) is not zero, so it goes back on the warm
/// stack with a fresh stamp and is retried after another [`AGE_TICKS`].
fn release_chain(base: usize, idx: usize, head: u32, now: u32) {
    let cap = class_size(idx);
    let meta = meta();
    // Chains built in place through the side table: released (cold) and refused (warm).
    let (mut cold_head, mut cold_tail, mut released) = (NIL, NIL, 0usize);
    let (mut warm_head, mut warm_tail) = (NIL, NIL);
    let mut page = head;
    while page != NIL {
        let next = meta[page as usize].load(Ordering::Relaxed);
        // SAFETY: the slot belongs to the detached chain, so nobody else touches it.
        let ok = unsafe { libc::madvise((base + page as usize * PAGE) as *mut libc::c_void, cap, libc::MADV_DONTNEED) } == 0;
        let (chain_head, chain_tail) = if ok { (&mut cold_head, &mut cold_tail) } else { (&mut warm_head, &mut warm_tail) };
        if ok {
            released += 1;
        } else {
            meta[NPAGES + page as usize].store(now, Ordering::Relaxed);
        }
        meta[page as usize].store(*chain_head, Ordering::Relaxed);
        if *chain_head == NIL {
            *chain_tail = page;
        }
        *chain_head = page;
        page = next;
    }
    let mut lists = lock(idx);
    if cold_head != NIL {
        meta[cold_tail as usize].store(lists.cold, Ordering::Relaxed);
        lists.cold = cold_head;
    }
    if warm_head != NIL {
        meta[warm_tail as usize].store(lists.warm, Ordering::Relaxed);
        lists.warm = warm_head;
    }
    drop(lists);
    WARM_SLOTS.fetch_sub(released, Ordering::Relaxed);
}

fn sweep_loop() {
    loop {
        thread::sleep(if WARM_SLOTS.load(Ordering::Relaxed) > 0 { SWEEP_BUSY } else { SWEEP_IDLE });
        let now = TICK.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        let base = base();
        if base == 0 {
            continue;
        }
        let meta = meta();
        for idx in 0..NCLASS {
            // The warm stack is ordered by free time, so the old slots are a suffix: cut it at
            // the first old one and take the rest.
            let old = {
                let mut lists = lock(idx);
                let (mut prev, mut cur) = (NIL, lists.warm);
                while cur != NIL && now.wrapping_sub(meta[NPAGES + cur as usize].load(Ordering::Relaxed)) < AGE_TICKS {
                    prev = cur;
                    cur = meta[cur as usize].load(Ordering::Relaxed);
                }
                if cur != NIL {
                    if prev == NIL {
                        lists.warm = NIL;
                    } else {
                        meta[prev as usize].store(NIL, Ordering::Relaxed);
                    }
                }
                cur
            };
            if old != NIL {
                release_chain(base, idx, old, now);
            }
        }
    }
}

pub(crate) fn in_region(ptr: *mut u8) -> bool {
    let base = base();
    base != 0 && (ptr as usize).wrapping_sub(base) < REGION
}

fn eligible(layout: &Layout) -> bool {
    layout.size() > MIN_SIZE && layout.size() < MAX_SIZE && layout.align() <= PAGE
}

/// A slot for `size` bytes and whether its contents may be non-zero, or `None` when the region
/// is not available or full.
fn region_alloc(size: usize) -> Option<(*mut u8, bool)> {
    let base = base();
    if base == 0 {
        return None;
    }
    let (idx, cap) = class_of(size);
    let meta = meta();
    {
        let mut lists = lock(idx);
        if lists.warm != NIL {
            let page = lists.warm;
            lists.warm = meta[page as usize].load(Ordering::Relaxed);
            drop(lists);
            WARM_SLOTS.fetch_sub(1, Ordering::Relaxed);
            return Some(((base + page as usize * PAGE) as *mut u8, true));
        }
        if lists.cold != NIL {
            let page = lists.cold;
            lists.cold = meta[page as usize].load(Ordering::Relaxed);
            return Some(((base + page as usize * PAGE) as *mut u8, false));
        }
    }
    let off = BUMP.fetch_add(cap, Ordering::Relaxed);
    if off + cap > REGION || !ensure_committed(base, off + cap) {
        return None;
    }
    Some(((base + off) as *mut u8, false))
}

fn region_free(ptr: *mut u8, size: usize) {
    let (idx, _) = class_of(size);
    let page = (ptr as usize - base()) / PAGE;
    let meta = meta();
    meta[NPAGES + page].store(TICK.load(Ordering::Relaxed), Ordering::Relaxed);
    WARM_SLOTS.fetch_add(1, Ordering::Relaxed);
    let mut lists = lock(idx);
    meta[page].store(lists.warm, Ordering::Relaxed);
    lists.warm = page as u32;
}

/// The wrapped allocator, with large blocks served from the region.
pub struct Tiered<A>(pub A);

// SAFETY: blocks are either forwarded to the wrapped allocator or are disjoint slots of the
// reserved region, each handed out to one caller at a time and sized for the request (a slot is
// at least the class size for the layout it is allocated with, and `dealloc`/`realloc` recompute
// the class from the layout the caller must pass back unchanged). No method allocates from
// `Tiered` itself.
unsafe impl<A: GlobalAlloc> GlobalAlloc for Tiered<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if eligible(&layout)
            && let Some((p, _)) = region_alloc(layout.size())
        {
            return p;
        }
        // SAFETY: same contract as ours.
        unsafe { self.0.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if eligible(&layout)
            && let Some((p, dirty)) = region_alloc(layout.size())
        {
            if dirty {
                // SAFETY: `p` is valid for `layout.size()` bytes.
                unsafe { std::ptr::write_bytes(p, 0, layout.size()) };
            }
            return p;
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
        if layout.size() <= MIN_SIZE && new_size <= MIN_SIZE {
            // SAFETY: both sizes are below the threshold, so `ptr` is the wrapped allocator's.
            return unsafe { self.0.realloc(ptr, layout, new_size) };
        }
        // SAFETY: the caller guarantees `new_size` is valid for `layout.align()`.
        let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        if layout.size() > MIN_SIZE && in_region(ptr) && eligible(&new_layout) && class_of(new_size).0 == class_of(layout.size()).0 {
            return ptr;
        }
        // SAFETY: `new_layout` is non-zero sized (above the threshold or `new_size` of a live block).
        let new = unsafe { self.alloc(new_layout) };
        if !new.is_null() {
            // SAFETY: `ptr` is valid for `layout.size()` bytes, `new` for `new_size`, and they are
            // distinct live blocks; `ptr` was allocated by `self` with `layout`.
            unsafe {
                std::ptr::copy_nonoverlapping(ptr, new, layout.size().min(new_size));
                self.dealloc(ptr, layout);
            }
        }
        new
    }
}

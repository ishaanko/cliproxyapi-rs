//! `bigheap` installed as the global allocator of a test binary: the free lists must never need
//! the allocator themselves (an earlier version deadlocked when a free list grew), concurrent
//! threads and the sweeper must keep every block intact, and recycled blocks must come back
//! zeroed when zeroed memory is requested. The module is included by path, so its items are
//! tested here rather than in the module (a `#[cfg(test)]` module there would run twice).
#![cfg(all(target_os = "linux", target_pointer_width = "64"))]

#[allow(dead_code)]
#[path = "../src/bigheap.rs"]
mod bigheap;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::mpsc;
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use bigheap::{MAX_SIZE, MIN_SIZE, NCLASS, PAGE, Tiered, class_index, class_of, class_size, in_region};

#[global_allocator]
static ALLOC: Tiered<System> = Tiered(System);

/// The tests share one region and its sweeper; running them one at a time keeps the slot
/// reuse assertions deterministic.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// Runs `f` on its own thread and fails instead of hanging if it does not finish.
fn finishes(f: impl FnOnce() + Send + 'static) {
    bigheap::init();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        f();
        let _ = tx.send(());
    });
    rx.recv_timeout(Duration::from_secs(120)).expect("allocator deadlocked or stalled");
}

/// Region slots are page aligned; the system allocator's large blocks are not.
fn from_region(v: &[u8]) -> bool {
    v.as_ptr() as usize % 4096 == 0
}

#[test]
fn classes_round_trip_and_cover_requests() {
    for size in [MIN_SIZE + 1, 9_000, 12_289, 16_385, 20_000, 32_768, 32_769, 70_000, 100_000, 1 << 20, (2 << 20) + 5, 3_000_000, (1 << 30) + 1, MAX_SIZE - 1] {
        let (idx, cap) = class_of(size);
        assert!(cap >= size && cap - size <= size / 8 + PAGE, "{size} -> {cap}");
        assert_eq!(cap % PAGE, 0);
        assert!(idx < NCLASS);
        assert_eq!(class_size(idx), cap, "{size}");
        assert_eq!(class_index(cap), idx);
        // Sizes that round to one slot share its class.
        assert_eq!(class_of(cap).0, idx);
    }
    let all: std::collections::HashSet<usize> = (0..NCLASS).map(class_size).collect();
    assert_eq!(all.len(), NCLASS, "class sizes are distinct");
}

#[test]
fn blocks_keep_their_contents_across_realloc_and_reuse() {
    let _serial = serial();
    bigheap::init();
    let a = Tiered(System);
    let layout = Layout::from_size_align(2_200_000, 8).unwrap();
    // SAFETY: layouts are non-zero and passed back unchanged.
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

#[test]
fn a_slot_the_kernel_will_not_release_is_never_treated_as_zeroed() {
    let _serial = serial();
    finishes(|| {
        // 16000 bytes is a 16 KiB slot.
        let mut v = vec![0u8; 16_000];
        let slot = v.as_mut_ptr();
        assert!(in_region(slot));
        // SAFETY: the range is this block's slot. Locked pages make MADV_DONTNEED fail with EINVAL.
        if unsafe { libc::mlock(slot as *const libc::c_void, 16_384) } != 0 {
            eprintln!("mlock refused (RLIMIT_MEMLOCK); skipping");
            return;
        }
        v.fill(0xee);
        drop(v);
        // Several sweeps: the sweeper tries to release the slot and must put it back as dirty.
        thread::sleep(Duration::from_millis(1200));
        let blocks: Vec<Vec<u8>> = (0..64).map(|_| vec![0u8; 16_000]).collect();
        assert!(blocks.iter().any(|b| b.as_ptr() == slot as *const u8), "the locked slot was not reused");
        assert!(blocks.iter().all(|b| b.iter().all(|&x| x == 0)), "zeroed allocation returned dirty memory");
        drop(blocks);
        // SAFETY: undoing the lock above.
        unsafe { libc::munlock(slot as *const libc::c_void, 16_384) };
    });
}

#[test]
fn freeing_thousands_of_blocks_does_not_deadlock() {
    let _serial = serial();
    finishes(|| {
        for size in [16_000usize, 12_000, 40_000, 300_000] {
            let blocks: Vec<Vec<u8>> = (0..5000).map(|i| vec![(i % 251) as u8; size]).collect();
            assert!(blocks.iter().all(|b| from_region(b)));
            assert!(blocks.iter().enumerate().all(|(i, b)| b[0] == (i % 251) as u8 && b[size - 1] == (i % 251) as u8));
            drop(blocks);
        }
    });
}

#[test]
fn concurrent_mixed_sizes_with_sweeper_keep_blocks_intact() {
    let _serial = serial();
    finishes(|| {
        let workers: Vec<_> = (0..6u8)
            .map(|t| {
                thread::spawn(move || {
                    let mut seed = 0x9E37_79B9u32.wrapping_mul(u32::from(t) + 1);
                    let mut next = move || {
                        seed ^= seed << 13;
                        seed ^= seed >> 17;
                        seed ^= seed << 5;
                        seed
                    };
                    let mut live: Vec<(Vec<u8>, u8)> = Vec::new();
                    for round in 0..4000u32 {
                        let size = 9_000 + (next() as usize % 3_000_000) / (1 + (next() as usize % 40));
                        let tag = (round % 250) as u8 + 1;
                        let mut v = vec![0u8; size];
                        assert!(v.iter().all(|&b| b == 0), "zeroed block was dirty");
                        v.fill(tag);
                        // Growth and shrink paths.
                        if round % 7 == 0 {
                            v.extend(std::iter::repeat_n(tag, size / 3 + 1));
                        }
                        if round % 11 == 0 {
                            v.truncate(size / 2 + 1);
                            v.shrink_to_fit();
                        }
                        live.push((v, tag));
                        if live.len() > 24 {
                            let (old, tag) = live.swap_remove(next() as usize % live.len());
                            assert!(old.iter().all(|&b| b == tag), "block was overwritten");
                        }
                        // Let the sweeper run and release cold slots mid-test.
                        if round % 1000 == 999 {
                            thread::sleep(Duration::from_millis(250));
                        }
                    }
                    for (v, tag) in live {
                        assert!(v.iter().all(|&b| b == tag));
                    }
                })
            })
            .collect();
        for w in workers {
            w.join().unwrap();
        }
    });
}

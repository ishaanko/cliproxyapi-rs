//! `bigheap` installed as the global allocator of a test binary: the free lists must never need
//! the allocator themselves (an earlier version deadlocked when a free list grew), concurrent
//! threads and the sweeper must keep every block intact, and recycled blocks must come back
//! zeroed when zeroed memory is requested.
#![cfg(all(target_os = "linux", target_pointer_width = "64"))]

#[allow(dead_code)]
#[path = "../src/bigheap.rs"]
mod bigheap;

use std::alloc::System;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

#[global_allocator]
static ALLOC: bigheap::Tiered<System> = bigheap::Tiered(System);

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
fn freeing_thousands_of_blocks_does_not_deadlock() {
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

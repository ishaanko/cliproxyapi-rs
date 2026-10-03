//! Periodically flushes LLVM PGO counters so a SIGKILLed server still leaves a profile (used by
//! `tools/pgo.sh`). Only linked in `-Cprofile-generate` builds with feature `pgo-dump`.
unsafe extern "C" {
    fn __llvm_profile_write_file() -> i32;
}

pub fn start() {
    std::thread::spawn(|| {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            // SAFETY: the profiling runtime's own flush entry point; safe to call at any time.
            unsafe {
                __llvm_profile_write_file();
            }
        }
    });
}

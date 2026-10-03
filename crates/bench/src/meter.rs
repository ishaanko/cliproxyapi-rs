//! Load-insensitive per-process counters for the server under test: user-space instructions and
//! task-clock via `perf_event_open`, `/proc` CPU time and context switches, and optional allocator
//! totals (Rust server built with the `alloc-stats` hook).
//!
//! The perf counters are attached before the server starts (`Proc::spawn_gated`) with `inherit`,
//! so every thread the server ever creates is included. They count only user space (`exclude_kernel`), which is all
//! `perf_event_paranoid=2` allows for unprivileged processes (which also rules out the
//! context-switch event, so that comes from `/proc/<pid>/task/*/status`); syscall cost shows up
//! in the `/proc` CPU time instead. Any counter the kernel refuses (no PMU in a VM, no permission) is
//! simply `None` and the report prints `-`.

use std::io::Read;
use std::os::unix::net::UnixStream;

use perf_event::events::{Hardware, Software};
use perf_event::{Builder, Counter};

/// Counters attached to one server pid.
pub struct Meter {
    pid: u32,
    instr: Option<Counter>,
    task: Option<Counter>,
    /// Abstract unix socket name of the allocator stats thread (see `cpa-allocstats`), when the server has it.
    alloc_sock: Option<String>,
}

fn open(event: impl perf_event::events::Event, pid: u32) -> Option<Counter> {
    Builder::new(event)
        .observe_pid(i32::try_from(pid).ok()?)
        .any_cpu()
        .inherit(true)
        .exclude_kernel(true)
        .exclude_hv(true)
        .enabled(true)
        .build()
        .ok()
}

/// A raw reading of every counter; subtract two with [`Sample::since`].
#[derive(Clone, Copy, Debug, Default)]
pub struct Sample {
    pub cpu_s: Option<f64>,
    pub instr: Option<u64>,
    pub ctxsw: Option<u64>,
    pub task_ns: Option<u64>,
    pub allocs: Option<u64>,
    pub alloc_bytes: Option<u64>,
}

/// Counter deltas over a window, divided by the requests completed in it.
#[derive(Clone, Copy, Debug, Default)]
pub struct PerRequest {
    /// Server CPU microseconds per request from `/proc` (10 ms ticks) ...
    pub cpu_us: Option<f64>,
    /// ... and from the task-clock counter (ns resolution) when available.
    pub task_us: Option<f64>,
    pub instr: Option<f64>,
    pub ctxsw: Option<f64>,
    pub allocs: Option<f64>,
    pub alloc_bytes: Option<f64>,
}

impl Meter {
    pub fn attach(pid: u32, alloc_sock: Option<String>) -> Self {
        Meter {
            pid,
            instr: open(Hardware::INSTRUCTIONS, pid),
            task: open(Software::TASK_CLOCK, pid),
            alloc_sock,
        }
    }

    /// Whether hardware instruction counting is working (false on VMs without a PMU).
    pub fn has_instructions(&self) -> bool {
        self.instr.is_some()
    }

    pub fn sample(&mut self) -> Sample {
        let rd = |c: &mut Option<Counter>| c.as_mut().and_then(|c| c.read().ok());
        let (allocs, alloc_bytes) = match self.alloc_sock.as_deref().and_then(read_alloc_stats) {
            Some((a, b)) => (Some(a), Some(b)),
            None => (None, None),
        };
        Sample {
            cpu_s: crate::procs::cpu_seconds(self.pid),
            instr: rd(&mut self.instr),
            ctxsw: ctxsw_total(self.pid),
            task_ns: rd(&mut self.task),
            allocs,
            alloc_bytes,
        }
    }
}

/// Voluntary plus involuntary context switches summed over the process's live threads.
fn ctxsw_total(pid: u32) -> Option<u64> {
    let mut total = 0u64;
    for task in std::fs::read_dir(format!("/proc/{pid}/task")).ok()?.flatten() {
        // A thread may exit between listing and reading.
        let Ok(status) = std::fs::read_to_string(task.path().join("status")) else { continue };
        for line in status.lines() {
            if let Some(v) = line.strip_prefix("voluntary_ctxt_switches:").or_else(|| line.strip_prefix("nonvoluntary_ctxt_switches:")) {
                total += v.trim().parse::<u64>().unwrap_or(0);
            }
        }
    }
    Some(total)
}

/// Reads `allocs=.. bytes=..` from the server's allocator stats socket.
fn read_alloc_stats(name: &str) -> Option<(u64, u64)> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::SocketAddr;
    let mut s = String::new();
    let addr = SocketAddr::from_abstract_name(name.as_bytes()).ok()?;
    UnixStream::connect_addr(&addr).ok()?.read_to_string(&mut s).ok()?;
    let field = |k: &str| s.split_whitespace().find_map(|w| w.strip_prefix(k)?.parse::<u64>().ok());
    Some((field("allocs=")?, field("bytes=")?))
}

impl Sample {
    /// Per-request deltas between `start` and `self` for `requests` completed requests.
    pub fn since(&self, start: &Sample, requests: u64) -> PerRequest {
        if requests == 0 {
            return PerRequest::default();
        }
        let n = requests as f64;
        let d = |a: Option<u64>, b: Option<u64>| Some(a?.saturating_sub(b?) as f64 / n);
        PerRequest {
            cpu_us: self.cpu_s.zip(start.cpu_s).map(|(b, a)| (b - a) * 1e6 / n),
            task_us: d(self.task_ns, start.task_ns).map(|v| v / 1e3),
            instr: d(self.instr, start.instr),
            ctxsw: d(self.ctxsw, start.ctxsw),
            allocs: d(self.allocs, start.allocs),
            alloc_bytes: d(self.alloc_bytes, start.alloc_bytes),
        }
    }
}

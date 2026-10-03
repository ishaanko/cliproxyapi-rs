//! In-process log feed for standalone mode (Go: internal/tui/loghook.go). The server's logger
//! pushes formatted lines; the Logs tab drains them. The queue is bounded and drops the oldest
//! line when full so logging never blocks.

use std::collections::VecDeque;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::Notify;

#[derive(Clone)]
pub struct LogHook {
    inner: Arc<Inner>,
}

struct Inner {
    queue: Mutex<VecDeque<String>>,
    cap: usize,
    notify: Notify,
}

impl LogHook {
    /// `NewLogHook`: a hook buffering up to `buf_size` lines.
    pub fn new(buf_size: usize) -> Self {
        LogHook {
            inner: Arc::new(Inner {
                queue: Mutex::new(VecDeque::with_capacity(buf_size.min(4096))),
                cap: buf_size.max(1),
                notify: Notify::new(),
            }),
        }
    }

    /// `Fire`: queues a line (trailing CR/LF trimmed), evicting the oldest when full.
    pub fn push(&self, line: &str) {
        let line = line.trim_end_matches(['\n', '\r']).to_string();
        {
            let mut q = self.inner.queue.lock();
            if q.len() >= self.inner.cap {
                q.pop_front();
            }
            q.push_back(line);
        }
        self.inner.notify.notify_one();
    }

    /// Waits for the next line.
    pub async fn recv(&self) -> String {
        loop {
            if let Some(line) = self.inner.queue.lock().pop_front() {
                return line;
            }
            self.inner.notify.notified().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn drops_oldest_when_full() {
        let hook = LogHook::new(2);
        hook.push("a\n");
        hook.push("b");
        hook.push("c\r\n");
        assert_eq!(hook.recv().await, "b");
        assert_eq!(hook.recv().await, "c");
    }
}

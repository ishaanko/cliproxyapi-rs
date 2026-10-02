//! Bridge from the synchronous `Store` / `CooldownStateStore` traits to async clients.
//!
//! The traits are called from `spawn_blocking` threads but also directly from async contexts (CLI
//! logins), where `Runtime::block_on` would panic. Futures therefore run on a private runtime and
//! the caller blocks on a std channel, which is safe from any thread.

use std::future::Future;
use std::sync::OnceLock;
use std::sync::mpsc;

use tokio::runtime::Runtime;

fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("cpa-store")
            .enable_all()
            .build()
            .expect("cpa-store: failed to start the store runtime")
    })
}

/// Runs `fut` on the store runtime and blocks the calling thread until it finishes.
pub(crate) fn block_on<T: Send + 'static>(fut: impl Future<Output = T> + Send + 'static) -> T {
    let (tx, rx) = mpsc::sync_channel(1);
    runtime().spawn(async move {
        let _ = tx.send(fut.await);
    });
    // The sender only drops unsent if the task panicked; surface that as a panic here too.
    rx.recv().expect("cpa-store: store task panicked")
}

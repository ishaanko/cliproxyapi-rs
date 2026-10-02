//! Keyed single-flight for token refresh (Go `singleflight.Group` + `context.WithoutCancel`):
//! concurrent callers with the same key share one in-flight call, and the call keeps running even
//! if the caller that started it goes away.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::OnceCell;

use crate::error::AuthFlowError;

type Flight<T> = Arc<OnceCell<Result<T, AuthFlowError>>>;

pub struct SingleFlight<T: Clone + Send + Sync + 'static> {
    inflight: Mutex<HashMap<String, Flight<T>>>,
}

impl<T: Clone + Send + Sync + 'static> Default for SingleFlight<T> {
    fn default() -> Self {
        Self {
            inflight: Mutex::new(HashMap::new()),
        }
    }
}

impl<T: Clone + Send + Sync + 'static> SingleFlight<T> {
    /// Runs `f` once per concurrent `key`; every waiter gets a clone of the result. `f` runs on its
    /// own task so cancelling the awaiting caller does not abort the shared work.
    pub async fn run<F, Fut>(&self, key: &str, f: F) -> Result<T, AuthFlowError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, AuthFlowError>> + Send + 'static,
    {
        let cell = {
            let mut map = self.inflight.lock();
            map.entry(key.to_string())
                .or_insert_with(|| Arc::new(OnceCell::new()))
                .clone()
        };
        let result = cell
            .get_or_init(|| async move {
                match tokio::spawn(f()).await {
                    Ok(r) => r,
                    Err(e) => Err(AuthFlowError::other(format!(
                        "single-flight task failed: {e}"
                    ))),
                }
            })
            .await
            .clone();
        let mut map = self.inflight.lock();
        if map.get(key).is_some_and(|c| Arc::ptr_eq(c, &cell)) {
            map.remove(key);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn concurrent_callers_share_one_call() {
        let sf = Arc::new(SingleFlight::<usize>::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..5 {
            let (sf, calls) = (sf.clone(), calls.clone());
            handles.push(tokio::spawn(async move {
                sf.run("k", move || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Ok(7)
                })
                .await
            }));
        }
        for h in handles {
            assert_eq!(h.await.unwrap().unwrap(), 7);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // A later call starts a fresh flight.
        let again = sf.run("k", || async { Ok(8) }).await.unwrap();
        assert_eq!(again, 8);
    }
}

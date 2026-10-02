//! Keyed single-flight for token refresh (Go `singleflight.Group` + `context.WithoutCancel`):
//! concurrent callers with the same key share one in-flight call. The call is spawned once and
//! publishes its result through a watch channel, so it completes and every waiter (including ones
//! that arrive after the initiator was dropped) gets the result without a second call.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::watch;

use crate::error::AuthFlowError;

type Outcome<T> = Option<Result<T, AuthFlowError>>;

struct Flight<T> {
    id: u64,
    rx: watch::Receiver<Outcome<T>>,
}

struct Inner<T> {
    next_id: u64,
    flights: HashMap<String, Flight<T>>,
}

pub struct SingleFlight<T: Clone + Send + Sync + 'static> {
    inner: Arc<Mutex<Inner<T>>>,
}

impl<T: Clone + Send + Sync + 'static> Default for SingleFlight<T> {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                next_id: 0,
                flights: HashMap::new(),
            })),
        }
    }
}

impl<T: Clone + Send + Sync + 'static> SingleFlight<T> {
    /// Runs `f` once per concurrent `key`; every waiter gets a clone of the result. `f` runs on its
    /// own task, so dropping any waiter (including the first) never aborts or repeats the work.
    pub async fn run<F, Fut>(&self, key: &str, f: F) -> Result<T, AuthFlowError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, AuthFlowError>> + Send + 'static,
    {
        let mut rx = {
            let mut inner = self.inner.lock();
            match inner.flights.get(key) {
                Some(flight) => flight.rx.clone(),
                None => {
                    let id = inner.next_id;
                    inner.next_id += 1;
                    let (tx, rx) = watch::channel(None);
                    inner
                        .flights
                        .insert(key.to_string(), Flight { id, rx: rx.clone() });
                    let shared = self.inner.clone();
                    let key = key.to_string();
                    let work = tokio::spawn(f());
                    tokio::spawn(async move {
                        let result = match work.await {
                            Ok(r) => r,
                            Err(e) => Err(AuthFlowError::other(format!(
                                "single-flight task failed: {e}"
                            ))),
                        };
                        // Publish first so waiters that already hold the receiver see the value,
                        // then free the key so later calls start a fresh flight.
                        let _ = tx.send(Some(result));
                        let mut inner = shared.lock();
                        if inner.flights.get(&key).is_some_and(|fl| fl.id == id) {
                            inner.flights.remove(&key);
                        }
                    });
                    rx
                }
            }
        };
        match rx.wait_for(|v| v.is_some()).await {
            Ok(v) => v
                .clone()
                .unwrap_or_else(|| Err(AuthFlowError::other("single-flight produced no result"))),
            Err(_) => Err(AuthFlowError::other("single-flight task dropped")),
        }
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

    #[tokio::test]
    async fn dropping_the_initiator_neither_aborts_nor_repeats_the_call() {
        let sf = Arc::new(SingleFlight::<usize>::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let initiator = tokio::spawn({
            let sf = sf.clone();
            async move {
                sf.run("k", move || async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    Ok(1)
                })
                .await
            }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        initiator.abort();
        let c2 = calls.clone();
        let waiter = sf
            .run("k", move || async move {
                c2.fetch_add(1, Ordering::SeqCst);
                Ok(2)
            })
            .await
            .unwrap();
        assert_eq!(waiter, 1, "waiter must receive the original call's result");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

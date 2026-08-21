//! The minimal async runtime a single-threaded wasm component can offer
//! subduction: a busy-poll executor, a deadline timer, and block-on that
//! keeps the executor and websocket moving while a foreground future runs.
//!
//! Subduction's builder wants `Spawn + Send + Sync` and boxed `Sendable`
//! futures even though nothing here ever leaves the one guest thread — so
//! everything is `Arc<Mutex<…>>` and the futures are `Send`, and the
//! "runtime" is: poll every task every pump, no wakers honored (a no-op
//! waker plus unconditional re-poll cannot lose a wakeup, it just spends a
//! poll). Pump frequency is bounded by the serve loop, so the spin is paced
//! by `poll-request`'s bounded wait, not a hot loop.

use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures::future::{AbortHandle, Abortable, BoxFuture, FutureExt};
use futures::task::noop_waker;
use subduction_core::spawn::Spawn;
use subduction_core::timeout::{TimedOut, Timeout};

/// The task queue. Cloned handles share it; `pump` polls everything once.
#[derive(Clone, Default)]
pub struct Exec {
    tasks: Arc<Mutex<Vec<BoxFuture<'static, ()>>>>,
}

impl Exec {
    pub fn new() -> Self {
        Self::default()
    }

    /// Poll every task once; drop the finished. Returns how many remain.
    pub fn pump(&self) -> usize {
        // Take the queue so a task that spawns during its own poll doesn't
        // deadlock on the mutex.
        let mut tasks = std::mem::take(&mut *self.tasks.lock().unwrap());
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        tasks.retain_mut(|t| t.as_mut().poll(&mut cx).is_pending());
        let mut q = self.tasks.lock().unwrap();
        // Anything spawned mid-pump landed in the fresh queue; keep both.
        tasks.append(&mut q);
        *q = tasks;
        q.len()
    }

    /// Drive `fut` to completion, pumping the background tasks between polls
    /// and calling `idle` (the caller's own I/O pump) when nothing is ready.
    pub fn block_on<T>(
        &self,
        fut: impl std::future::Future<Output = T>,
        deadline: Duration,
    ) -> Result<T, TimedOut> {
        let mut fut = Box::pin(fut);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let end = Instant::now() + deadline;
        loop {
            if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                return Ok(v);
            }
            self.pump();
            if Instant::now() >= end {
                return Err(TimedOut);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

impl Spawn<future_form::Sendable> for Exec {
    fn spawn(&self, fut: BoxFuture<'static, ()>) -> AbortHandle {
        let (handle, reg) = AbortHandle::new_pair();
        let fut = Abortable::new(fut, reg).map(|_| ()).boxed();
        self.tasks.lock().unwrap().push(fut);
        handle
    }
}

/// Deadline timer over `Instant`: polls the inner future, then the clock.
/// No waker bookkeeping — the executor re-polls everything anyway.
#[derive(Clone)]
pub struct Timer;

impl Timeout<future_form::Sendable> for Timer {
    fn timeout<'a, T: 'a>(
        &'a self,
        dur: Duration,
        fut: BoxFuture<'a, T>,
    ) -> BoxFuture<'a, Result<T, TimedOut>> {
        let deadline = Instant::now() + dur;
        let mut fut = fut;
        // poll_fn rather than an async block: the future must be `Send`
        // without a `T: Send` bound, so nothing of type `T` may be held
        // across an await — poll and return by value.
        Box::pin(futures::future::poll_fn(move |cx| {
            if let Poll::Ready(v) = fut.as_mut().poll(cx) {
                return Poll::Ready(Ok(v));
            }
            if Instant::now() >= deadline {
                return Poll::Ready(Err(TimedOut));
            }
            cx.waker().wake_by_ref();
            Poll::Pending
        }))
    }
}

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zork_agent::session::ports::{Clock, Sleep};

/// Keeps a current-thread Tokio runtime active while a test combines paused
/// time with real sockets. Without it, Tokio may auto-advance to the next
/// timer before the operating system reports ready I/O.
pub struct PausedTimeIoGuard {
    driver: tokio::task::JoinHandle<()>,
}

impl PausedTimeIoGuard {
    pub fn start() -> Self {
        Self {
            driver: tokio::spawn(async {
                loop {
                    tokio::task::yield_now().await;
                }
            }),
        }
    }
}

impl Drop for PausedTimeIoGuard {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

#[derive(Clone, Debug)]
pub struct ManualClock {
    inner: Arc<Mutex<State>>,
    timer_added: Arc<tokio::sync::Notify>,
}

#[derive(Debug)]
struct State {
    now_ms: i64,
    next_timer: u64,
    timers: BTreeMap<(i64, u64), tokio::sync::oneshot::Sender<()>>,
}

impl ManualClock {
    pub fn new(now_ms: i64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(State {
                now_ms,
                next_timer: 0,
                timers: BTreeMap::new(),
            })),
            timer_added: Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub fn advance(&self, duration: Duration) {
        let millis = i64::try_from(duration.as_millis()).unwrap_or(i64::MAX);
        self.advance_to(self.now_ms().saturating_add(millis));
    }

    pub fn advance_to(&self, now_ms: i64) {
        let ready = {
            let mut state = self.inner.lock().expect("manual clock lock poisoned");
            state.now_ms = state.now_ms.max(now_ms);
            let first_pending = state
                .timers
                .keys()
                .find(|(deadline_ms, _)| *deadline_ms > state.now_ms)
                .copied();
            match first_pending {
                Some(first_pending) => {
                    let pending = state.timers.split_off(&first_pending);
                    std::mem::replace(&mut state.timers, pending)
                }
                None => std::mem::take(&mut state.timers),
            }
        };
        for (_, timer) in ready {
            let _ = timer.send(());
        }
    }

    pub fn pending_timer_count(&self) -> usize {
        let mut state = self.inner.lock().expect("manual clock lock poisoned");
        state.timers.retain(|_, timer| !timer.is_closed());
        state.timers.len()
    }

    pub fn current_ms(&self) -> i64 {
        self.now_ms()
    }

    pub fn pending_deadlines(&self) -> Vec<i64> {
        let mut state = self.inner.lock().expect("manual clock lock poisoned");
        state.timers.retain(|_, timer| !timer.is_closed());
        state
            .timers
            .keys()
            .map(|(deadline_ms, _)| *deadline_ms)
            .collect()
    }

    pub async fn wait_for_pending_timer(&self) {
        loop {
            let added = self.timer_added.notified();
            if self.pending_timer_count() > 0 {
                return;
            }
            added.await;
        }
    }

    pub async fn wait_for_pending_deadline(&self, expected_deadline_ms: i64) {
        loop {
            let added = self.timer_added.notified();
            if self.pending_deadlines().contains(&expected_deadline_ms) {
                return;
            }
            added.await;
        }
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new(1_700_000_000_000)
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> i64 {
        self.inner
            .lock()
            .expect("manual clock lock poisoned")
            .now_ms
    }

    fn sleep_until(&self, deadline_ms: i64) -> Sleep {
        let receiver = {
            let mut state = self.inner.lock().expect("manual clock lock poisoned");
            if deadline_ms <= state.now_ms {
                return Box::pin(std::future::ready(()));
            }
            state.next_timer = state.next_timer.saturating_add(1);
            let key = (deadline_ms, state.next_timer);
            let (sender, receiver) = tokio::sync::oneshot::channel();
            state.timers.insert(key, sender);
            receiver
        };
        self.timer_added.notify_waiters();
        Box::pin(async move {
            let _ = receiver.await;
        })
    }
}

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

pub type Sleep = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Wall time and deadline waiting used by the session runtime.
///
/// Persisted deadlines use epoch milliseconds so they remain meaningful after
/// recovery. Implementations own how those deadlines are waited for.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;

    fn sleep_until(&self, deadline_ms: i64) -> Sleep;

    fn sleep(&self, duration: Duration) -> Sleep {
        self.sleep_until(self.now_ms().saturating_add(duration_ms(duration)))
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        system_now_ms()
    }

    fn sleep_until(&self, deadline_ms: i64) -> Sleep {
        let remaining_ms = deadline_ms.saturating_sub(system_now_ms()).max(0) as u64;
        Box::pin(tokio::time::sleep(Duration::from_millis(remaining_ms)))
    }
}

pub fn system_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

fn duration_ms(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

struct RateLimiterState {
    next_allowed: Instant,
}

/// Controls the delay between outbound requests to avoid hammering the server.
///
/// Thread-safe and stateful: schedules request slots sequentially so concurrent tasks
/// do not burst simultaneously.
#[derive(Clone)]
pub struct RateLimiter {
    state: Arc<Mutex<RateLimiterState>>,
    delay: Duration,
}

impl RateLimiter {
    pub fn new(delay_ms: u64) -> Self {
        Self {
            state: Arc::new(Mutex::new(RateLimiterState {
                next_allowed: Instant::now(),
            })),
            delay: Duration::from_millis(delay_ms),
        }
    }

    /// Wait until the next allowed request slot arrives.
    ///
    /// Atomically claims a slot so concurrent tasks are spaced apart by at least `delay_ms`.
    pub async fn wait(&self) {
        if self.delay.is_zero() {
            return;
        }

        let target = {
            let mut state = self.state.lock().await;
            let now = Instant::now();
            let scheduled = if state.next_allowed > now {
                state.next_allowed
            } else {
                now
            };
            state.next_allowed = scheduled + self.delay;
            scheduled
        };

        let now = Instant::now();
        if target > now {
            tokio::time::sleep(target - now).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_rate_limiter_paces_concurrent_tasks() {
        let delay_ms = 40;
        let limiter = RateLimiter::new(delay_ms);
        let start = Instant::now();

        let mut handles = Vec::new();
        for _ in 0..4 {
            let lim = limiter.clone();
            handles.push(tokio::spawn(async move {
                lim.wait().await;
                Instant::now()
            }));
        }

        let mut timestamps = Vec::new();
        for h in handles {
            timestamps.push(h.await.unwrap());
        }

        timestamps.sort();

        // The 4 requests should span at least 3 * delay_ms = 120ms
        let total_span = timestamps.last().unwrap().duration_since(*timestamps.first().unwrap());
        assert!(
            total_span >= Duration::from_millis(delay_ms * 3 - 15),
            "Expected span >= ~{}ms but got {:?}",
            delay_ms * 3,
            total_span
        );

        // Verify each consecutive slot is spaced
        for i in 1..timestamps.len() {
            let diff = timestamps[i].duration_since(timestamps[i - 1]);
            assert!(
                diff >= Duration::from_millis(delay_ms - 15),
                "Task {} and {} spaced by only {:?}",
                i - 1,
                i,
                diff
            );
        }

        let _ = start;
    }
}

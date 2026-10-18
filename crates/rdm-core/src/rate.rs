//! Global token bucket shared by every worker (architecture §3.5).
//!
//! Workers `acquire` the number of bytes they are about to write; the bucket is
//! shared, so N connections consume one global budget instead of each being
//! throttled independently. Burst capacity is a quarter second (min 64 KiB) so
//! short downloads cannot massively overshoot the configured average.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::time::sleep;

const MIN_BURST: f64 = 64.0 * 1024.0;

pub(crate) struct TokenBucket {
    rate: f64,
    burst: f64,
    state: Mutex<Bucket>,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    pub fn new(bytes_per_sec: u64) -> Self {
        let rate = bytes_per_sec.max(1) as f64;
        let burst = (rate / 4.0).clamp(MIN_BURST.min(rate), rate);
        Self {
            rate,
            burst,
            state: Mutex::new(Bucket {
                tokens: burst,
                last: Instant::now(),
            }),
        }
    }

    /// Block until `bytes` of the global budget are available.
    pub async fn acquire(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let wait = {
            let mut bucket = self.state.lock().expect("rate limiter mutex poisoned");
            let now = Instant::now();
            let elapsed = now.duration_since(bucket.last).as_secs_f64();
            bucket.tokens = (bucket.tokens + elapsed * self.rate).min(self.burst);
            bucket.last = now;
            // Reserve the bytes now (may go negative); everyone waits together.
            bucket.tokens -= bytes as f64;
            if bucket.tokens >= 0.0 {
                0.0
            } else {
                -bucket.tokens / self.rate
            }
        };
        if wait > 0.0 {
            sleep(Duration::from_secs_f64(wait)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn throttles_above_the_configured_rate() {
        // 512 KiB/s with 256 KiB burst: acquiring 768 KiB must wait ~0.5 s.
        let bucket = TokenBucket::new(512 * 1024);
        let started = Instant::now();
        bucket.acquire(768 * 1024).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(450),
            "limiter returned too early: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "limiter waited far too long: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn within_burst_is_immediate() {
        let bucket = TokenBucket::new(4 * 1024 * 1024);
        let started = Instant::now();
        bucket.acquire(128 * 1024).await;
        assert!(started.elapsed() < Duration::from_millis(200));
    }
}

use std::time::Duration;

use tokio::time::Instant;

/// Token bucket refilled at `rate` tokens per second, holding at most one
/// second's worth.
#[derive(Debug)]
pub(crate) struct TokenBucket {
    rate: f64,
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    /// `None` when `rate` is 0 (unlimited).
    pub fn new(rate: u32) -> Option<Self> {
        (rate > 0).then(|| Self {
            rate: f64::from(rate),
            tokens: f64::from(rate),
            last: Instant::now(),
        })
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * self.rate).min(self.rate);
    }

    /// Takes `cost` tokens, going into debt, and returns how long the caller
    /// must wait for the debt to be repaid (for throttling, e.g. TCP).
    pub fn reserve(&mut self, cost: f64) -> Duration {
        self.refill();
        self.tokens -= cost;
        if self.tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-self.tokens / self.rate)
        }
    }
}

/// A packet-count and a byte bucket checked together.
#[derive(Debug)]
pub(crate) struct RateLimit {
    packets: Option<TokenBucket>,
    bytes: Option<TokenBucket>,
}

impl RateLimit {
    pub fn new(packets_per_sec: u32, bytes_per_sec: u32) -> Self {
        Self {
            packets: TokenBucket::new(packets_per_sec),
            bytes: TokenBucket::new(bytes_per_sec),
        }
    }

    pub fn try_take(&mut self, len: usize) -> bool {
        // Check both before taking from either, so a refused packet costs nothing.
        let packets_ok = self.packets.as_mut().is_none_or(|b| {
            b.refill();
            b.tokens >= 1.0
        });
        let bytes_ok = self.bytes.as_mut().is_none_or(|b| {
            b.refill();
            b.tokens >= len as f64
        });
        if !(packets_ok && bytes_ok) {
            return false;
        }
        if let Some(b) = &mut self.packets {
            b.tokens -= 1.0;
        }
        if let Some(b) = &mut self.bytes {
            b.tokens -= len as f64;
        }
        true
    }

    pub fn reserve(&mut self, len: usize) -> Duration {
        let packets = self
            .packets
            .as_mut()
            .map_or(Duration::ZERO, |b| b.reserve(1.0));
        let bytes = self
            .bytes
            .as_mut()
            .map_or(Duration::ZERO, |b| b.reserve(len as f64));
        packets.max(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn bucket_refills_over_time() {
        let mut limit = RateLimit::new(10, 0);
        for _ in 0..10 {
            assert!(limit.try_take(100));
        }
        assert!(!limit.try_take(1));
        tokio::time::advance(Duration::from_millis(100)).await;
        assert!(limit.try_take(1));
        assert!(!limit.try_take(1));
    }

    #[tokio::test(start_paused = true)]
    async fn reserve_reports_debt() {
        let mut limit = RateLimit::new(0, 1000);
        assert_eq!(limit.reserve(1000), Duration::ZERO);
        let wait = limit.reserve(500);
        assert!((wait.as_secs_f64() - 0.5).abs() < 1e-6, "{wait:?}");
    }

    #[test]
    fn zero_means_unlimited() {
        let mut limit = RateLimit::new(0, 0);
        for _ in 0..10_000 {
            assert!(limit.try_take(1 << 20));
        }
    }
}

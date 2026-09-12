//! Per-link token bucket. Charge only completed IO, including framing overhead.
use std::time::Instant;

pub(crate) struct Bucket {
    rate: Option<u64>,
    tokens: u128,
    capacity: u128,
    last: Instant,
}

const SCALE: u128 = 1_000_000_000;

impl Bucket {
    pub(crate) fn new(rate: Option<u64>, now: Instant) -> Self {
        // Allow 100ms of burst, with one normal Ethernet frame at minimum.
        let capacity = u128::from(rate.unwrap_or(0) / 10).max(1518) * SCALE;
        Self {
            rate,
            tokens: capacity,
            capacity,
            last: now,
        }
    }

    pub(crate) fn available(&mut self, now: Instant) -> usize {
        let Some(rate) = self.rate else {
            return usize::MAX;
        };
        self.tokens = self
            .tokens
            .saturating_add(
                now.saturating_duration_since(self.last)
                    .as_nanos()
                    .saturating_mul(u128::from(rate)),
            )
            .min(self.capacity);
        self.last = now;
        (self.tokens / SCALE) as usize
    }

    pub(crate) fn ready(&mut self, now: Instant) -> bool {
        // Do not wake an always-writable fd for each newly accrued byte.
        self.available(now) >= 1024
    }

    pub(crate) fn consume(&mut self, bytes: usize) {
        if self.rate.is_some() {
            self.tokens = self.tokens.saturating_sub(bytes as u128 * SCALE);
        }
    }
}

#[test]
fn bandwidth_envelope_and_idle_burst_are_bounded() {
    use std::time::Duration;
    let start = Instant::now();
    let mut bucket = Bucket::new(Some(100_000), start);
    assert_eq!(bucket.available(start), 10_000);
    bucket.consume(10_000);
    assert_eq!(bucket.available(start), 0);
    assert_eq!(bucket.available(start + Duration::from_millis(5)), 500);
    // Partial IO charges only the bytes actually transferred.
    bucket.consume(100);
    assert_eq!(bucket.available(start + Duration::from_millis(5)), 400);
    assert_eq!(bucket.available(start + Duration::from_secs(3600)), 10_000);
    let mut total = 0;
    let mut bucket = Bucket::new(Some(65_537), start);
    for millisecond in 0..1000 {
        let bytes = bucket.available(start + Duration::from_millis(millisecond));
        total += bytes;
        bucket.consume(bytes);
    }
    assert!(total <= 65_537 + 6553);
    assert!(total > 65_537);
}

#[test]
fn unlimited_default_does_not_throttle() {
    let now = Instant::now();
    let mut bucket = Bucket::new(None, now);
    bucket.consume(usize::MAX);
    assert_eq!(bucket.available(now), usize::MAX);
}

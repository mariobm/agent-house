//! Opt-in aggregate experiment counters. Never log keys, content or credentials.
use ahvm_volume::{Head, ObjectStore, Result};
use std::sync::{
    atomic::{AtomicU64, Ordering::Relaxed},
    Arc,
};
use std::time::Instant;
#[derive(Default)]
struct Counter {
    calls: AtomicU64,
    micros: AtomicU64,
}
impl Counter {
    fn run<T>(&self, f: impl FnOnce() -> T) -> T {
        let start = Instant::now();
        let out = f();
        self.calls.fetch_add(1, Relaxed);
        self.micros
            .fetch_add(start.elapsed().as_micros() as u64, Relaxed);
        out
    }
    fn value(&self) -> (u64, u64) {
        (self.calls.load(Relaxed), self.micros.load(Relaxed) / 1000)
    }
}
pub struct Measured {
    inner: Arc<dyn ObjectStore>,
    get: Counter,
    put: Counter,
    head: Counter,
}
impl std::fmt::Debug for Measured {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MeasuredStore")
    }
}
impl Measured {
    pub fn wrap(inner: Arc<dyn ObjectStore>) -> Arc<dyn ObjectStore> {
        if std::env::var_os("AHVM_VOLUME_METRICS").is_none() {
            return inner;
        }
        let store = Arc::new(Self {
            inner,
            get: Counter::default(),
            put: Counter::default(),
            head: Counter::default(),
        });
        let weak = Arc::downgrade(&store);
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            let Some(s) = weak.upgrade() else { break };
            eprintln!(
                "store totals (calls, aggregate_ms): get={:?} put={:?} head={:?}",
                s.get.value(),
                s.put.value(),
                s.head.value()
            );
        });
        store
    }
}
impl ObjectStore for Measured {
    fn head(&self, id: &str) -> Result<Option<Head>> {
        self.head.run(|| self.inner.head(id))
    }
    fn chunk(&self, id: &str, h: &str) -> Result<Vec<u8>> {
        self.get.run(|| self.inner.chunk(id, h))
    }
    fn put_chunk(&self, id: &str, h: &str, b: &[u8]) -> Result<()> {
        self.put.run(|| self.inner.put_chunk(id, h, b))
    }
    fn publish(&self, id: &str, r: Option<&str>, b: &[u8]) -> Result<String> {
        self.head.run(|| self.inner.publish(id, r, b))
    }
}

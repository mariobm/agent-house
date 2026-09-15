//! Opt-in lifecycle measurements. No command payloads, paths or credentials.
use std::io::Write;
use std::sync::OnceLock;
use std::time::Instant;

/// Cached once at process startup/first use; changing the environment later has no effect.
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("AHVM_TIMINGS").as_deref() == Ok("1"))
}

/// Emit one JSON line on stderr per completed stage when AHVM_TIMINGS=1.
/// Identity is a sandbox/volume ID supplied by the host, never request contents.
/// Nested stages overlap: their durations must not be summed with their parent.
pub fn measure<T, E>(
    component: &'static str,
    stage: &'static str,
    id: &str,
    work: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    if !enabled() {
        return work();
    }
    let start = Instant::now();
    let result = work();
    let event = serde_json::json!({
        "event": "ahvm_timing", "component": component, "stage": stage,
        "id": id, "pid": std::process::id(),
        "elapsed_ms": start.elapsed().as_secs_f64() * 1000.0,
        "ok": result.is_ok(),
    });
    // Diagnostics must not turn a successful lifecycle operation into a failure.
    let _ = writeln!(std::io::stderr().lock(), "{event}");
    result
}

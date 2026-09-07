//! Thermal manager: idle Hot/running sandboxes go Cold/stopped, and
//! store rows reconcile with backend truth.
//!
//! Runs as a background task calling [`sweep_once`] on a cadence.
//! Activity is the last guest-touching op per sandbox (routes touch it;
//! lifecycle transitions don't need to). First-seen running sandboxes are
//! recorded, never stopped, so a daemon restart never causes a mass stop
//! before any activity is observed.

use crate::{state_str, thermal_str, unix_now, AppState};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Last-guest-activity per sandbox id (monotonic clock, daemon-local).
/// Rebuilt from zero on restart (first-seen rule covers the gap).
#[derive(Debug, Clone, Default)]
pub struct ActivityTracker {
    inner: Arc<Mutex<HashMap<String, Instant>>>,
}

impl ActivityTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record activity now.
    pub fn touch(&self, id: &str) {
        self.touch_at(id, Instant::now());
    }

    /// Record activity at `at`. Public as a recovery/testing hook (e.g.
    /// backdating in tests, or seeding from persisted timestamps later).
    pub fn touch_at(&self, id: &str, at: Instant) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.insert(id.to_string(), at);
        }
    }

    /// Forget an id (destroy). Keeps the map bounded; a recreated id
    /// starts fresh under the first-seen rule.
    pub fn remove(&self, id: &str) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.remove(id);
        }
    }

    pub fn last(&self, id: &str) -> Option<Instant> {
        self.inner.lock().ok()?.get(id).copied()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ThermalConfig {
    /// Running sandboxes idle longer than this get stopped.
    pub idle_secs: u64,
    /// Sweep cadence (also the startup reconcile delay budget).
    pub sweep_secs: u64,
}

impl Default for ThermalConfig {
    fn default() -> Self {
        Self {
            idle_secs: 3600,
            sweep_secs: 60,
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepStats {
    pub checked: usize,
    pub stopped: usize,
    pub reconciled: usize,
}

/// One sweep pass: page all sandbox rows, reconcile each with backend
/// truth, stop the idle. Never fails the sweep on one bad sandbox
/// (error diamonds: backend gone, worker wedged, store hiccup).
pub async fn sweep_once(state: &AppState, cfg: ThermalConfig, now: Instant) -> SweepStats {
    let mut stats = SweepStats::default();
    let mut after: Option<(i64, String)> = None;
    loop {
        let rows = match state.store.list_all_sandboxes(after, 100) {
            Ok(rows) => rows,
            Err(e) => {
                eprintln!("thermal: list: {e}");
                return stats;
            }
        };
        if rows.is_empty() {
            break;
        }
        after = rows.last().map(|r| (r.created_at, r.id.clone()));
        for row in rows {
            stats.checked += 1;
            // Backend truth first (blocking pool; NotFound handled below).
            let backend = state.backend.clone();
            let id = row.id.clone();
            let live = tokio::task::spawn_blocking(move || backend.status(&id)).await;
            let live = match live {
                Ok(Ok(info)) => Some(info),
                Ok(Err(ahvm_engine::Error::NotFound(_))) => None,
                Ok(Err(e)) => {
                    eprintln!("thermal: status {}: {e}", row.id);
                    continue;
                }
                Err(e) => {
                    eprintln!("thermal: status task {}: {e}", row.id);
                    continue;
                }
            };
            let Some(live) = live else {
                // Backend lost the record (data wiped): surface Failed
                // rather than a stale Running row. Destroy stays explicit.
                if row.state != "failed" {
                    let _ = state
                        .store
                        .set_sandbox_state(&row.id, "failed", "cold", unix_now());
                    stats.reconciled += 1;
                }
                continue;
            };
            if state_str(&live.state) != row.state || thermal_str(&live.thermal) != row.thermal {
                let _ = state.store.set_sandbox_state(
                    &row.id,
                    &state_str(&live.state),
                    &thermal_str(&live.thermal),
                    unix_now(),
                );
                stats.reconciled += 1;
            }
            if live.state != ahvm_engine::State::Running {
                continue;
            }
            let last = match state.activity.last(&row.id) {
                Some(t) => t,
                None => {
                    // First sight (e.g. right after a daemon restart):
                    // record now, never stop on first sight.
                    state.activity.touch_at(&row.id, now);
                    continue;
                }
            };
            if now.duration_since(last).as_secs() > cfg.idle_secs {
                let backend = state.backend.clone();
                let id = row.id.clone();
                let stopped = tokio::task::spawn_blocking(move || backend.stop(&id)).await;
                match stopped {
                    Ok(Ok(())) => {
                        let _ =
                            state
                                .store
                                .set_sandbox_state(&row.id, "stopped", "cold", unix_now());
                        stats.stopped += 1;
                    }
                    Ok(Err(e)) => eprintln!("thermal: stop {}: {e}", row.id),
                    Err(e) => eprintln!("thermal: stop task {}: {e}", row.id),
                }
            }
        }
    }
    stats
}

/// Sweep forever. Shutdown is process exit (no shared state to flush:
/// activity rebuilds, records persist per-op).
pub async fn run(state: AppState, cfg: ThermalConfig) -> ! {
    loop {
        tokio::time::sleep(Duration::from_secs(cfg.sweep_secs)).await;
        let stats = sweep_once(&state, cfg, Instant::now()).await;
        if stats.stopped + stats.reconciled > 0 {
            eprintln!(
                "thermal: sweep checked={} stopped={} reconciled={}",
                stats.checked, stats.stopped, stats.reconciled
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ahvm_engine::{Backend, MockBackend, State};
    use std::time::Duration;

    fn state() -> AppState {
        let store = Arc::new(ahvm_store::Store::open_in_memory().unwrap());
        let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(
            std::env::temp_dir().join(format!("thermal-{}", std::process::id())),
        ));
        AppState {
            store,
            backend,
            quotas: crate::quotas::Registry::new(),
            activity: ActivityTracker::new(),
            ops: crate::scheduler::OpsLimiter::new(4),
        }
    }

    fn owner(store: &ahvm_store::Store) {
        store
            .upsert_user(&ahvm_store::User {
                id: "u".to_string(),
                name: "u".to_string(),
                api_key_hash: "h".to_string(),
                max_sandboxes: 8,
                max_cpus: 32,
                max_memory_mb: 65536,
                max_volumes_mb: 0,
                max_snapshots: 8,
                created_at: 0,
                updated_at: 0,
            })
            .unwrap();
    }

    fn row(id: &str, state: &str, thermal: &str) -> ahvm_store::Sandbox {
        ahvm_store::Sandbox {
            id: id.to_string(),
            owner_user_id: "u".to_string(),
            name: id.to_string(),
            backend: ahvm_store::Backend::Krucible,
            state: state.to_string(),
            thermal: thermal.to_string(),
            cpus: 1,
            memory_mb: 512,
            ip: String::new(),
            created_at: 0,
            updated_at: 0,
        }
    }

    /// Mirror a mock-backend sandbox into the store under a chosen id.
    /// MockBackend generates its own ids, so create-then-relabel: create
    /// via the backend, then rewrite the store row to the backend id.
    fn live_pair(st: &AppState, name: &str) -> String {
        let spec = ahvm_engine::SandboxSpec {
            name: name.to_string(),
            cpus: 1,
            memory_mb: 512,
            backend: ahvm_engine::BackendKind::Krucible,
            root_image: None,
            kernel_image: None,
            extra_env: Default::default(),
        };
        let info = st.backend.create(&spec).unwrap();
        let mut r = row(&info.id, "running", "hot");
        r.name = name.to_string();
        st.store.create_sandbox(&r).unwrap();
        info.id
    }

    #[tokio::test]
    async fn idle_running_stops_active_stays_first_seen_skips() {
        let st = state();
        owner(&st.store);
        let idle = live_pair(&st, "idle");
        let active = live_pair(&st, "active");
        let fresh = live_pair(&st, "fresh");
        let now = Instant::now();
        st.activity.touch_at(&idle, now - Duration::from_secs(7200));
        st.activity.touch_at(&active, now);
        // `fresh` deliberately untouched: first sight must not stop it.
        let stats = sweep_once(
            &st,
            ThermalConfig {
                idle_secs: 3600,
                sweep_secs: 60,
            },
            now,
        )
        .await;
        assert_eq!(stats.checked, 3);
        assert_eq!(stats.stopped, 1);
        assert_eq!(st.store.get_sandbox(&idle).unwrap().state, "stopped");
        assert_eq!(st.store.get_sandbox(&active).unwrap().state, "running");
        assert_eq!(st.store.get_sandbox(&fresh).unwrap().state, "running");
        assert!(st.activity.last(&fresh).is_some());
        // And the backend agrees about the stopped one.
        assert_eq!(st.backend.status(&idle).unwrap().state, State::Stopped);
    }

    #[tokio::test]
    async fn backend_missing_row_marks_failed_stopped_left_alone() {
        let st = state();
        owner(&st.store);
        // Store row with no backend record: backend data was wiped.
        st.store
            .create_sandbox(&row("ghost", "running", "hot"))
            .unwrap();
        // Stopped row, backend still knows it (mock keeps stopped records).
        let stopped = live_pair(&st, "down");
        st.backend.stop(&stopped).unwrap();
        st.store
            .set_sandbox_state(&stopped, "stopped", "cold", 0)
            .unwrap();
        let stats = sweep_once(&st, ThermalConfig::default(), Instant::now()).await;
        assert_eq!(stats.checked, 2);
        assert_eq!(st.store.get_sandbox("ghost").unwrap().state, "failed");
        assert_eq!(st.store.get_sandbox(&stopped).unwrap().state, "stopped");
    }

    #[tokio::test]
    async fn drift_reconciles_to_backend_truth() {
        let st = state();
        owner(&st.store);
        let id = live_pair(&st, "drift");
        // Store says stopped; backend says running: backend wins.
        st.store
            .set_sandbox_state(&id, "stopped", "cold", 0)
            .unwrap();
        let stats = sweep_once(&st, ThermalConfig::default(), Instant::now()).await;
        assert_eq!(stats.reconciled, 1);
        assert_eq!(st.store.get_sandbox(&id).unwrap().state, "running");
    }
}

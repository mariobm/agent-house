//! Thermal manager: idle running sandboxes pause, then stop later, and
//! store rows reconcile with backend truth.
//!
//! Runs as a background task calling [`sweep_once`] on a cadence.
//! Activity is the last guest-touching op per sandbox (routes touch it;
//! lifecycle transitions don't need to). First-seen running sandboxes are
//! recorded, never stopped, so a daemon restart never causes a mass stop
//! before any activity is observed.

use crate::{state_str, thermal_str, unix_now, AppState};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const DEFAULT_AGENT_IDLE_STOP_SECS: u64 = 300;

/// Last-guest-activity per sandbox id (monotonic clock, daemon-local).
/// Rebuilt from zero on restart (first-seen rule covers the gap).
#[derive(Debug, Clone)]
pub struct ActivityTracker {
    inner: Arc<Mutex<TrackerInner>>,
    policy_updates: Arc<tokio::sync::Mutex<()>>,
}

impl Default for ActivityTracker {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(TrackerInner {
                agent_idle_stop_secs: DEFAULT_AGENT_IDLE_STOP_SECS,
                ..Default::default()
            })),
            policy_updates: Default::default(),
        }
    }
}

#[derive(Debug, Default)]
struct TrackerInner {
    pause_after_secs: u64,
    agent_idle_stop_secs: u64,
    last: HashMap<String, Instant>,
    inflight: HashMap<String, (u64, usize)>,
    /// Independent of controller task lifetime; durable run recovery owns clearing it.
    managed_runs: HashMap<String, (String, i64)>,
    /// Completed managed VMs retain a shorter stop policy until removed.
    managed_cooldown: HashSet<String>,
    next_generation: u64,
    /// Ids with a committed stop: no new guest work admits until the
    /// transition finishes (see [`ActivityTracker::begin_stop`]).
    stopping: HashSet<String>,
}

impl ActivityTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn pause_after_secs(&self) -> u64 {
        self.inner.lock().unwrap().pause_after_secs
    }
    pub fn set_pause_after_secs(&self, seconds: u64) {
        self.inner.lock().unwrap().pause_after_secs = seconds;
    }

    pub fn idle_policy(&self) -> IdlePolicy {
        let inner = self.inner.lock().unwrap();
        IdlePolicy {
            pause_after_secs: inner.pause_after_secs,
            agent_idle_stop_secs: inner.agent_idle_stop_secs,
        }
    }

    pub fn set_idle_policy(&self, policy: IdlePolicy) {
        let mut inner = self.inner.lock().unwrap();
        inner.pause_after_secs = policy.pause_after_secs;
        inner.agent_idle_stop_secs = policy.agent_idle_stop_secs;
    }

    /// Install a durable controller's current run identity after its lifecycle
    /// lock and persisted CAS. A task panic or lease expiry does not drop this hold.
    pub fn set_managed_run(&self, id: &str, run_id: &str, epoch: i64) -> bool {
        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };
        if inner.stopping.contains(id) {
            return false;
        }
        inner
            .managed_runs
            .insert(id.to_string(), (run_id.to_string(), epoch));
        inner.last.insert(id.to_string(), Instant::now());
        true
    }

    /// Only the current identity may finish. Persist completion before calling;
    /// later guest activity must not be backdated by a recovered finish timestamp.
    pub fn finish_managed_run(
        &self,
        id: &str,
        run_id: &str,
        epoch: i64,
        finished_at: Instant,
    ) -> bool {
        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };
        if !inner
            .managed_runs
            .get(id)
            .is_some_and(|(run, generation)| run == run_id && *generation == epoch)
        {
            return false;
        }
        inner.managed_runs.remove(id);
        Self::cooldown_at(&mut inner, id, finished_at);
        true
    }

    /// Restore a persisted completed run before starting thermal sweeping.
    /// Never replace an active run or race an already committed transition.
    pub fn restore_managed_cooldown(&self, id: &str, finished_at: Instant) -> bool {
        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };
        if inner.stopping.contains(id) || inner.managed_runs.contains_key(id) {
            return false;
        }
        Self::cooldown_at(&mut inner, id, finished_at);
        true
    }

    fn cooldown_at(inner: &mut TrackerInner, id: &str, finished_at: Instant) {
        inner.managed_cooldown.insert(id.to_string());
        inner
            .last
            .entry(id.to_string())
            .and_modify(|last| *last = (*last).max(finished_at))
            .or_insert(finished_at);
    }

    pub fn has_managed_run(&self, id: &str) -> bool {
        self.inner
            .lock()
            .map(|inner| inner.managed_runs.contains_key(id))
            .unwrap_or(false)
    }

    pub fn effective_idle_secs(&self, id: &str, default: u64) -> u64 {
        self.inner
            .lock()
            .map(|inner| {
                if inner.managed_cooldown.contains(id) {
                    inner.agent_idle_stop_secs
                } else {
                    default
                }
            })
            .unwrap_or(default)
    }

    /// Record activity now.
    pub fn touch(&self, id: &str) {
        self.touch_at(id, Instant::now());
    }

    /// Record activity at `at`. Public as a recovery/testing hook (e.g.
    /// backdating in tests, or seeding from persisted timestamps later).
    pub fn touch_at(&self, id: &str, at: Instant) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.last.insert(id.to_string(), at);
        }
    }

    /// Forget an id (destroy). Keeps the map bounded; a recreated id
    /// starts fresh under the first-seen rule.
    pub fn remove(&self, id: &str) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.last.remove(id);
            inner.inflight.remove(id);
            inner.managed_runs.remove(id);
            inner.managed_cooldown.remove(id);
            inner.stopping.remove(id);
        }
    }

    pub fn last(&self, id: &str) -> Option<Instant> {
        self.inner.lock().ok()?.last.get(id).copied()
    }

    /// Mark a guest operation in flight. Returns `None` when a stop has
    /// already committed for `id` — the caller must refuse (409), never
    /// race into a dying worker. Drop ends the guard AND touches:
    /// completion counts as activity. Guards hold no locks across awaits.
    pub fn begin(&self, id: &str) -> Option<InFlight> {
        let mut inner = self.inner.lock().ok()?;
        if inner.stopping.contains(id) {
            return None;
        }
        inner.next_generation = inner.next_generation.wrapping_add(1);
        let next = inner.next_generation;
        let (generation, count) = inner.inflight.entry(id.to_string()).or_insert((next, 0));
        *count += 1;
        Some(InFlight {
            tracker: self.clone(),
            id: id.to_string(),
            generation: *generation,
        })
    }

    /// Atomic idle-gated stop commit: the idle-timestamp check, the
    /// in-flight check, and the stopping commit happen under one lock
    /// acquisition. A short operation that begins and finishes between a
    /// stale idle read and this call still refreshes `last`, so the commit
    /// refuses — checking the timestamp outside (e.g. in the sweep's
    /// prefilter) can only skip work early, never wrongly commit.
    /// Missing entries refuse: without evidence of idleness, don't stop.
    pub fn begin_stop_if_idle(
        &self,
        id: &str,
        now: Instant,
        idle_secs: u64,
    ) -> Option<StopGuard<'_>> {
        self.begin_idle_transition(id, now, idle_secs, false)
    }

    fn begin_pause_if_idle(&self, id: &str, now: Instant, seconds: u64) -> Option<StopGuard<'_>> {
        self.begin_idle_transition(id, now, seconds, true)
    }

    fn begin_idle_transition(
        &self,
        id: &str,
        now: Instant,
        idle_secs: u64,
        pause: bool,
    ) -> Option<StopGuard<'_>> {
        let mut inner = self.inner.lock().ok()?;
        if pause && (idle_secs == 0 || inner.pause_after_secs != idle_secs) {
            return None;
        }
        if inner.stopping.contains(id)
            || inner.managed_runs.contains_key(id)
            || inner.inflight.get(id).map(|(_, count)| *count).unwrap_or(0) > 0
        {
            return None;
        }
        // Recheck the managed policy under the same lock as activity and holds.
        let idle_secs = if !pause && inner.managed_cooldown.contains(id) {
            inner.agent_idle_stop_secs
        } else {
            idle_secs
        };
        match inner.last.get(id) {
            Some(&t) if now.duration_since(t).as_secs() <= idle_secs => None,
            Some(_) => {
                inner.stopping.insert(id.to_string());
                Some(StopGuard {
                    tracker: self,
                    id: id.to_string(),
                })
            }
            // No activity record at all: refuse (the sweep records
            // first-sight separately and skips).
            None => None,
        }
    }

    /// True while a guarded operation or durable managed run is active for `id`.
    pub fn in_flight(&self, id: &str) -> bool {
        self.inner
            .lock()
            .ok()
            .map(|inner| {
                inner.managed_runs.contains_key(id)
                    || inner.inflight.get(id).map(|(_, count)| *count).unwrap_or(0) > 0
            })
            .unwrap_or(false)
    }
}

/// In-flight operation guard from [`ActivityTracker::begin`].
#[derive(Debug)]
pub struct InFlight {
    tracker: ActivityTracker,
    id: String,
    generation: u64,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.tracker.inner.lock() {
            if let Some((generation, n)) = inner.inflight.get_mut(&self.id) {
                if *generation != self.generation {
                    return;
                }
                *n = n.saturating_sub(1);
                if *n == 0 {
                    inner.inflight.remove(&self.id);
                }
            } else {
                return;
            }
            inner.last.insert(self.id.clone(), Instant::now());
        }
    }
}

/// Committed-stop guard from [`ActivityTracker::begin_stop`]. Drop clears
/// the flag WITHOUT touching: a stopped box must not look active.
#[derive(Debug)]
pub struct StopGuard<'a> {
    tracker: &'a ActivityTracker,
    id: String,
}

impl Drop for StopGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.tracker.inner.lock() {
            inner.stopping.remove(&self.id);
        }
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
    pub paused: usize,
    pub reconciled: usize,
    /// Idle rows skipped because all op permits were busy (retried next sweep).
    pub deferred: usize,
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
            // Lifecycle serialization: routes take this same id lock around
            // their backend op + store mirror, so a sampled state can never
            // overwrite a newer commit (or vice versa). Lock order
            // everywhere: lifecycle → permit → backend. Non-blocking: one
            // long operation must not stall reconciliation of later rows.
            let Some(_lc) = state.lifecycle.try_lock(&row.id) else {
                stats.deferred += 1;
                continue;
            };
            if state.store.check_lifecycle_fence(&row.id, None).is_err() {
                stats.deferred += 1;
                continue;
            }
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
            if !matches!(
                live.state,
                ahvm_engine::State::Running | ahvm_engine::State::Paused
            ) {
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
            let pause_secs = state.activity.pause_after_secs();
            let idle_secs = state.activity.effective_idle_secs(&row.id, cfg.idle_secs);
            let stop = now.duration_since(last).as_secs() > idle_secs;
            let pause = !stop
                && pause_secs > 0
                && live.state == ahvm_engine::State::Running
                && now.duration_since(last).as_secs() > pause_secs
                && state.backend.supports_pause(&row.id);
            if stop || pause {
                let threshold = if stop { idle_secs } else { pause_secs };
                // Share the op bound with foreground work: stopping snapshots.
                // Non-blocking take — a busy scheduler defers this row to the
                // next sweep instead of head-of-line blocking the whole pass.
                let Some(_permit) = state.ops.try_acquire() else {
                    stats.deferred += 1;
                    continue;
                };
                // Recheck after admission: activity may have refreshed while
                // the row waited (or while a permit was unavailable). This
                // is only a fast path to skip the commit attempt; the
                // commit itself rechecks atomically below.
                let fresh = state.activity.last(&row.id);
                if fresh.is_some_and(|t| now.duration_since(t).as_secs() <= threshold) {
                    continue;
                }
                // Atomic stop commit (idle timestamp + in-flight + flag in
                // one acquisition): a short op that ran to completion after
                // the stale read above still blocks the commit here.
                let transition = if stop {
                    state.activity.begin_stop_if_idle(&row.id, now, threshold)
                } else {
                    state.activity.begin_pause_if_idle(&row.id, now, threshold)
                };
                let Some(_stop) = transition else {
                    stats.deferred += 1;
                    continue;
                };
                let backend = state.backend.clone();
                let id = row.id.clone();
                let stopped = tokio::task::spawn_blocking(move || {
                    if stop {
                        backend.stop(&id)
                    } else {
                        backend.pause(&id)
                    }
                })
                .await;
                match stopped {
                    Ok(Ok(())) => {
                        let _ = state.store.set_sandbox_state(
                            &row.id,
                            if stop { "stopped" } else { "paused" },
                            if stop { "cold" } else { "warm" },
                            unix_now(),
                        );
                        if stop {
                            stats.stopped += 1;
                        } else {
                            stats.paused += 1;
                        }
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
        tokio::time::sleep(Duration::from_secs(cfg.sweep_secs.clamp(1, 5))).await;
        let stats = sweep_once(&state, cfg, Instant::now()).await;
        if stats.stopped + stats.paused + stats.reconciled > 0 {
            eprintln!(
                "thermal: sweep checked={} stopped={} paused={} reconciled={}",
                stats.checked, stats.stopped, stats.paused, stats.reconciled
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ahvm_engine::{Backend, MockBackend, SandboxInfo, State};
    use std::time::Duration;

    #[test]
    fn managed_run_blocks_quiet_pause_and_stop_until_exact_finish() {
        let t = ActivityTracker::new();
        t.set_pause_after_secs(30);
        assert!(t.set_managed_run("vm", "run", 1));
        let now = Instant::now();
        t.touch_at("vm", now - Duration::from_secs(7200));
        assert!(t.has_managed_run("vm"));
        assert!(t.in_flight("vm"));
        assert!(t.begin_pause_if_idle("vm", now, 30).is_none());
        assert!(t.begin_stop_if_idle("vm", now, 3600).is_none());
        // No RAII guard or controller task is required to keep this hold alive.
        assert!(t.finish_managed_run("vm", "run", 1, now));
        assert!(!t.has_managed_run("vm"));
        assert!(!t.in_flight("vm"));
    }

    #[test]
    fn stale_managed_finish_cannot_release_replacement_or_epoch() {
        let t = ActivityTracker::new();
        assert!(t.set_managed_run("vm", "old", 1));
        assert!(t.set_managed_run("vm", "new", 2));
        assert!(!t.finish_managed_run("vm", "old", 1, Instant::now()));
        assert!(!t.finish_managed_run("vm", "new", 1, Instant::now()));
        assert!(!t.restore_managed_cooldown("vm", Instant::now()));
        assert!(t.has_managed_run("vm"));
        assert_eq!(t.effective_idle_secs("vm", 3600), 3600);
        assert!(t.finish_managed_run("vm", "new", 2, Instant::now()));
        assert!(!t.finish_managed_run("vm", "new", 2, Instant::now()));
    }

    #[test]
    fn managed_completion_uses_300_seconds_at_atomic_commit() {
        let t = ActivityTracker::new();
        assert!(t.set_managed_run("vm", "run", 1));
        let finished = Instant::now();
        assert!(t.finish_managed_run("vm", "run", 1, finished));
        assert_eq!(t.effective_idle_secs("vm", 3600), 300);
        // Even a stale/default caller threshold cannot shorten managed cooldown.
        assert!(t
            .begin_stop_if_idle("vm", finished + Duration::from_secs(300), 0)
            .is_none());
        let stop = t
            .begin_stop_if_idle("vm", finished + Duration::from_secs(301), 3600)
            .unwrap();
        assert!(!t.set_managed_run("vm", "next", 2));
        assert!(!t.restore_managed_cooldown("vm", finished));
        drop(stop);
        assert_eq!(t.effective_idle_secs("vm", 3600), 300);
    }

    #[test]
    fn managed_cooldown_preserves_fresh_activity_and_attached_guards() {
        let t = ActivityTracker::new();
        let finished = Instant::now();
        let now = finished + Duration::from_secs(301);
        assert!(t.restore_managed_cooldown("vm", finished));
        let attached = t.begin("vm").unwrap();
        assert!(t.begin_stop_if_idle("vm", now, 300).is_none());
        drop(attached);
        t.touch_at("vm", now);
        // Restoring an older completed run must not erase recent CLI activity.
        assert!(t.restore_managed_cooldown("vm", finished));
        assert_eq!(t.last("vm"), Some(now));
        assert!(t.begin_stop_if_idle("vm", now, 300).is_none());
        assert!(t.set_managed_run("vm", "next", 2));
        t.touch_at("vm", now);
        assert!(t.finish_managed_run("vm", "next", 2, finished));
        assert_eq!(t.last("vm"), Some(now));
    }

    #[test]
    fn remove_resets_managed_identity_and_policy() {
        let t = ActivityTracker::new();
        assert!(t.restore_managed_cooldown("vm", Instant::now()));
        assert!(t.set_managed_run("vm", "run", 1));
        t.remove("vm");
        assert!(!t.has_managed_run("vm"));
        assert!(!t.in_flight("vm"));
        assert_eq!(t.effective_idle_secs("vm", 3600), 3600);
        assert_eq!(t.last("vm"), None);
        assert!(!t.finish_managed_run("vm", "run", 1, Instant::now()));
    }

    #[tokio::test]
    async fn sweep_stops_managed_vm_after_completion_not_global_hour() {
        let st = state();
        owner(&st.store);
        st.activity.set_pause_after_secs(30);
        let id = live_pair(&st, "managed");
        assert!(st.activity.set_managed_run(&id, "run", 1));
        let finished = Instant::now();
        let cfg = ThermalConfig {
            idle_secs: 3600,
            sweep_secs: 5,
        };
        st.activity
            .touch_at(&id, finished - Duration::from_secs(7200));
        let held = sweep_once(&st, cfg, finished).await;
        assert_eq!((held.paused, held.stopped), (0, 0));
        assert!(st.activity.finish_managed_run(&id, "run", 1, finished));
        assert_eq!(
            sweep_once(&st, cfg, finished + Duration::from_secs(300))
                .await
                .stopped,
            0
        );
        assert_eq!(
            sweep_once(&st, cfg, finished + Duration::from_secs(301))
                .await
                .stopped,
            1
        );
        assert_eq!(st.backend.status(&id).unwrap().state, State::Stopped);
        assert_eq!(st.store.get_sandbox(&id).unwrap().state, "stopped");
    }

    #[test]
    fn admission_and_stop_commit_exclude_each_other() {
        let t = ActivityTracker::new();
        // Op first: stop refuses while guarded.
        let _g = t.begin("a").expect("first admission");
        assert!(t.begin_stop_if_idle("a", Instant::now(), 0).is_none());
        drop(_g);
        // Released: stop commits, new ops refuse with 409-driving None.
        // Idle gate passes (no record yet is refused; touch old first).
        t.touch_at("a", Instant::now() - std::time::Duration::from_secs(100));
        let _s = t
            .begin_stop_if_idle("a", Instant::now(), 10)
            .expect("stop commits");
        assert!(t.begin("a").is_none());
        // A second stop refuses while one is committed.
        assert!(t.begin_stop_if_idle("a", Instant::now(), 10).is_none());
        drop(_s);
        // After release, admission works again.
        assert!(t.begin("a").is_some());
        // Unknown ids admit freely (ops) but never stop-commit (no evidence).
        assert!(t.begin("fresh").is_some());
        assert!(t.begin_stop_if_idle("fresh", Instant::now(), 0).is_none());
    }

    #[test]
    fn fresh_activity_inside_the_window_refuses_commit() {
        // The reported race: an operation that begins and finishes between
        // a stale idle read and the commit must still block the commit.
        let t = ActivityTracker::new();
        let now = Instant::now();
        t.touch_at("a", now - std::time::Duration::from_secs(100));
        // ...meanwhile a short op runs to completion, refreshing activity.
        t.touch_at("a", now);
        assert!(t.begin_stop_if_idle("a", now, 10).is_none());
    }

    #[tokio::test]
    async fn pause_preserves_residency_guest_resumes_and_stop_still_expires() {
        let st = state();
        owner(&st.store);
        st.activity.set_pause_after_secs(30);
        let id = live_pair(&st, "pause");
        let now = Instant::now();
        let cfg = ThermalConfig {
            idle_secs: 3600,
            sweep_secs: 5,
        };
        st.activity.touch_at(&id, now - Duration::from_secs(31));
        let attached = st.activity.begin(&id).unwrap();
        assert_eq!(sweep_once(&st, cfg, now).await.paused, 0);
        assert_eq!(st.backend.status(&id).unwrap().state, State::Running);
        drop(attached);
        st.activity.touch_at(&id, now - Duration::from_secs(31));
        assert_eq!(sweep_once(&st, cfg, now).await.paused, 1);
        assert_eq!(st.store.get_sandbox(&id).unwrap().state, "paused");
        assert_eq!(st.backend.status(&id).unwrap().state, State::Paused);
        assert_eq!(sweep_once(&st, cfg, now).await.paused, 0);
        assert_eq!(st.backend.status(&id).unwrap().state, State::Paused);
        let guest = crate::routes::guest(&st, &id).await.unwrap();
        assert_eq!(st.backend.status(&id).unwrap().state, State::Running);
        assert_eq!(st.store.get_sandbox(&id).unwrap().state, "running");
        drop(guest);
        st.activity.touch_at(&id, now - Duration::from_secs(31));
        assert_eq!(sweep_once(&st, cfg, now).await.paused, 1);
        st.activity.touch_at(&id, now - Duration::from_secs(3601));
        assert_eq!(sweep_once(&st, cfg, now).await.stopped, 1);
        assert_eq!(st.backend.status(&id).unwrap().state, State::Stopped);
    }

    #[test]
    fn changed_policy_invalidates_pending_pause_commit() {
        let activity = ActivityTracker::new();
        let now = Instant::now();
        activity.touch_at("vm", now - Duration::from_secs(100));
        activity.set_pause_after_secs(30);
        activity.set_pause_after_secs(0);
        assert!(activity.begin_pause_if_idle("vm", now, 30).is_none());
        activity.set_pause_after_secs(60);
        assert!(activity.begin_pause_if_idle("vm", now, 30).is_none());
        let guard = activity.begin_pause_if_idle("vm", now, 60).unwrap();
        assert!(activity.begin("vm").is_none());
        drop(guard);
        assert!(activity.begin("vm").is_some());
    }

    #[tokio::test]
    async fn idle_policy_is_admin_only_validated_and_persisted() {
        use axum::{extract::State as Extract, Extension, Json};
        let st = state();
        let call = |user: &str, seconds| {
            set_policy(
                Extract(st.clone()),
                Extension(crate::auth::UserId(user.into())),
                Json(IdlePolicyUpdate {
                    pause_after_secs: Some(seconds),
                    agent_idle_stop_secs: None,
                }),
            )
        };
        assert!(matches!(
            call("u", 30).await,
            Err(crate::ApiError::Forbidden(_))
        ));
        for seconds in [1, 4, 86401, u64::MAX] {
            assert!(matches!(
                call("admin", seconds).await,
                Err(crate::ApiError::Invalid(_))
            ));
        }
        for seconds in [30, 60, 0] {
            let _ = call("admin", seconds).await.unwrap();
            assert_eq!(st.store.pause_after_secs().unwrap(), Some(seconds));
            assert_eq!(st.activity.pause_after_secs(), seconds);
        }
    }

    #[test]
    fn changed_agent_policy_is_authoritative_at_stop_commit() {
        let t = ActivityTracker::new();
        let now = Instant::now();
        assert!(t.restore_managed_cooldown("vm", now));
        let stale = t.effective_idle_secs("vm", 3600);
        assert_eq!(stale, 300);
        t.set_idle_policy(IdlePolicy {
            pause_after_secs: 30,
            agent_idle_stop_secs: 600,
        });
        // A sweep that sampled 300 before the admin edit cannot stop at 301.
        assert!(t
            .begin_stop_if_idle("vm", now + Duration::from_secs(301), stale)
            .is_none());
        assert_eq!(t.effective_idle_secs("vm", 3600), 600);
        assert_eq!(t.effective_idle_secs("ordinary", 3600), 3600);
        t.set_idle_policy(IdlePolicy {
            pause_after_secs: 30,
            agent_idle_stop_secs: 60,
        });
        let shell = t.begin("vm").unwrap();
        assert!(t
            .begin_stop_if_idle("vm", now + Duration::from_secs(601), 600)
            .is_none());
        drop(shell);
        // Newly configured timeout applies to already-completed managed VMs.
        assert!(t
            .begin_stop_if_idle("vm", now + Duration::from_secs(601), 600)
            .is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn partial_agent_policy_updates_validate_and_preserve_other_fields() {
        use axum::{extract::State as Extract, Extension, Json};
        let st = state();
        let call = |user: &str, pause, stop| {
            set_policy(
                Extract(st.clone()),
                Extension(crate::auth::UserId(user.into())),
                Json(IdlePolicyUpdate {
                    pause_after_secs: pause,
                    agent_idle_stop_secs: stop,
                }),
            )
        };
        assert!(matches!(
            call("u", None, Some(60)).await,
            Err(crate::ApiError::Forbidden(_))
        ));
        assert!(matches!(
            call("admin", None, None).await,
            Err(crate::ApiError::Invalid(_))
        ));
        for seconds in [0, 1, 59, 86401, u64::MAX] {
            assert!(matches!(
                call("admin", Some(90), Some(seconds)).await,
                Err(crate::ApiError::Invalid(_))
            ));
            assert_eq!(st.store.pause_after_secs().unwrap(), None);
            assert_eq!(st.store.agent_idle_stop_secs().unwrap(), None);
        }
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let mut updates = Vec::new();
        for (pause, stop) in [(Some(45), None), (None, Some(120))] {
            let st = st.clone();
            let barrier = barrier.clone();
            updates.push(tokio::spawn(async move {
                barrier.wait().await;
                set_policy(
                    Extract(st),
                    Extension(crate::auth::UserId("admin".into())),
                    Json(IdlePolicyUpdate {
                        pause_after_secs: pause,
                        agent_idle_stop_secs: stop,
                    }),
                )
                .await
                .unwrap()
                .0
            }));
        }
        for task in updates {
            task.await.unwrap();
        }
        assert_eq!(
            st.activity.idle_policy(),
            IdlePolicy {
                pause_after_secs: 45,
                agent_idle_stop_secs: 120
            }
        );
        assert_eq!(st.store.pause_after_secs().unwrap(), Some(45));
        assert_eq!(st.store.agent_idle_stop_secs().unwrap(), Some(120));
        for seconds in [60, 300, 86400] {
            let saved = call("admin", None, Some(seconds)).await.unwrap().0;
            assert_eq!(saved.pause_after_secs, 45);
            assert_eq!(saved.agent_idle_stop_secs, seconds);
        }
        let read = policy(
            Extract(st.clone()),
            Extension(crate::auth::UserId("admin".into())),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(read, st.activity.idle_policy());
        assert!(policy(
            Extract(st.clone()),
            Extension(crate::auth::UserId("u".into()))
        )
        .await
        .is_err());
        assert!(serde_json::from_str::<IdlePolicyUpdate>(r#"{"agent_idle_stop_sec":60}"#).is_err());
        assert!(
            serde_json::from_str::<IdlePolicyUpdate>(r#"{"agent_idle_stop_secs":-1}"#).is_err()
        );
    }

    fn state() -> AppState {
        let store = Arc::new(ahvm_store::Store::open_in_memory().unwrap());
        let backend: Arc<dyn Backend> = Arc::new(MockBackend::new(
            std::env::temp_dir().join(format!("thermal-{}", std::process::id())),
        ));
        AppState {
            private_owners: Default::default(),
            store,
            backend,
            quotas: crate::quotas::Registry::new(),
            activity: ActivityTracker::new(),
            ops: crate::scheduler::OpsLimiter::new(4),
            lifecycle: crate::scheduler::LifecycleLocks::new(),
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
            storage_mode: None,
            name: name.to_string(),
            cpus: 1,
            memory_mb: 512,
            backend: ahvm_engine::BackendKind::Krucible,
            root_image: None,
            kernel_image: None,
            desktop: false,
            desktop_gpu: false,
            network_bytes_per_sec: None,
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

    /// Scripted backend: `status` parks until released, then reports the
    /// programmed state. Lets the test force the exact interleaving of the
    /// reported race (sweep samples stale, foreground start commits).
    struct GateBackend {
        entered: std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>>,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
        stops: std::sync::Mutex<Vec<String>>,
    }

    impl std::fmt::Debug for GateBackend {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("GateBackend").finish_non_exhaustive()
        }
    }

    impl GateBackend {
        fn new() -> (
            Self,
            std::sync::mpsc::Receiver<()>,
            std::sync::mpsc::Sender<()>,
        ) {
            let (entered_tx, entered_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            (
                Self {
                    entered: std::sync::Mutex::new(Some(entered_tx)),
                    release: std::sync::Mutex::new(release_rx),
                    stops: std::sync::Mutex::new(Vec::new()),
                },
                entered_rx,
                release_tx,
            )
        }
    }

    fn stopped_info(id: &str) -> SandboxInfo {
        SandboxInfo {
            storage: Default::default(),
            id: id.to_string(),
            name: id.to_string(),
            state: State::Stopped,
            thermal: ahvm_engine::Thermal::Cold,
            ip: String::new(),
        }
    }

    impl ahvm_engine::Backend for GateBackend {
        fn capabilities(&self) -> ahvm_engine::Capabilities {
            ahvm_engine::BackendKind::Krucible.capabilities()
        }
        fn create(&self, _spec: &ahvm_engine::SandboxSpec) -> ahvm_engine::Result<SandboxInfo> {
            Err(ahvm_engine::Error::NotFound("gate".into()))
        }
        fn destroy(&self, _id: &str) -> ahvm_engine::Result<()> {
            Err(ahvm_engine::Error::NotFound("gate".into()))
        }
        fn start(&self, _id: &str) -> ahvm_engine::Result<()> {
            Err(ahvm_engine::Error::NotFound("gate".into()))
        }
        fn stop(&self, id: &str) -> ahvm_engine::Result<()> {
            self.stops.lock().unwrap().push(id.to_string());
            Ok(())
        }
        fn status(&self, id: &str) -> ahvm_engine::Result<SandboxInfo> {
            // Park until the driver releases, then report stale Stopped:
            // without lifecycle serialization the sweep writes this over
            // a newer Running commit.
            if let Some(tx) = self.entered.lock().unwrap().take() {
                let _ = tx.send(());
            }
            let _ = self.release.lock().unwrap().recv();
            Ok(stopped_info(id))
        }
        fn list(&self) -> ahvm_engine::Result<Vec<SandboxInfo>> {
            Ok(vec![])
        }
        fn exec(
            &self,
            _id: &str,
            _argv: &[String],
        ) -> ahvm_engine::Result<ahvm_engine::ExecResult> {
            Err(ahvm_engine::Error::NotFound("gate".into()))
        }
        fn create_snapshot(
            &self,
            _id: &str,
            _snapshot_id: &str,
        ) -> ahvm_engine::Result<ahvm_engine::SnapshotManifest> {
            Err(ahvm_engine::Error::NotFound("gate".into()))
        }
        fn restore(
            &self,
            _snapshot: &ahvm_engine::SnapshotManifest,
            _new_id: &str,
        ) -> ahvm_engine::Result<SandboxInfo> {
            Err(ahvm_engine::Error::NotFound("gate".into()))
        }
        fn fork(&self, _id: &str, _new_id: &str) -> ahvm_engine::Result<SandboxInfo> {
            Err(ahvm_engine::Error::NotFound("gate".into()))
        }
        fn snapshot_manifest(
            &self,
            snapshot_id: &str,
        ) -> ahvm_engine::Result<ahvm_engine::SnapshotManifest> {
            Err(ahvm_engine::Error::NotFound(format!(
                "snapshot {snapshot_id}"
            )))
        }
        fn file_read(
            &self,
            _id: &str,
            _path: &str,
            _offset: u64,
            _limit: u64,
        ) -> ahvm_engine::Result<ahvm_engine::FileChunk> {
            Err(ahvm_engine::Error::NotFound("gate".into()))
        }
        fn file_write(&self, _id: &str, _path: &str, _data: &[u8]) -> ahvm_engine::Result<u64> {
            Err(ahvm_engine::Error::NotFound("gate".into()))
        }
        fn file_list(
            &self,
            _id: &str,
            _path: &str,
            _offset: u64,
            _limit: u64,
        ) -> ahvm_engine::Result<ahvm_engine::DirListing> {
            Err(ahvm_engine::Error::NotFound("gate".into()))
        }
        fn session_create(
            &self,
            _id: &str,
            _argv: &[String],
            _pty: bool,
        ) -> ahvm_engine::Result<String> {
            Err(ahvm_engine::Error::NotFound("gate".into()))
        }
        fn session_read(
            &self,
            _id: &str,
            _session_id: &str,
            from_seq: u64,
            _budget: Duration,
        ) -> ahvm_engine::Result<ahvm_engine::SessionChunk> {
            std::thread::sleep(Duration::from_millis(20));
            Ok(ahvm_engine::SessionChunk {
                data: vec![],
                eof: false,
                exit_code: None,
                next_seq: from_seq,
                truncated: false,
            })
        }
        fn session_input(
            &self,
            _id: &str,
            _session_id: &str,
            _data: &[u8],
        ) -> ahvm_engine::Result<u64> {
            Err(ahvm_engine::Error::NotFound("gate".into()))
        }
        fn session_kill(&self, _id: &str, _session_id: &str) -> ahvm_engine::Result<()> {
            Err(ahvm_engine::Error::NotFound("gate".into()))
        }
        fn session_delete(&self, _id: &str, _session_id: &str) -> ahvm_engine::Result<()> {
            Err(ahvm_engine::Error::NotFound("gate".into()))
        }
        fn session_list(&self, _id: &str) -> ahvm_engine::Result<Vec<ahvm_engine::SessionInfo>> {
            Ok(vec![ahvm_engine::SessionInfo {
                id: "shell".into(),
                argv: vec!["bash".into()],
                running: true,
                started_at: 0,
            }])
        }
        fn session_resize(
            &self,
            _id: &str,
            _session_id: &str,
            _rows: u16,
            _cols: u16,
        ) -> ahvm_engine::Result<()> {
            Err(ahvm_engine::Error::NotFound("gate".into()))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connected_quiet_shell_blocks_idle_stop_until_disconnect() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let mut st = state();
        owner(&st.store);
        st.store
            .create_sandbox(&row("attached", "running", "hot"))
            .unwrap();
        st.backend = Arc::new(GateBackend::new().0);
        let router = axum::Router::new()
            .route(
                "/v1/sandboxes/{id}/sessions/{sid}/stream",
                axum::routing::get(crate::sessions::stream),
            )
            .layer(axum::Extension(crate::auth::UserId("u".into())))
            .with_state(st.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let (mut socket, _) = tokio_tungstenite::connect_async(format!(
            "ws://{address}/v1/sandboxes/attached/sessions/shell/stream"
        ))
        .await
        .unwrap();
        let message = tokio::time::timeout(Duration::from_secs(2), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(message, Message::Ping(_)));
        socket.flush().await.unwrap();
        st.activity
            .touch_at("attached", Instant::now() - Duration::from_secs(7200));
        assert!(
            st.activity
                .begin_stop_if_idle("attached", Instant::now(), 3600)
                .is_none(),
            "attached shell was idle-stopped"
        );
        socket.close(None).await.unwrap();
        drop(socket);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if st
                    .activity
                    .begin_stop_if_idle("attached", Instant::now(), 0)
                    .is_some()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(st.activity.last("attached").unwrap().elapsed() < Duration::from_secs(2));
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sweep_cannot_overwrite_newer_lifecycle_commit() {
        // Exact repro of the reported race: the sweep samples Stopped while
        // a foreground start commits Running. The per-id lock must serialize
        // the two commits; whoever holds it first wins the race to write,
        // and the start's write lands last. Without the sweep-side lock the
        // foreground write slips through first and the assertion fails.
        let store = Arc::new(ahvm_store::Store::open_in_memory().unwrap());
        owner(&store);
        store
            .create_sandbox(&row("race", "running", "hot"))
            .unwrap();
        let (gate, entered, release) = GateBackend::new();
        let lifecycle = crate::scheduler::LifecycleLocks::new();
        let mk_state = |backend: Arc<dyn Backend>| AppState {
            private_owners: Default::default(),
            store: store.clone(),
            backend,
            quotas: crate::quotas::Registry::new(),
            activity: ActivityTracker::new(),
            ops: crate::scheduler::OpsLimiter::new(4),
            lifecycle: lifecycle.clone(),
        };
        let gate = Arc::new(gate);
        let sweep_state = mk_state(gate);
        let sweep = tokio::spawn(async move {
            sweep_once(
                &sweep_state,
                ThermalConfig {
                    idle_secs: 3600,
                    sweep_secs: 60,
                },
                Instant::now(),
            )
            .await
        });
        // Wait until the sweep is parked inside status() holding the id lock.
        entered.recv().unwrap();
        // Foreground start through the same lock (mirrors set_running):
        // it must NOT complete while the sweep holds the lock.
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let fg_store = store.clone();
        let fg_lifecycle = lifecycle.clone();
        tokio::spawn(async move {
            let _lc = fg_lifecycle.lock("race").await;
            fg_store
                .set_sandbox_state("race", "running", "warm", 1)
                .unwrap();
            let _ = done_tx.send(());
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(200), done_rx)
                .await
                .is_err(),
            "foreground commit slipped past the sweep's lock"
        );
        // Release the sweep: it writes its stale Stopped, drops the lock,
        // then the foreground write lands last. Final state must be Running.
        release.send(()).unwrap();
        let stats = sweep.await.unwrap();
        assert_eq!(stats.checked, 1);
        // The foreground write must complete promptly once unblocked.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let row = store.get_sandbox("race").unwrap();
                if row.state == "running" && row.thermal == "warm" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("foreground commit never landed");
        assert_eq!(
            store.get_sandbox("race").unwrap().state,
            "running",
            "stale sweep sample overwrote the newer start commit"
        );
    }
}

#[test]
fn stale_stream_completion_does_not_touch_recreated_sandbox() {
    let tracker = ActivityTracker::new();
    let old = tracker.begin("id").unwrap();
    tracker.remove("id");
    let new = tracker.begin("id").unwrap();
    drop(old);
    assert!(tracker.in_flight("id"));
    assert!(tracker.last("id").is_none());
    drop(new);
    assert!(!tracker.in_flight("id"));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct IdlePolicy {
    pub pause_after_secs: u64,
    pub agent_idle_stop_secs: u64,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdlePolicyUpdate {
    pub pause_after_secs: Option<u64>,
    pub agent_idle_stop_secs: Option<u64>,
}
pub async fn policy(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::Extension(user): axum::Extension<crate::auth::UserId>,
) -> crate::ApiResult<axum::Json<IdlePolicy>> {
    if user.0 != "admin" {
        return Err(crate::ApiError::Forbidden(
            "host administrator required".into(),
        ));
    }
    Ok(axum::Json(state.activity.idle_policy()))
}
pub async fn set_policy(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::Extension(user): axum::Extension<crate::auth::UserId>,
    axum::Json(update): axum::Json<IdlePolicyUpdate>,
) -> crate::ApiResult<axum::Json<IdlePolicy>> {
    if user.0 != "admin" {
        return Err(crate::ApiError::Forbidden(
            "host administrator required".into(),
        ));
    }
    if update.pause_after_secs.is_none() && update.agent_idle_stop_secs.is_none() {
        return Err(crate::ApiError::Invalid(
            "at least one idle policy field is required".into(),
        ));
    }
    if update
        .pause_after_secs
        .is_some_and(|seconds| seconds != 0 && !(5..=86400).contains(&seconds))
    {
        return Err(crate::ApiError::Invalid(
            "pause_after_secs must be 0 (disabled) or 5..86400".into(),
        ));
    }
    if update
        .agent_idle_stop_secs
        .is_some_and(|seconds| !(60..=86400).contains(&seconds))
    {
        return Err(crate::ApiError::Invalid(
            "agent_idle_stop_secs must be 60..86400".into(),
        ));
    }
    let _update = state.activity.policy_updates.lock().await;
    let current = state.activity.idle_policy();
    let policy = IdlePolicy {
        pause_after_secs: update.pause_after_secs.unwrap_or(current.pause_after_secs),
        agent_idle_stop_secs: update
            .agent_idle_stop_secs
            .unwrap_or(current.agent_idle_stop_secs),
    };
    // Persist both values atomically before publishing the in-memory policy.
    // There is no await between the database commit and publication.
    state
        .store
        .set_idle_policy(policy.pause_after_secs, policy.agent_idle_stop_secs)?;
    state.activity.set_idle_policy(policy);
    Ok(axum::Json(policy))
}

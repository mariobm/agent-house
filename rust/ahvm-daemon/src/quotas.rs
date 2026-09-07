//! Atomic quota enforcement: committed store rows plus in-flight holds.
//!
//! The race: two concurrent creates both pass a count check, then both
//! boot. Holds close it: the check (committed + held) and the hold insert
//! happen under one daemon-side lock, so the second creator always sees
//! the first's hold. Committed rows cover everything else; a hold lives
//! only until its operation commits its record or fails.
//!
//! Single-daemon assumption is explicit: two daemons sharing one data dir
//! would need store-level serialization (the store offers IMMEDIATE
//! transactions for exactly that future). Crash recovery cannot strand a
//! hold (holds are memory-only), and startup destroys backend sandboxes
//! with no store row, so accounting has no invisible consumers.

use crate::{ApiError, ApiResult};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
struct SandboxHold {
    owner: String,
    cpus: i64,
    mem_mb: i64,
}

#[derive(Debug, Default)]
struct Inner {
    sandboxes: HashMap<String, SandboxHold>,
    snapshots: HashSet<(String, String)>,
}

/// Quota holds registry. Clone shares the registry (one per daemon).
#[derive(Debug, Clone, Default)]
pub struct Registry {
    inner: Arc<Mutex<Inner>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reserve sandbox capacity for a create/restore. Fails fast with
    /// Conflict (id already creating) or Forbidden (quota exceeded).
    pub fn reserve_sandbox(
        &self,
        store: &ahvm_store::Store,
        user: &ahvm_store::User,
        id: &str,
        cpus: i64,
        mem_mb: i64,
    ) -> ApiResult<SandboxGuard<'_>> {
        let mut inner = self.inner.lock().expect("quota mutex poisoned");
        if inner.sandboxes.contains_key(id) {
            return Err(ApiError::Conflict(format!(
                "sandbox {id} is already being created"
            )));
        }
        // Committed rows: every non-failed sandbox counts against the
        // sandbox quota; creating + running count CPU/RAM (stopped holds
        // disk only, failed holds nothing until destroyed).
        let rows = store.list_sandboxes(&user.id, None, 100_000)?;
        let mut count = 0i64;
        let mut cpu_sum = 0i64;
        let mut mem_sum = 0i64;
        for row in &rows {
            if row.state != "failed" {
                count += 1;
            }
            if row.state == "running" || row.state == "creating" {
                cpu_sum += row.cpus;
                mem_sum += row.memory_mb;
            }
        }
        for hold in inner.sandboxes.values().filter(|h| h.owner == user.id) {
            count += 1;
            cpu_sum += hold.cpus;
            mem_sum += hold.mem_mb;
        }
        if count + 1 > user.max_sandboxes {
            return Err(ApiError::Forbidden(format!(
                "sandbox quota exceeded (max {})",
                user.max_sandboxes
            )));
        }
        if cpu_sum + cpus > user.max_cpus {
            return Err(ApiError::Forbidden(format!(
                "cpu quota exceeded (max {})",
                user.max_cpus
            )));
        }
        if mem_sum + mem_mb > user.max_memory_mb {
            return Err(ApiError::Forbidden(format!(
                "memory quota exceeded (max {} MB)",
                user.max_memory_mb
            )));
        }
        inner.sandboxes.insert(
            id.to_string(),
            SandboxHold {
                owner: user.id.clone(),
                cpus,
                mem_mb,
            },
        );
        Ok(SandboxGuard {
            registry: self,
            id: id.to_string(),
        })
    }

    /// Reserve a snapshot name. Same race, same discipline as sandboxes.
    pub fn reserve_snapshot(
        &self,
        store: &ahvm_store::Store,
        user: &ahvm_store::User,
        name: &str,
    ) -> ApiResult<SnapshotGuard<'_>> {
        let mut inner = self.inner.lock().expect("quota mutex poisoned");
        if inner
            .snapshots
            .contains(&(user.id.clone(), name.to_string()))
        {
            return Err(ApiError::Conflict(format!(
                "snapshot {name} is already being created"
            )));
        }
        let committed = store.count_snapshots(&user.id)?;
        let held = inner
            .snapshots
            .iter()
            .filter(|(owner, _)| owner == &user.id)
            .count() as i64;
        if committed + held + 1 > user.max_snapshots {
            return Err(ApiError::Forbidden(format!(
                "snapshot quota exceeded (max {})",
                user.max_snapshots
            )));
        }
        inner.snapshots.insert((user.id.clone(), name.to_string()));
        Ok(SnapshotGuard {
            registry: self,
            owner: user.id.clone(),
            name: name.to_string(),
        })
    }
}

/// Released on drop (including panic paths, best-effort via try_lock).
#[derive(Debug)]
pub struct SandboxGuard<'a> {
    registry: &'a Registry,
    id: String,
}

impl Drop for SandboxGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.registry.inner.try_lock() {
            inner.sandboxes.remove(&self.id);
        }
    }
}

/// Released on drop (including panic paths, best-effort via try_lock).
#[derive(Debug)]
pub struct SnapshotGuard<'a> {
    registry: &'a Registry,
    owner: String,
    name: String,
}

impl Drop for SnapshotGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.registry.inner.try_lock() {
            inner
                .snapshots
                .remove(&(self.owner.clone(), self.name.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(id: &str, max_sb: i64, max_cpu: i64, max_mem: i64, max_snap: i64) -> ahvm_store::User {
        ahvm_store::User {
            id: id.to_string(),
            name: id.to_string(),
            api_key_hash: "x".to_string(),
            max_sandboxes: max_sb,
            max_cpus: max_cpu,
            max_memory_mb: max_mem,
            max_volumes_mb: 0,
            max_snapshots: max_snap,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn sandbox_row(owner: &str, id: &str, state: &str, cpus: i64, mem: i64) -> ahvm_store::Sandbox {
        ahvm_store::Sandbox {
            id: id.to_string(),
            owner_user_id: owner.to_string(),
            name: id.to_string(),
            backend: ahvm_store::Backend::Krucible,
            state: state.to_string(),
            thermal: "hot".to_string(),
            cpus,
            memory_mb: mem,
            ip: String::new(),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn duplicate_in_flight_id_conflicts() {
        let store = ahvm_store::Store::open_in_memory().unwrap();
        let reg = Registry::new();
        let u = user("u", 8, 32, 65536, 8);
        let _g = reg.reserve_sandbox(&store, &u, "sb-1", 1, 512).unwrap();
        assert!(matches!(
            reg.reserve_sandbox(&store, &u, "sb-1", 1, 512),
            Err(ApiError::Conflict(_))
        ));
    }

    #[test]
    fn committed_rows_count_against_all_quotas() {
        let store = ahvm_store::Store::open_in_memory().unwrap();
        store.upsert_user(&user("u", 8, 32, 65536, 8)).unwrap();
        let reg = Registry::new();
        // One running 2-CPU box against tight limits.
        store
            .create_sandbox(&sandbox_row("u", "old", "running", 2, 1024))
            .unwrap();
        let u = user("u", 8, 2, 1024, 8);
        assert!(
            matches!(
                reg.reserve_sandbox(&store, &u, "new", 1, 512),
                Err(ApiError::Forbidden(_))
            ),
            "cpu quota must count committed running boxes"
        );
        let u = user("u", 1, 32, 65536, 8);
        assert!(
            matches!(
                reg.reserve_sandbox(&store, &u, "new", 1, 512),
                Err(ApiError::Forbidden(_))
            ),
            "sandbox count must count committed rows"
        );
        // Failed rows hold no quota: recovery never needs a destroy first.
        store
            .create_sandbox(&sandbox_row("u", "dead", "failed", 8, 32768))
            .unwrap();
        let u = user("u", 2, 32, 65536, 8);
        assert!(reg.reserve_sandbox(&store, &u, "new", 1, 512).is_ok());
    }

    #[test]
    fn snapshot_names_conflict_and_count() {
        let store = ahvm_store::Store::open_in_memory().unwrap();
        let reg = Registry::new();
        let u = user("u", 8, 32, 65536, 1);
        let _g = reg.reserve_snapshot(&store, &u, "s1").unwrap();
        assert!(matches!(
            reg.reserve_snapshot(&store, &u, "s1"),
            Err(ApiError::Conflict(_))
        ));
        assert!(
            matches!(
                reg.reserve_snapshot(&store, &u, "s2"),
                Err(ApiError::Forbidden(_))
            ),
            "in-flight hold counts against the snapshot quota"
        );
    }
}

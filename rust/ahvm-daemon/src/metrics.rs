//! Bounded host-only telemetry. Never reconciles, wakes, or contacts a VM.
use crate::{auth::UserId, ApiError, ApiResult, AppState};
use axum::{extract::State, Extension, Json};
use serde_json::{json, Value};
pub async fn sample(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
) -> ApiResult<Json<Value>> {
    if user.0 != "admin" {
        return Err(ApiError::Forbidden("admin required".into()));
    }
    tokio::task::spawn_blocking(move || {
        let rows = state.store.list_all_sandboxes(None, 101)?;
        let truncated = rows.len() > 100;
        let vms: Vec<_> = rows.into_iter().take(100).map(|row| json!({"id": row.id, "usage": state.backend.resource_usage(&row.id)})).collect();
        Ok(Json(json!({"protocol":1,"sampled_at_ms":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis(),"host":host(),"vms":vms,"truncated":truncated})))
    }).await.map_err(|_| ApiError::Internal("metrics task failed".into()))?
}
fn host() -> Option<Value> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let memory = std::fs::read_to_string("/proc/meminfo").ok()?;
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    parse_host(&stat, &memory, boot.trim())
}
fn parse_host(stat: &str, memory: &str, boot: &str) -> Option<Value> {
    let fields: Vec<u64> = stat
        .lines()
        .next()?
        .strip_prefix("cpu ")?
        .split_whitespace()
        .take(8)
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    if fields.len() != 8 {
        return None;
    }
    let total = fields.iter().try_fold(0u64, |a, b| a.checked_add(*b))?;
    let idle = fields[3].checked_add(fields[4])?;
    let mem = |key: &str| {
        memory
            .lines()
            .find_map(|line| {
                line.strip_prefix(key)?
                    .split_whitespace()
                    .next()?
                    .parse::<u64>()
                    .ok()
            })?
            .checked_mul(1024)
    };
    let total_bytes = mem("MemTotal:")?;
    let available = mem("MemAvailable:")?;
    Some(
        json!({"generation":boot,"cpu_total":total,"cpu_busy":total.checked_sub(idle)?,"memory_bytes":total_bytes.checked_sub(available)?,"memory_total_bytes":total_bytes}),
    )
}
/// Unlike the interactive storage route, this does not take a lifecycle lock.
pub async fn backlog(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<Json<Value>> {
    if user.0 != "admin" {
        return Err(ApiError::Forbidden("admin required".into()));
    }
    tokio::task::spawn_blocking(move || {
        Ok(Json(
            json!({"pending_bytes":state.backend.replication_backlog(&id)}),
        ))
    })
    .await
    .map_err(|_| ApiError::Internal("metrics task failed".into()))?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn counters_exclude_guest_double_count_and_use_available_memory() {
        let v = parse_host(
            "cpu  10 0 10 70 10 0 0 0 5 0\n",
            "MemTotal: 100 kB\nMemAvailable: 25 kB\n",
            "boot",
        )
        .unwrap();
        assert_eq!(v["cpu_total"], 100);
        assert_eq!(v["cpu_busy"], 20);
        assert_eq!(v["memory_bytes"], 76800);
        assert!(parse_host("bad", "", "").is_none());
    }
}

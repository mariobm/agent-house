//! Shared route plumbing: per-request sandbox ownership.

use crate::{ApiError, ApiResult, AppState};

/// Store row for a sandbox the authed user owns (else 404).
pub async fn owned(state: &AppState, user: &str, id: &str) -> ApiResult<ahvm_store::Sandbox> {
    match state.store.get_sandbox(id) {
        Ok(row) if row.owner_user_id == user => Ok(row),
        Ok(_) => Err(ApiError::NotFound(format!("sandbox {id}"))),
        Err(ahvm_store::Error::NotFound(_)) => Err(ApiError::NotFound(format!("sandbox {id}"))),
        Err(e) => Err(e.into()),
    }
}

/// A reserved name cannot be claimed by a different owner after deletion.
pub fn reserved_owner(state: &AppState, user: &str, id: &str) -> ApiResult<()> {
    if state
        .private_owners
        .get(id)
        .is_some_and(|owner| owner != user)
    {
        return Err(ApiError::Forbidden(
            "sandbox name reserved by host policy".into(),
        ));
    }
    Ok(())
}

/// Serialize admission with idle transitions, then keep activity protected for
/// the operation's lifetime. Running VMs need no VMM control or storage RPC.
pub async fn guest(
    state: &AppState,
    user: &str,
    id: &str,
) -> ApiResult<std::sync::Arc<crate::thermal::InFlight>> {
    admit(state, user, id, true, None).await
}

/// Passive session inspection/cleanup must not wake an idle VM.
pub async fn guest_passive(
    state: &AppState,
    user: &str,
    id: &str,
) -> ApiResult<std::sync::Arc<crate::thermal::InFlight>> {
    admit(state, user, id, false, None).await
}

/// Preview grants carry their immutable port generation across authorization
/// and admission. Validate before waking or dispatching to any guest worker.
pub async fn guest_preview(
    state: &AppState,
    user: &str,
    id: &str,
    port: u16,
    generation: &[u8],
) -> ApiResult<std::sync::Arc<crate::thermal::InFlight>> {
    admit(state, user, id, true, Some((port, generation))).await
}

async fn admit(
    state: &AppState,
    user: &str,
    id: &str,
    wake: bool,
    preview: Option<(u16, &[u8])>,
) -> ApiResult<std::sync::Arc<crate::thermal::InFlight>> {
    let lifecycle = state.lifecycle.lock(id).await;
    unchanged_identity(&lifecycle)?;
    owned(state, user, id).await?;
    if let Some((port, expected)) = preview {
        if state.store.preview_generation(id, port)?.as_deref() != Some(expected) {
            return Err(ApiError::Unauthorized);
        }
    }
    let flight = state
        .activity
        .begin(id)
        .ok_or_else(|| ApiError::Conflict("sandbox is transitioning".into()))?;
    let backend = state.backend.clone();
    let key = id.to_owned();
    let store = state.store.clone();
    tokio::task::spawn_blocking(move || -> ApiResult<_> {
        // Cancellation must not release admission while control I/O still runs.
        let _lifecycle = lifecycle;
        let resumed = if wake {
            backend.resume_paused(&key)?
        } else {
            None
        };
        if let Some(info) = resumed {
            store.set_sandbox_state(
                &key,
                &crate::state_str(&info.state),
                &crate::thermal_str(&info.thermal),
                crate::unix_now(),
            )?;
        }
        Ok(std::sync::Arc::new(flight))
    })
    .await
    .map_err(|e| ApiError::Internal(format!("admission task: {e}")))?
}

/// Reuse waits until all old guest RPCs and transports have actually unwound.
/// Called while holding this name's lifecycle lock.
pub fn identity_available(state: &AppState, id: &str) -> ApiResult<()> {
    crate::runs::check_lifecycle(state, id)?;
    if state.activity.in_flight(id) {
        return Err(ApiError::Conflict(
            "previous sandbox operations are still active".into(),
        ));
    }
    Ok(())
}
pub fn unchanged_identity(guard: &crate::scheduler::LifecycleGuard) -> ApiResult<()> {
    if guard.identity_changed() {
        return Err(ApiError::Conflict(
            "sandbox identity changed while queued; retry".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod security_tests {
    use super::*;
    use ahvm_engine::MockBackend;
    use ahvm_store::{Store, User};
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::sync::Arc;
    use tower::ServiceExt;

    fn fixture() -> (AppState, axum::Router) {
        let store = Arc::new(Store::open_in_memory().unwrap());
        for id in ["alice", "bob"] {
            store
                .upsert_user(&User {
                    id: id.into(),
                    name: id.into(),
                    api_key_hash: hex::encode(Sha256::digest(id.as_bytes())),
                    max_sandboxes: 4,
                    max_cpus: 8,
                    max_memory_mb: 2048,
                    max_volumes_mb: 2048,
                    max_snapshots: 4,
                    created_at: 1,
                    updated_at: 1,
                })
                .unwrap();
        }
        let dir = std::env::temp_dir().join(format!(
            "ahvm-admission-{}-{}",
            std::process::id(),
            crate::unix_now()
        ));
        let state = AppState {
            private_owners: Arc::default(),
            store,
            backend: Arc::new(MockBackend::new(&dir)),
            quotas: crate::quotas::Registry::new(),
            activity: crate::thermal::ActivityTracker::new(),
            ops: crate::scheduler::OpsLimiter::new(4),
            lifecycle: crate::scheduler::LifecycleLocks::new(),
        };
        (state.clone(), crate::build_router(state))
    }
    fn request(user: &str, method: &str, uri: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("Authorization", format!("Bearer {user}"))
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }
    async fn create(app: &axum::Router, user: &str) -> StatusCode {
        app.clone()
            .oneshot(request(
                user,
                "POST",
                "/v1/sandboxes",
                json!({"name":"shared","cpus":1,"memory_mb":128}),
            ))
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn canceled_delete_permit_wait_does_not_publish_storage_deletion() {
        let (state, app) = fixture();
        assert_eq!(create(&app, "alice").await, StatusCode::CREATED);
        let reservation = state
            .store
            .reserve_replicated_volume("alice", "shared", &"a".repeat(64), 65536, crate::unix_now())
            .unwrap();
        let permits = [
            state.ops.acquire().await,
            state.ops.acquire().await,
            state.ops.acquire().await,
            state.ops.acquire().await,
        ];
        let mut deleting = Box::pin(app.oneshot(request(
            "alice",
            "DELETE",
            "/v1/sandboxes/shared",
            json!(null),
        )));
        assert!(futures_util::poll!(deleting.as_mut()).is_pending());
        assert_eq!(
            state
                .store
                .replicated_reservation("alice", &reservation.volume_id)
                .unwrap()
                .state,
            reservation.state
        );
        drop(deleting);
        drop(permits);
        assert!(state.store.get_sandbox("shared").is_ok());
        assert!(state.backend.status("shared").is_ok());
        assert_eq!(state.lifecycle.retained_entries(), 0);
    }

    #[tokio::test]
    async fn nonexistent_router_names_do_not_retain_locks() {
        let (state, app) = fixture();
        for i in 0..1000 {
            let response = app
                .clone()
                .oneshot(request(
                    "alice",
                    "GET",
                    &format!("/v1/sandboxes/missing-{i}"),
                    json!(null),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
        assert_eq!(state.lifecycle.retained_entries(), 0);
    }

    #[tokio::test]
    async fn queued_guest_and_lifecycle_requests_cannot_cross_replacement() {
        for owner in ["alice", "bob"] {
            for (method, path, body) in [
                (
                    "GET",
                    "/v1/sandboxes/shared/files?path=/private",
                    json!(null),
                ),
                (
                    "POST",
                    "/v1/sandboxes/shared/exec",
                    json!({"argv":["echo","private"]}),
                ),
                ("POST", "/v1/sandboxes/shared/stop", json!(null)),
            ] {
                let (state, app) = fixture();
                assert_eq!(create(&app, "alice").await, StatusCode::CREATED);
                let deletion = state.lifecycle.lock("shared").await;
                let mut replacement = Box::pin(create(&app, owner));
                assert!(futures_util::poll!(replacement.as_mut()).is_pending());
                let mut queued =
                    Box::pin(app.clone().oneshot(request("alice", method, path, body)));
                assert!(futures_util::poll!(queued.as_mut()).is_pending());
                deletion.invalidate_identity();
                state.backend.destroy("shared").unwrap();
                state.activity.remove("shared");
                state.store.delete_sandbox("shared").unwrap();
                drop(deletion);
                assert_eq!(replacement.await, StatusCode::CREATED);
                assert_eq!(queued.await.unwrap().status(), StatusCode::CONFLICT);
                assert_eq!(
                    state.store.get_sandbox("shared").unwrap().owner_user_id,
                    owner
                );
            }
        }
    }

    #[tokio::test]
    async fn active_guest_identity_survives_delete_and_blocks_reuse_until_last_clone() {
        let (state, app) = fixture();
        assert_eq!(create(&app, "alice").await, StatusCode::CREATED);
        let admission = guest(&state, "alice", "shared").await.unwrap();
        let detached_worker = admission.clone();
        let deletion = app
            .clone()
            .oneshot(request(
                "alice",
                "DELETE",
                "/v1/sandboxes/shared",
                json!(null),
            ))
            .await
            .unwrap();
        assert_eq!(deletion.status(), StatusCode::NO_CONTENT);
        assert_eq!(create(&app, "bob").await, StatusCode::CONFLICT);
        drop(admission);
        assert_eq!(create(&app, "bob").await, StatusCode::CONFLICT);
        drop(detached_worker);
        assert_eq!(create(&app, "bob").await, StatusCode::CREATED);
        assert!(guest(&state, "alice", "shared").await.is_err());
    }
}

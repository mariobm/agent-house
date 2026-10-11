//! Explicit clean builders and immutable project images. These admin routes
//! accept control-plane provenance, never guest-selected source VMs or paths.
use crate::{auth::UserId, operations, unix_now, ApiError, ApiResult, AppState};
use ahvm_engine::{
    project_digest, project_uuid, ProjectImageBaseStatus, ProjectImageInfo, ProjectImageMetadata,
    ProjectImageSelection, State as VmState, StorageMode, PROJECT_IMAGE_HEADROOM_BYTES,
    PROJECT_IMAGE_MAX_BYTES, PROJECT_IMAGE_TRAILER_BYTES,
};
use ahvm_store::{ProjectEnvironmentBuilder, ProjectEnvironmentImage};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuilderRequest {
    pub metadata: ProjectImageMetadata,
    pub cpus: u8,
    pub memory_mb: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureRequest {
    pub builder_id: String,
    pub install_run_id: String,
    pub clone_credentials_revoked: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionRequest {
    pub generation: i64,
    pub current_image_id: Option<String>,
    pub previous_image_id: Option<String>,
    #[serde(default)]
    pub active_build_ids: Vec<String>,
    #[serde(default)]
    pub release_build_ids: Vec<String>,
}
fn admin(user: &UserId) -> ApiResult<()> {
    if user.0 != "admin" {
        Err(ApiError::Forbidden("node admin required".into()))
    } else {
        Ok(())
    }
}
fn builder_name(id: &str) -> String {
    format!("env-build-{id}")
}
fn builder_operation(id: &str) -> String {
    format!("project-builder-{id}")
}
fn metadata(row: &ProjectEnvironmentBuilder) -> ApiResult<ProjectImageMetadata> {
    let m: ProjectImageMetadata = serde_json::from_str(&row.metadata_json)
        .map_err(|_| ApiError::Internal("invalid builder provenance".into()))?;
    m.validate()?;
    Ok(m)
}
fn named_ubuntu_base() -> Option<String> {
    let path = crate::sandboxes::resolve_image(Some("ubuntu-dev"))
        .ok()
        .flatten()?;
    if !std::fs::symlink_metadata(&path).ok()?.is_file() {
        return None;
    }
    std::path::Path::new(&path)
        .canonicalize()
        .ok()?
        .to_str()
        .map(str::to_owned)
}

fn image_metadata(row: &ProjectEnvironmentImage) -> ApiResult<ProjectImageMetadata> {
    let m: ProjectImageMetadata = serde_json::from_str(&row.metadata_json)
        .map_err(|_| ApiError::Internal("invalid image provenance".into()))?;
    m.validate()?;
    Ok(m)
}
fn owned_builder(state: &AppState, owner: &str, id: &str) -> ApiResult<ProjectEnvironmentBuilder> {
    let row = state.store.project_builder(id)?;
    if row.owner_user_id != owner {
        return Err(ApiError::NotFound("project builder".into()));
    }
    Ok(row)
}
fn owned_image(state: &AppState, owner: &str, id: &str) -> ApiResult<ProjectEnvironmentImage> {
    let row = state.store.project_image(id)?;
    if row.owner_user_id != owner {
        return Err(ApiError::NotFound("project image".into()));
    }
    Ok(row)
}
fn image_view(row: &ProjectEnvironmentImage) -> ApiResult<Value> {
    Ok(
        json!({"schema":1,"protocol":"project-images-v1","image_id":row.id,"builder_id":row.id,"phase":row.phase,"metadata":image_metadata(row)?,"source_sandbox_id":row.source_sandbox_id,"source_volume_id":row.source_volume_id,"capture_source_logical_bytes":row.source_logical_bytes,"logical_bytes":row.logical_bytes,"digest":row.digest,"error_code":row.detail,"created_at":row.created_at,"updated_at":row.updated_at,"logical_released":row.phase=="retired","reclamation_complete":row.phase=="retired"}),
    )
}
fn builder_view(state: &AppState, row: &ProjectEnvironmentBuilder) -> ApiResult<Value> {
    let op = state
        .store
        .lifecycle_operation(&builder_operation(&row.id), &row.owner_user_id)
        .ok();
    let sandbox = state.store.get_sandbox(&row.sandbox_id).ok();
    let reservation = state
        .store
        .replicated_for_sandbox(&row.owner_user_id, &row.sandbox_id)?;
    let source = reservation.as_ref().map(|r| r.logical_bytes).or_else(|| {
        state
            .store
            .project_image(&row.id)
            .ok()
            .map(|r| r.source_logical_bytes)
    });
    let phase = if row.phase != "creating" {
        row.phase.clone()
    } else if op
        .as_ref()
        .is_some_and(|o| o.state == "interrupted" || o.status.is_some_and(|s| s >= 400))
    {
        "failed".into()
    } else if sandbox
        .as_ref()
        .is_some_and(|s| matches!(s.state.as_str(), "running" | "paused" | "stopped"))
    {
        "running".into()
    } else {
        "creating".into()
    };
    let receipt=op.map(|o|json!({"id":o.id,"sandbox_id":o.sandbox_id,"state":o.state,"status":o.status,"sandbox_state":o.sandbox_state,"error":o.error,"retirement_pending":reservation.as_ref().is_some_and(|r|r.state=="deleting"&&!r.logical_released)}));
    Ok(
        json!({"schema":1,"protocol":"project-images-v1","builder_id":row.id,"sandbox_id":row.sandbox_id,"phase":phase,"metadata":metadata(row)?,"lifecycle_operation_id":builder_operation(&row.id),"lifecycle_operation":receipt,"capture_source_logical_bytes":source,"candidate_logical_bytes":source.and_then(|b|b.checked_add(PROJECT_IMAGE_TRAILER_BYTES as i64))}),
    )
}
pub async fn profile(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
) -> ApiResult<Json<Value>> {
    admin(&user)?;
    let capacity = state.backend.clone();
    let capacity = crate::blocking(move || capacity.project_image_capacity()).await;
    let image = named_ubuntu_base();
    let digest = image
        .as_ref()
        .and_then(|p| std::path::Path::new(p).file_stem())
        .and_then(|s| s.to_str())
        .filter(|s| project_digest(s));
    let bytes = image
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len());
    let candidate = bytes
        .and_then(|b| b.checked_add(PROJECT_IMAGE_TRAILER_BYTES))
        .filter(|b| *b <= PROJECT_IMAGE_MAX_BYTES);
    let (quota, images, disks) = state.store.project_storage_usage(&user.0)?;
    let pending = state.store.pending_project_image_bytes()?.max(0) as u64;
    let base_status = if matches!(&capacity, Ok(Some(_))) {
        image.as_ref().zip(digest).map(|(path, digest)| {
            state
                .backend
                .project_image_base_status(std::path::Path::new(path), digest)
        })
    } else {
        None
    };
    let (available, reason, physical) = match capacity {
        Ok(Some(c))
            if candidate.is_some()
                && base_status
                    .as_ref()
                    .is_some_and(|p| p.status == ProjectImageBaseStatus::Verified) =>
        {
            (true, "ready", Some(c))
        }
        Ok(Some(c))
            if base_status
                .as_ref()
                .is_some_and(|p| p.status == ProjectImageBaseStatus::Verifying) =>
        {
            (false, "base_image_verifying", Some(c))
        }
        Ok(Some(c))
            if base_status
                .as_ref()
                .is_some_and(|p| p.status == ProjectImageBaseStatus::Invalid) =>
        {
            (false, "base_image_invalid", Some(c))
        }
        Ok(Some(c)) => (false, "base_image_unavailable", Some(c)),
        Ok(None) => (false, "storage_not_supported", None),
        Err(_) => (false, "storage_unavailable", None),
    };
    let reserved = physical
        .map(|c| c.physical_reserved_bytes.max(pending))
        .unwrap_or(pending);
    Ok(Json(
        json!({"schema":1,"protocol":"project-images-v1","available":available,"reason":reason,"base_image_digest":digest,"base_content_digest":base_status.as_ref().and_then(|p|p.content_digest.as_deref()),"base_image_logical_bytes":bytes,"candidate_logical_bytes":candidate,"logical_quota_bytes":quota,"logical_image_usage_bytes":images,"logical_disk_usage_bytes":disks,"logical_remaining_bytes":quota.saturating_sub(images).saturating_sub(disks).max(0),"max_image_bytes":PROJECT_IMAGE_MAX_BYTES,"scratch_reserve_bytes":PROJECT_IMAGE_HEADROOM_BYTES,"physical_available_bytes":physical.map(|c|c.physical_available_bytes),"physical_reserved_bytes":reserved,"physical_remaining_bytes":physical.map(|c|c.physical_available_bytes.saturating_sub(reserved))}),
    ))
}
pub async fn create_builder(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
    Json(body): Json<BuilderRequest>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    admin(&user)?;
    body.metadata.validate()?;
    if !project_uuid(&id)
        || body.metadata.build_id != id
        || body.cpus == 0
        || body.cpus > 8
        || body.memory_mb < 128
    {
        return Err(ApiError::Invalid("invalid clean builder request".into()));
    }
    let row = ProjectEnvironmentBuilder {
        id: id.clone(),
        sandbox_id: builder_name(&id),
        owner_user_id: user.0.clone(),
        request_json: serde_json::to_string(&body)
            .map_err(|_| ApiError::Invalid("invalid builder request".into()))?,
        metadata_json: String::from_utf8(body.metadata.canonical_bytes()?)
            .map_err(|_| ApiError::Invalid("invalid builder provenance".into()))?,
        phase: "creating".into(),
        created_at: unix_now(),
    };
    let guard = state.lifecycle.lock(&row.sandbox_id).await;
    crate::routes::identity_available(&state, &row.sandbox_id)?;
    let fresh = state.store.admit_project_builder(&row)?;
    let saved = state.store.project_builder(&id)?;
    let request = operations::Request::Create {
        sandbox_id: row.sandbox_id.clone(),
        cpus: body.cpus,
        memory_mb: body.memory_mb,
        image: Some("ubuntu-dev".into()),
        image_digest: Some(body.metadata.base_image_digest.clone()),
        project_image: None,
        storage_mode: Some(StorageMode::Replicated),
        network_bytes_per_sec: None,
    };
    let receipt = state
        .store
        .lifecycle_operation(&builder_operation(&id), &user.0)
        .ok();
    if saved.phase == "failed"
        && receipt.as_ref().is_some_and(|r| {
            r.status == Some(409)
                && r.error
                    .as_ref()
                    .is_some_and(|e| e["code"] == "project_image_base_invalid")
        })
    {
        return Err(ApiError::Conflict(
            "installed Ubuntu base does not match its frozen digest".into(),
        ));
    }
    if matches!(saved.phase.as_str(), "retired" | "sealed" | "captured") {
        return Ok((StatusCode::OK, Json(builder_view(&state, &saved)?)));
    }
    if receipt.is_none() {
        let probe = state.backend.clone();
        if crate::blocking(move || probe.project_image_capacity())
            .await?
            .is_none()
        {
            return Err(ApiError::Conflict(
                "project image storage is not configured".into(),
            ));
        }
        let base = named_ubuntu_base().unwrap_or_default();
        let proof = state.backend.project_image_base_status(
            std::path::Path::new(&base),
            &body.metadata.base_image_digest,
        );
        match proof.status {
            ProjectImageBaseStatus::Verifying => {
                return Err(ApiError::Conflict(
                    "installed Ubuntu base verification is pending".into(),
                ))
            }
            ProjectImageBaseStatus::Invalid => {
                state.store.reject_project_builder(
                    &id,
                    &serde_json::to_string(&request)
                        .map_err(|_| ApiError::Internal("invalid create intent".into()))?,
                    unix_now(),
                )?;
                return Err(ApiError::Conflict(
                    "installed Ubuntu base does not match its frozen digest".into(),
                ));
            }
            ProjectImageBaseStatus::Verified
                if proof.content_digest.as_deref() == Some(&body.metadata.base_content_digest) => {}
            ProjectImageBaseStatus::Verified => {
                state.store.reject_project_builder(
                    &id,
                    &serde_json::to_string(&request)
                        .map_err(|_| ApiError::Internal("invalid create intent".into()))?,
                    unix_now(),
                )?;
                return Err(ApiError::Conflict(
                    "installed Ubuntu base does not match its frozen digest".into(),
                ));
            }
        }
    }
    drop(guard);
    let result = operations::submit(
        State(state.clone()),
        Extension(user),
        Path(builder_operation(&id)),
        Json(request),
    )
    .await;
    result?;
    let row = state.store.project_builder(&id)?;
    Ok((
        if fresh {
            StatusCode::ACCEPTED
        } else {
            StatusCode::OK
        },
        Json(builder_view(&state, &row)?),
    ))
}

pub async fn get_builder(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    admin(&user)?;
    Ok(Json(builder_view(
        &state,
        &owned_builder(&state, &user.0, &id)?,
    )?))
}
pub async fn get_image(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    admin(&user)?;
    let row = owned_image(&state, &user.0, &id)?;
    if row.phase == "capturing"
        || (row.phase == "preparing" && state.store.project_builder(&id)?.phase == "sealed")
    {
        let body: CaptureRequest = serde_json::from_str(&row.request_json)
            .map_err(|_| ApiError::Internal("invalid capture journal".into()))?;
        spawn_capture(state.clone(), row.clone(), body, None);
    }
    Ok(Json(image_view(&row)?))
}
pub async fn capture(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
    Json(body): Json<CaptureRequest>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    admin(&user)?;
    if !project_uuid(&id)
        || id != body.builder_id
        || !project_uuid(&body.install_run_id)
        || !body.clone_credentials_revoked
    {
        return Err(ApiError::Invalid(
            "capture requires a clean builder and revoked clone credentials".into(),
        ));
    }
    let builder = owned_builder(&state, &user.0, &id)?;
    let canonical = serde_json::to_string(&body)
        .map_err(|_| ApiError::Invalid("invalid capture request".into()))?;
    let image_lc = state.lifecycle.lock(&format!("project-image:{id}")).await;
    match state.store.project_image(&id) {
        Ok(row) => {
            if row.phase == "retired" || row.request_json != canonical {
                return Err(ApiError::Conflict(
                    "capture identity already has another request".into(),
                ));
            }
            return Ok((StatusCode::OK, Json(image_view(&row)?)));
        }
        Err(ahvm_store::Error::NotFound(_)) => {}
        Err(e) => return Err(e.into()),
    }
    let reservation = state
        .store
        .replicated_for_sandbox(&user.0, &builder.sandbox_id)?
        .filter(|r| !r.logical_released)
        .ok_or_else(|| ApiError::Conflict("builder disk geometry is unavailable".into()))?;
    let logical = reservation
        .logical_bytes
        .checked_add(PROJECT_IMAGE_TRAILER_BYTES as i64)
        .filter(|b| *b > 0 && *b as u64 <= PROJECT_IMAGE_MAX_BYTES)
        .ok_or_else(|| ApiError::Invalid("builder disk exceeds image limit".into()))?;
    let probe = state.backend.clone();
    let capacity = crate::blocking(move || probe.project_image_capacity())
        .await?
        .ok_or_else(|| ApiError::Conflict("project image storage is not configured".into()))?;
    let pending = state.store.pending_project_image_bytes()?.max(0) as u64;
    if capacity
        .physical_available_bytes
        .saturating_sub(capacity.physical_reserved_bytes.max(pending))
        < (logical as u64).saturating_add(PROJECT_IMAGE_HEADROOM_BYTES)
    {
        return Err(ApiError::Conflict(
            "project image disk headroom is unavailable".into(),
        ));
    }
    let m = metadata(&builder)?;
    let row = ProjectEnvironmentImage {
        id: id.clone(),
        owner_user_id: user.0,
        tenant_id: m.tenant_id.clone(),
        project_id: m.project_id.clone(),
        metadata_json: builder.metadata_json,
        request_json: canonical,
        source_volume_id: reservation.volume_id,
        source_sandbox_id: builder.sandbox_id,
        source_logical_bytes: reservation.logical_bytes,
        logical_bytes: logical,
        phase: "preparing".into(),
        digest: None,
        image_path: None,
        detail: None,
        created_at: unix_now(),
        updated_at: unix_now(),
    };
    let fresh = state.store.admit_project_image(&row)?;
    if fresh {
        spawn_capture(state.clone(), row.clone(), body, Some(image_lc));
    }
    Ok((StatusCode::ACCEPTED, Json(image_view(&row)?)))
}
fn spawn_capture(
    state: AppState,
    row: ProjectEnvironmentImage,
    body: CaptureRequest,
    guard: Option<crate::scheduler::LifecycleGuard>,
) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static JOBS: OnceLock<Mutex<HashSet<(usize, String)>>> = OnceLock::new();
    let jobs = JOBS.get_or_init(|| Mutex::new(HashSet::new()));
    let key = (
        std::sync::Arc::as_ptr(&state.store) as usize,
        row.id.clone(),
    );
    if !jobs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key.clone())
    {
        return;
    }
    struct JobGuard {
        key: (usize, String),
    }
    impl Drop for JobGuard {
        fn drop(&mut self) {
            if let Some(jobs) = JOBS.get() {
                jobs.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&self.key);
            }
        }
    }
    tokio::spawn(async move {
        let _job = JobGuard { key };
        let mut first_guard = guard;
        for attempt in 0..12 {
            let image = match first_guard.take() {
                Some(g) => g,
                None => {
                    state
                        .lifecycle
                        .lock(&format!("project-image:{}", row.id))
                        .await
                }
            };
            let vm = state.lifecycle.lock(&row.source_sandbox_id).await;
            let permit = state.ops.acquire().await;
            let worker = state.clone();
            let id = row.id.clone();
            let input = body.clone();
            let prepared = tokio::task::spawn_blocking(move || {
                let (_vm, _permit) = (vm, permit);
                prepare_capture(&worker, &id, &input)
            })
            .await;
            if !matches!(prepared, Ok(Ok(()))) {
                let sealed = state
                    .store
                    .project_builder(&row.id)
                    .is_ok_and(|b| b.phase == "sealed");
                if !sealed {
                    let _ = state.store.set_project_image_phase(
                        &row.id,
                        "failed",
                        Some("capture_preflight_failed"),
                        unix_now(),
                    );
                    return;
                }
                let _ = state.store.set_project_image_detail(
                    &row.id,
                    "preparing",
                    "capture_stop_sync_pending",
                    unix_now(),
                );
                drop(image);
                if attempt == 11 {
                    return;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
            let vm = state.lifecycle.lock(&row.source_sandbox_id).await;
            let worker = state.clone();
            let id = row.id.clone();
            let result = tokio::task::spawn_blocking(move || {
                let _vm = vm;
                capture_step(&worker, &id)
            })
            .await;
            if matches!(result, Ok(Ok(true))) {
                return;
            }
            let _ = state.store.set_project_image_detail(
                &row.id,
                "capturing",
                "capture_reconciliation_required",
                unix_now(),
            );
            drop(image);
            if attempt == 11 {
                return;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        // Unconfirmed transport remains durably charged and pinned. A future
        // observation or daemon recovery restarts bounded reconciliation.
    });
}
fn validate_info(row: &ProjectEnvironmentImage, info: &ProjectImageInfo) -> ApiResult<()> {
    if info.image_id != row.id
        || info.source_volume_id != row.source_volume_id
        || info.logical_bytes != row.logical_bytes as u64
        || info.metadata != image_metadata(row)?
    {
        return Err(ApiError::Conflict(
            "private image registry provenance changed".into(),
        ));
    }
    Ok(())
}
fn prepare_capture(state: &AppState, id: &str, body: &CaptureRequest) -> ApiResult<()> {
    let row = state.store.project_image(id)?;
    if row.phase == "capturing" {
        return Ok(());
    }
    if row.phase != "preparing" {
        return Err(ApiError::Conflict("capture is no longer active".into()));
    }
    let builder = state.store.project_builder(id)?;
    if builder.phase != "sealed" {
        let run = state.store.get_managed_run(&body.install_run_id)?;
        if run.sandbox_id != row.source_sandbox_id
            || run.phase != "succeeded"
            || run.exit_code != Some(0)
            || run.finished_at.is_none()
        {
            return Err(ApiError::Conflict(
                "builder installation is not confirmed".into(),
            ));
        }
        let request: crate::runs::RunRequest = serde_json::from_str(&run.request_json)
            .map_err(|_| ApiError::Conflict("invalid install receipt".into()))?;
        if !request.session_isolated || request.fence_on_failure {
            return Err(ApiError::Conflict(
                "builder install requires an isolated finite controller".into(),
            ));
        }
        let m = image_metadata(&row)?;
        let args = vec![
            "/usr/bin/python3".into(),
            "-I".into(),
            "-c".into(),
            include_str!("project_images/sanitize.py").into(),
            serde_json::to_string(&m)
                .map_err(|_| ApiError::Internal("invalid image provenance".into()))?,
            body.install_run_id.clone(),
        ];
        state.backend.resume_paused(&row.source_sandbox_id)?;
        let output = state.backend.exec(&row.source_sandbox_id, &args)?;
        if output.exit_code != 0 {
            return Err(ApiError::Conflict(
                "builder root proof or sanitization failed".into(),
            ));
        }
        state.store.set_project_builder_phase(id, "sealed")?;
    }
    if state.backend.status(&row.source_sandbox_id)?.state != VmState::Stopped {
        state.backend.stop(&row.source_sandbox_id)?;
    }
    if state.backend.status(&row.source_sandbox_id)?.state != VmState::Stopped {
        return Err(ApiError::Conflict("builder stop has not completed".into()));
    }
    let proof = state.backend.sync_remote(&row.source_sandbox_id)?;
    if proof.local_failed
        || proof.replication_failed
        || proof.pending_bytes != 0
        || proof.local_sequence != proof.remote_sequence
    {
        return Err(ApiError::Conflict(
            "builder disk is not fully synced".into(),
        ));
    }
    state
        .store
        .set_sandbox_state(&row.source_sandbox_id, "stopped", "cold", unix_now())?;
    state
        .store
        .set_project_image_phase(id, "capturing", None, unix_now())?;
    Ok(())
}
fn capture_step(state: &AppState, id: &str) -> ApiResult<bool> {
    let row = state.store.project_image(id)?;
    if row.phase != "capturing" {
        return Ok(true);
    }
    let status = state.backend.project_image_status(
        &row.source_sandbox_id,
        &row.source_volume_id,
        &row.id,
    )?;
    if status.quiescent && matches!(status.phase.as_str(), "failed" | "retired") {
        state.store.set_project_image_phase(
            id,
            "failed",
            Some(status.error_code.as_deref().unwrap_or("capture_failed")),
            unix_now(),
        )?;
        return Ok(true);
    }
    let info = match status.image {
        Some(info) if status.phase == "ready" && status.quiescent => info,
        _ => state
            .backend
            .capture_project_image(&row.source_sandbox_id, &image_metadata(&row)?)?,
    };
    validate_info(&row, &info)?;
    state.store.finish_project_image(
        id,
        &info.digest,
        info.image_path
            .to_str()
            .ok_or_else(|| ApiError::Internal("invalid private image path".into()))?,
        unix_now(),
    )?;
    state.store.set_project_builder_phase(id, "captured")?;
    Ok(true)
}
pub async fn retain(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path((tenant, project)): Path<(String, String)>,
    Json(body): Json<RetentionRequest>,
) -> ApiResult<Json<Value>> {
    admin(&user)?;
    if !project_uuid(&tenant)
        || !project_uuid(&project)
        || body.active_build_ids.len() > 64
        || body.release_build_ids.len() > 64
        || body
            .current_image_id
            .iter()
            .chain(body.previous_image_id.iter())
            .chain(body.active_build_ids.iter())
            .chain(body.release_build_ids.iter())
            .any(|id| !project_uuid(id))
    {
        return Err(ApiError::Invalid("invalid project image retention".into()));
    }
    let canonical = serde_json::to_string(&body)
        .map_err(|_| ApiError::Invalid("invalid retention request".into()))?;
    state.store.retain_project_images(
        &user.0,
        &tenant,
        &project,
        body.generation,
        &canonical,
        body.current_image_id.as_deref(),
        body.previous_image_id.as_deref(),
        &body.active_build_ids,
        &body.release_build_ids,
    )?;
    Ok(Json(
        json!({"schema":1,"protocol":"project-images-v1","tenant_id":tenant,"project_id":project,"generation":body.generation,"current_image_id":body.current_image_id,"previous_image_id":body.previous_image_id,"active_build_ids":body.active_build_ids,"release_build_ids":body.release_build_ids}),
    ))
}
pub async fn delete(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    admin(&user)?;
    if !project_uuid(&id) {
        return Err(ApiError::Invalid("invalid project image identity".into()));
    }
    let _lc = state.lifecycle.lock(&format!("project-image:{id}")).await;
    match state.store.project_image(&id) {
        Ok(_) => {}
        Err(ahvm_store::Error::NotFound(_)) => {
            let row = state
                .store
                .retire_unadmitted_project_image(&id, &user.0, unix_now())?;
            return Ok((StatusCode::OK, Json(image_view(&row)?)));
        }
        Err(e) => return Err(e.into()),
    }
    let row = state
        .store
        .begin_project_image_delete(&id, &user.0, unix_now())?;
    if row.phase != "retired" {
        retire_step(&state, &row).await?;
    }
    let row = owned_image(&state, &user.0, &id)?;
    Ok((
        if row.phase == "retired" {
            StatusCode::OK
        } else {
            StatusCode::ACCEPTED
        },
        Json(image_view(&row)?),
    ))
}
async fn retire_step(state: &AppState, row: &ProjectEnvironmentImage) -> ApiResult<()> {
    let worker = state.clone();
    let owned = row.clone();
    let proof = crate::blocking(move || {
        worker.backend.retire_project_image(
            &owned.source_sandbox_id,
            &owned.source_volume_id,
            &owned.id,
            owned.digest.as_deref(),
        )
    })
    .await?;
    if proof.logical_released && proof.reclamation_complete {
        state
            .store
            .finish_project_image_retirement(&row.id, unix_now())?;
    }
    Ok(())
}
pub(crate) async fn select_image(
    state: &AppState,
    owner: &str,
    sandbox: &str,
    selection: &ProjectImageSelection,
) -> ApiResult<(crate::scheduler::LifecycleGuard, String)> {
    let guard = state
        .lifecycle
        .lock(&format!("project-image:{}", selection.image_id))
        .await;
    let row = owned_image(state, owner, &selection.image_id)?;
    if row.phase != "ready" {
        return Err(ApiError::Conflict(
            "prepared project image is unavailable".into(),
        ));
    }
    let backend = state.backend.clone();
    let source = row.clone();
    let info = crate::blocking(move || {
        backend.project_image(
            &source.source_sandbox_id,
            &source.source_volume_id,
            &source.id,
        )
    })
    .await?
    .ok_or_else(|| ApiError::Conflict("prepared image registry is unavailable".into()))?;
    validate_info(&row, &info)?;
    if info.digest != selection.digest {
        return Err(ApiError::Conflict(
            "prepared image generation changed".into(),
        ));
    }
    state.store.pin_project_image_vm(
        &selection.image_id,
        owner,
        &selection.tenant_id,
        &selection.project_id,
        &selection.digest,
        sandbox,
    )?;
    Ok((guard, info.image_path.to_string_lossy().into_owned()))
}
pub(crate) fn check_builder_create(
    state: &AppState,
    sandbox: &str,
    operation: Option<&str>,
) -> ApiResult<()> {
    if let Some(builder) = state.store.project_builder_for_sandbox(sandbox)? {
        if builder.phase != "creating" || operation != Some(builder_operation(&builder.id).as_str())
        {
            return Err(ApiError::Conflict(
                "builder create is retired, rejected or has another intent".into(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn check_builder_mutable(state: &AppState, sandbox: &str) -> ApiResult<()> {
    if state
        .store
        .project_builder_for_sandbox(sandbox)?
        .is_some_and(|b| matches!(b.phase.as_str(), "sealed" | "captured" | "retired"))
    {
        return Err(ApiError::Conflict(
            "builder source is sealed for immutable capture".into(),
        ));
    }
    Ok(())
}

/// Startup observes persisted intents; it never replays an uncertain guest
/// preflight. Sealed sources can idempotently complete stop/sync/export.
pub fn recover(state: &AppState) -> ApiResult<()> {
    let verifier = state.backend.clone();
    tokio::task::spawn_blocking(move || {
        if verifier.project_image_capacity().ok().flatten().is_some() {
            if let Some(path) = named_ubuntu_base() {
                if let Some(digest) = std::path::Path::new(&path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                {
                    let _ = verifier.project_image_base_status(std::path::Path::new(&path), digest);
                }
            }
        }
    });
    for row in state.store.unfinished_project_images()? {
        if row.phase == "preparing" && state.store.project_builder(&row.id)?.phase != "sealed" {
            state.store.set_project_image_phase(
                &row.id,
                "failed",
                Some("capture_preflight_interrupted"),
                unix_now(),
            )?;
            continue;
        }
        if matches!(row.phase.as_str(), "preparing" | "capturing") {
            let body: CaptureRequest = serde_json::from_str(&row.request_json)
                .map_err(|_| ApiError::Internal("invalid capture journal".into()))?;
            spawn_capture(state.clone(), row, body, None);
        }
    }
    let owned = state.clone();
    tokio::spawn(async move {
        loop {
            let _ = reconcile_vm_pins(&owned).await;
            for row in owned.store.unfinished_project_images().unwrap_or_default() {
                if row.phase == "deleting" {
                    let _guard = owned
                        .lifecycle
                        .lock(&format!("project-image:{}", row.id))
                        .await;
                    let _ = retire_step(&owned, &row).await;
                    continue;
                }
                if matches!(row.phase.as_str(), "preparing" | "capturing") {
                    if let Ok(body) = serde_json::from_str::<CaptureRequest>(&row.request_json) {
                        spawn_capture(owned.clone(), row, body, None);
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
    Ok(())
}
pub(crate) async fn release_vm_pin(
    state: &AppState,
    sandbox: &str,
    operation: &str,
) -> ApiResult<()> {
    let _vm = state.lifecycle.lock(sandbox).await;
    let Some(image) = state.store.project_image_vm(sandbox)? else {
        return Ok(());
    };
    let receipt = state
        .store
        .lifecycle_operation(operation, &image.owner_user_id)?;
    if receipt.state == "pending" || receipt.sandbox_id != sandbox {
        return Err(ApiError::Conflict(
            "VM image create/delete is still pending".into(),
        ));
    }
    let request: Value = serde_json::from_str(&receipt.request)
        .map_err(|_| ApiError::Internal("invalid lifecycle receipt".into()))?;
    if request["action"] != "delete"
        && !(request["action"] == "create" && request["project_image"]["image_id"] == image.id)
    {
        return Err(ApiError::Conflict(
            "image pin does not match the completed lifecycle operation".into(),
        ));
    }
    let backend = state.backend.clone();
    let key = sandbox.to_owned();
    match crate::blocking(move || backend.status(&key)).await {
        Err(ApiError::NotFound(_)) => state.store.release_project_image_vm(sandbox)?,
        Ok(_) => return Err(ApiError::Conflict("VM still owns its image pin".into())),
        Err(e) => return Err(e),
    }
    Ok(())
}
async fn reconcile_vm_pins(state: &AppState) -> ApiResult<()> {
    for (sandbox, image) in state.store.project_image_vm_pins()? {
        if let Some(operation) = state
            .store
            .terminal_project_image_create(&sandbox, &image.id)?
        {
            let _ = release_vm_pin(state, &sandbox, &operation).await;
        }
    }
    Ok(())
}

//! Node-owned finite jobs. HTTP connections never own their activity hold.
//!
//! Native sessions are not idempotent. Record launch intent before dispatch,
//! recover by boot identity + exact argv, and NEVER replay an uncertain launch.
//! Replicated disks are required: fencing an uncertain job must cold-stop its
//! processes, not save them in a resumable RAM snapshot.

use crate::{auth::UserId, state_str, thermal_str, unix_now, ApiError, ApiResult, AppState};
use ahvm_engine::{State as VmState, StorageMode};
use ahvm_store::ManagedRun;
use axum::{
    extract::{Path, State},
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_secs(2);
const RECONCILE_SECS: i64 = 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRequest {
    pub sandbox_id: String,
    pub argv: Vec<String>,
    pub max_runtime_secs: u32,
}

fn admin(user: &UserId) -> ApiResult<()> {
    if user.0 != "admin" {
        return Err(ApiError::Forbidden("node admin required".into()));
    }
    Ok(())
}

fn validate(id: &str, body: &RunRequest) -> ApiResult<()> {
    if id.is_empty()
        || id.len() > 96
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        return Err(ApiError::Invalid(
            "run id must be 1..96 letters, digits, hyphens or underscores".into(),
        ));
    }
    if body.argv.is_empty()
        || body.argv.len() > 128
        || body.argv[0].is_empty()
        || body.argv.iter().any(|s| s.contains('\0'))
        || body.argv.iter().map(String::len).sum::<usize>() > 32768
        || !(1..=86400).contains(&body.max_runtime_secs)
    {
        return Err(ApiError::Invalid(
            "invalid argv or runtime (1..86400 seconds)".into(),
        ));
    }
    Ok(())
}

/// The marker is argv metadata in Forge, not shell interpolation. Arguments
/// after $0 are forwarded literally, including spaces and shell metacharacters.
fn command(run: &ManagedRun) -> ApiResult<Vec<String>> {
    let body: RunRequest = serde_json::from_str(&run.request_json)
        .map_err(|e| ApiError::Internal(format!("run request: {e}")))?;
    let mut argv = vec![
        "/bin/sh".into(),
        "-c".into(),
        "exec \"$@\"".into(),
        format!("ahvm-run:{}", run.id),
    ];
    argv.extend(body.argv);
    Ok(argv)
}

pub async fn submit(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
    Json(body): Json<RunRequest>,
) -> ApiResult<Json<ManagedRun>> {
    admin(&user)?;
    validate(&id, &body)?;
    let lc = state.lifecycle.lock(&body.sandbox_id).await;
    state.store.check_lifecycle_fence(&body.sandbox_id, None)?;
    let row = state.store.get_sandbox(&body.sandbox_id)?;
    let canonical = serde_json::to_string(&body).map_err(|e| ApiError::Internal(e.to_string()))?;
    // A retry must return its receipt even after the VM has stopped.
    match state.store.get_managed_run(&id) {
        Ok(existing) => {
            if existing.sandbox_id != body.sandbox_id
                || existing.owner_id != row.owner_user_id
                || existing.request_json != canonical
            {
                return Err(ApiError::Conflict(
                    "run id already has a different request".into(),
                ));
            }
            return Ok(Json(existing));
        }
        Err(ahvm_store::Error::NotFound(_)) => {}
        Err(e) => return Err(e.into()),
    }
    let check = state.clone();
    let sandbox = body.sandbox_id.clone();
    // Lifecycle ownership stays in the blocking task if the HTTP caller leaves.
    let (lc, live) = tokio::task::spawn_blocking(move || {
        let live = check.backend.status(&sandbox);
        (lc, live)
    })
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?;
    let live = live?;
    if live.storage.mode != StorageMode::Replicated
        || !matches!(live.state, VmState::Running | VmState::Paused)
    {
        return Err(ApiError::Conflict(
            "managed runs require an awake replicated VM; wake it before submitting".into(),
        ));
    }
    let now = unix_now();
    let (run, fresh) = state.store.admit_managed_run(
        &id,
        &body.sandbox_id,
        &row.owner_user_id,
        &canonical,
        now,
        now + i64::from(body.max_runtime_secs),
    )?;
    if fresh {
        // No await between durable admission, hold installation, and spawning.
        assert!(state
            .activity
            .set_managed_run(&run.sandbox_id, &run.id, run.epoch));
        spawn(state, run.clone(), true);
    }
    drop(lc);
    Ok(Json(run))
}

pub async fn get(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
) -> ApiResult<Json<ManagedRun>> {
    admin(&user)?;
    Ok(Json(state.store.get_managed_run(&id)?))
}

pub async fn cancel(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
) -> ApiResult<Json<ManagedRun>> {
    admin(&user)?;
    let initial = state.store.get_managed_run(&id)?;
    let _lc = state.lifecycle.lock(&initial.sandbox_id).await;
    let run = state.store.get_managed_run(&id)?;
    if run.finished_at.is_some() {
        return Ok(Json(run));
    }
    Ok(Json(update(
        &state,
        &run,
        "cancelling",
        None,
        None,
        None,
        Some("cancellation requested"),
        false,
    )?))
}

/// Call once at startup, before accepting HTTP or starting the idle sweeper.
/// The engine's exclusive directory lock prevents two daemon controllers.
pub fn recover(state: &AppState) -> ApiResult<()> {
    for (id, _) in state.store.list_managed_run_cooldowns()? {
        // Foreground activity is not durable: allow a fresh configured idle grace
        // on restart instead of stopping a previously attached shell early.
        state.activity.restore_managed_cooldown(&id, Instant::now());
    }
    let mut claimed = Vec::new();
    for old in state.store.list_active_managed_runs()? {
        let run = state.store.claim_managed_run(&old.id, unix_now())?;
        if !state
            .activity
            .set_managed_run(&run.sandbox_id, &run.id, run.epoch)
        {
            return Err(ApiError::Internal("cannot restore managed activity".into()));
        }
        claimed.push(run);
    }
    for run in claimed {
        spawn(state.clone(), run, false);
    }
    Ok(())
}

pub(crate) fn check_lifecycle(state: &AppState, sandbox: &str) -> ApiResult<()> {
    if state.store.managed_run_for_sandbox(sandbox)?.is_some() {
        return Err(ApiError::Conflict(
            "agent run is active; cancel the run before changing VM lifecycle".into(),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn update(
    state: &AppState,
    run: &ManagedRun,
    phase: &str,
    sid: Option<&str>,
    boot: Option<&str>,
    exit: Option<i32>,
    detail: Option<&str>,
    terminal: bool,
) -> ApiResult<ManagedRun> {
    let saved = state.store.update_managed_run(
        &run.id,
        run.epoch,
        phase,
        sid,
        boot,
        exit,
        detail,
        unix_now(),
        terminal,
    )?;
    if terminal {
        // SQLite commit happens BEFORE dropping the hold. A crash in between
        // is recovered as a completed job, never a lost active-job lease.
        state
            .activity
            .finish_managed_run(&run.sandbox_id, &run.id, run.epoch, Instant::now());
    }
    Ok(saved)
}

fn boot_id(state: &AppState, sandbox: &str) -> ApiResult<String> {
    let chunk = state
        .backend
        .file_read(sandbox, "/proc/sys/kernel/random/boot_id", 0, 64)?;
    let id = std::str::from_utf8(&chunk.data).unwrap_or_default().trim();
    if id.len() != 36 || !id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        return Err(ApiError::Unavailable(
            "guest boot identity unavailable".into(),
        ));
    }
    Ok(id.into())
}

fn spawn(state: AppState, run: ManagedRun, launch: bool) {
    tokio::spawn(async move {
        let mut launch = launch;
        loop {
            let lc = state.lifecycle.lock(&run.sandbox_id).await;
            let permit = state.ops.acquire().await;
            let state2 = state.clone();
            let id = run.id.clone();
            let epoch = run.epoch;
            let start = launch;
            launch = false; // Never redispatch, even after a lost RPC response.
            let step = tokio::task::spawn_blocking(move || {
                let (_lc, _permit) = (lc, permit);
                step(&state2, &id, epoch, start)
            })
            .await;
            match step {
                Ok(Ok(true)) => break,
                Ok(Ok(false)) => {}
                Ok(Err(e)) => eprintln!("managed run {}: {e}", run.id),
                Err(e) => {
                    // Keep the hold and reconcile on the next iteration. A
                    // panicking dispatch must never be retried or become idle.
                    eprintln!("managed run {} controller failed: {e}", run.id);
                }
            }
            tokio::time::sleep(POLL).await;
        }
    });
}

fn step(state: &AppState, id: &str, epoch: i64, launch: bool) -> ApiResult<bool> {
    let mut run = state.store.get_managed_run(id)?;
    if run.epoch != epoch || run.finished_at.is_some() {
        return Ok(true);
    }
    let live = state.backend.status(&run.sandbox_id)?;
    if live.state == VmState::Stopped {
        update(
            state,
            &run,
            "interrupted",
            None,
            None,
            None,
            Some("VM stopped before a confirmed result"),
            true,
        )?;
        return Ok(true);
    }
    if !matches!(live.state, VmState::Running | VmState::Paused) {
        return fence(state, &run, "worker unavailable");
    }
    state.backend.resume_paused(&run.sandbox_id)?;
    let boot = match boot_id(state, &run.sandbox_id) {
        Ok(boot) => boot,
        Err(e) => return uncertain(state, &run, &e.to_string()),
    };
    if run
        .boot_id
        .as_ref()
        .is_some_and(|previous| previous != &boot)
    {
        update(
            state,
            &run,
            "interrupted",
            None,
            None,
            None,
            Some("guest rebooted; job was not replayed"),
            true,
        )?;
        return Ok(true);
    }
    let argv = command(&run)?;
    if run.boot_id.is_none() && unix_now() >= run.deadline_at {
        update(
            state,
            &run,
            "interrupted",
            None,
            None,
            None,
            Some("deadline passed before dispatch"),
            true,
        )?;
        return Ok(true);
    }
    if launch && run.phase == "starting" && run.boot_id.is_none() {
        // Commit boot/launch intent first; after this point a restart must
        // look for the marker, even if there is no saved session_id yet.
        run = update(
            state,
            &run,
            "starting",
            None,
            Some(&boot),
            None,
            None,
            false,
        )?;
        match state.backend.session_create(&run.sandbox_id, &argv, false) {
            Ok(sid) => {
                run = update(state, &run, "running", Some(&sid), None, None, None, false)?;
            }
            Err(e) => return uncertain(state, &run, &format!("launch response unavailable: {e}")),
        }
    } else if run.boot_id.is_none() {
        update(
            state,
            &run,
            "interrupted",
            None,
            None,
            None,
            Some("controller stopped before dispatch; job was not replayed"),
            true,
        )?;
        return Ok(true);
    }
    let sessions = match state.backend.session_list(&run.sandbox_id) {
        Ok(sessions) => sessions,
        Err(e) => return uncertain(state, &run, &format!("session reconciliation: {e}")),
    };
    let matching: Vec<_> = sessions
        .iter()
        .filter(|s| s.argv == argv && run.session_id.as_ref().is_none_or(|id| id == &s.id))
        .collect();
    if matching.len() != 1 {
        return uncertain(state, &run, "exact guest session could not be identified");
    }
    let session = matching[0];
    if run.session_id.is_none() {
        let phase = if run.phase == "cancelling" {
            "cancelling"
        } else {
            "running"
        };
        run = update(
            state,
            &run,
            phase,
            Some(&session.id),
            None,
            None,
            None,
            false,
        )?;
    }
    if !session.running {
        // Read at the stream tail: stdout retention/delivery belongs to the
        // adapter, and a large output must not hide the terminal frame.
        let chunk = match state.backend.session_poll(
            &run.sandbox_id,
            &session.id,
            u64::MAX,
            Duration::from_millis(1000),
        ) {
            Ok(chunk) => chunk,
            Err(e) => return uncertain(state, &run, &format!("result reconciliation: {e}")),
        };
        if let Some(code) = chunk.exit_code.filter(|_| chunk.eof) {
            let phase = if run.phase == "cancelling" {
                "interrupted"
            } else if code == 0 {
                "succeeded"
            } else {
                "failed"
            };
            update(state, &run, phase, None, None, Some(code), None, true)?;
            return Ok(true);
        }
        return uncertain(state, &run, "session exited without a confirmed result");
    }
    if unix_now() >= run.deadline_at || run.phase == "cancelling" {
        // A cold stop is the final fence if kill/result acknowledgement was
        // lost. It retains the replicated disk and never snapshots job RAM.
        if run.phase != "cancelling" {
            run = update(
                state,
                &run,
                "cancelling",
                None,
                None,
                None,
                Some("runtime deadline reached"),
                false,
            )?;
        }
        let _ = state.backend.session_kill(&run.sandbox_id, &session.id);
        if unix_now().saturating_sub(run.updated_at) >= RECONCILE_SECS {
            return fence(state, &run, "cancellation could not be confirmed");
        }
    } else if run.phase != "running" {
        update(state, &run, "running", None, None, None, None, false)?;
    }
    Ok(false)
}

fn uncertain(state: &AppState, run: &ManagedRun, detail: &str) -> ApiResult<bool> {
    let run = if run.phase != "uncertain" && run.phase != "cancelling" {
        update(
            state,
            run,
            "uncertain",
            None,
            None,
            None,
            Some(detail),
            false,
        )?
    } else {
        run.clone()
    };
    if unix_now().saturating_sub(run.updated_at) >= RECONCILE_SECS || unix_now() >= run.deadline_at
    {
        return fence(
            state,
            &run,
            "could not reconcile job; stopping VM without replay",
        );
    }
    Ok(false)
}

fn fence(state: &AppState, run: &ManagedRun, detail: &str) -> ApiResult<bool> {
    let live = state.backend.status(&run.sandbox_id)?;
    if live.storage.mode != StorageMode::Replicated {
        return Err(ApiError::Conflict(
            "cannot safely fence a non-replicated managed run".into(),
        ));
    }
    state.backend.stop(&run.sandbox_id)?;
    let live = state.backend.status(&run.sandbox_id)?;
    if live.state != VmState::Stopped {
        return Err(ApiError::Unavailable(
            "job fencing has not completed".into(),
        ));
    }
    state.store.set_sandbox_state(
        &run.sandbox_id,
        &state_str(&live.state),
        &thermal_str(&live.thermal),
        unix_now(),
    )?;
    update(
        state,
        run,
        "interrupted",
        None,
        None,
        None,
        Some(detail),
        true,
    )?;
    Ok(true)
}

#[cfg(test)]
mod tests;

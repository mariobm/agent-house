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
pub async fn guest(state: &AppState, id: &str) -> ApiResult<crate::thermal::InFlight> {
    admit(state, id, true).await
}

/// Passive session inspection/cleanup must not wake an idle VM.
pub async fn guest_passive(state: &AppState, id: &str) -> ApiResult<crate::thermal::InFlight> {
    admit(state, id, false).await
}

async fn admit(state: &AppState, id: &str, wake: bool) -> ApiResult<crate::thermal::InFlight> {
    let lifecycle = state.lifecycle.lock(id).await;
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
        Ok(flight)
    })
    .await
    .map_err(|e| ApiError::Internal(format!("admission task: {e}")))?
}

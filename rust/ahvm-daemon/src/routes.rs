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

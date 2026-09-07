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

//! Bearer-token auth: `Authorization: Bearer <token>`, SHA-256 hexed and
//! looked up via `Store::get_user_by_key_hash` (same scheme as Go's
//! `sha256Hex`). Handlers receive the authed user id as
//! `Extension<UserId>`; sandbox rows are additionally ownership-checked
//! per request (unknown-or-foreign ids are 404 either way, never 403, so
//! ids are not an existence oracle).

use crate::{ApiError, ApiResult, AppState};
use axum::{
    extract::{Request, State},
    http::header,
    middleware::Next,
    response::Response,
};
use sha2::{Digest, Sha256};

/// Authed user id for handlers (never the raw token).
#[derive(Debug, Clone)]
pub struct UserId(pub String);

fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub async fn require_user(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> ApiResult<Response> {
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|t| !t.is_empty())
        .ok_or(ApiError::Unauthorized)?;
    let user = state
        .store
        .get_user_by_key_hash(&hash_token(token))
        .map_err(|_| ApiError::Unauthorized)?;
    req.extensions_mut().insert(UserId(user.id));
    Ok(next.run(req).await)
}

/// Bootstrap helper: ensure an `admin` user exists for `token` (used when
/// `AHVM_ADMIN_TOKEN` is set). Returns true when it created the user.
pub fn ensure_admin(store: &ahvm_store::Store, token: &str) -> std::result::Result<bool, ApiError> {
    match store.get_user("admin") {
        Ok(_) => Ok(false),
        Err(ahvm_store::Error::NotFound(_)) => {
            let now = crate::unix_now();
            store.upsert_user(&ahvm_store::User {
                id: "admin".to_string(),
                name: "admin".to_string(),
                api_key_hash: hash_token(token),
                max_sandboxes: 16,
                max_cpus: 32,
                max_memory_mb: 65536,
                max_volumes_mb: 102400,
                max_snapshots: 64,
                created_at: now,
                updated_at: now,
            })?;
            Ok(true)
        }
        Err(e) => Err(e.into()),
    }
}

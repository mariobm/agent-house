//! Request-independent lifecycle execution with durable, owner-bound receipts.
//! A receipt is authoritative about completion, including after a lost HTTP
//! response. Interrupted receipts fence retries across daemon restarts.
use crate::{auth::UserId, sandboxes, ApiError, ApiResult, AppState};
use ahvm_store::LifecycleOperation;
use axum::{
    extract::{Path, State},
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Create {
        sandbox_id: String,
        cpus: u8,
        memory_mb: u32,
    },
    Start {
        sandbox_id: String,
    },
    Stop {
        sandbox_id: String,
    },
    Delete {
        sandbox_id: String,
    },
}
impl Request {
    fn sandbox_id(&self) -> &str {
        match self {
            Self::Create { sandbox_id, .. }
            | Self::Start { sandbox_id }
            | Self::Stop { sandbox_id }
            | Self::Delete { sandbox_id } => sandbox_id,
        }
    }
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}
async fn view(state: &AppState, op: LifecycleOperation) -> ApiResult<Response> {
    let status = if op.state == "pending" {
        axum::http::StatusCode::ACCEPTED
    } else {
        axum::http::StatusCode::OK
    };
    let sandbox_state = if op.state == "pending" {
        None
    } else {
        // Read backend and metadata under the same lifecycle lock. An orphan
        // backend with no SQL row is NOT evidence that allocation is absent.
        let _lc = state.lifecycle.lock(&op.sandbox_id).await;
        let backend = state.backend.clone();
        let id = op.sandbox_id.clone();
        let tracked = match state.store.get_sandbox(&id) {
            Ok(row) if row.owner_user_id == op.owner_user_id => true,
            Ok(_) => return Err(ApiError::NotFound("sandbox".into())),
            Err(ahvm_store::Error::NotFound(_)) => false,
            Err(e) => return Err(e.into()),
        };
        Some(match crate::blocking(move || backend.status(&id)).await {
            Ok(info) if tracked => crate::state_str(&info.state),
            Ok(_) => "untracked".into(),
            Err(ApiError::NotFound(_)) if !tracked => "absent".into(),
            Err(ApiError::NotFound(_)) => "failed".into(),
            Err(e) => return Err(e),
        })
    };
    Ok((status,Json(serde_json::json!({"id":op.id,"sandbox_id":op.sandbox_id,"state":op.state,"status":op.status,"sandbox_state":sandbox_state}))).into_response())
}
pub async fn get(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    view(&state, state.store.lifecycle_operation(&id, &user.0)?).await
}
pub async fn submit(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
    Json(request): Json<Request>,
) -> ApiResult<Response> {
    if !valid_id(&id) || !valid_id(request.sandbox_id()) {
        return Err(ApiError::Invalid("invalid operation or sandbox id".into()));
    }
    let canonical =
        serde_json::to_string(&request).map_err(|e| ApiError::Internal(e.to_string()))?;
    // Check receipts before ownership: deletion deliberately removes the row but
    // must not make a completed delete or create executable again.
    match state.store.lifecycle_operation(&id, &user.0) {
        Ok(old) => {
            if old.request != canonical {
                return Err(ApiError::Conflict("operation key reused".into()));
            }
            return view(&state, old).await;
        }
        Err(ahvm_store::Error::NotFound(_)) => {}
        Err(e) => return Err(e.into()),
    }
    if matches!(request, Request::Create { .. }) {
        crate::routes::reserved_owner(&state, &user.0, request.sandbox_id())?;
        match state.store.get_sandbox(request.sandbox_id()) {
            Ok(row) if row.owner_user_id != user.0 => {
                return Err(ApiError::NotFound("sandbox".into()))
            }
            Ok(_) | Err(ahvm_store::Error::NotFound(_)) => {}
            Err(e) => return Err(e.into()),
        }
    } else {
        match crate::routes::owned(&state, &user.0, request.sandbox_id()).await {
            Ok(_) => {}
            // An absent delete still needs a receipt. The normal destroy route
            // checks ownership again and will not touch an untracked backend.
            Err(ApiError::NotFound(_)) if matches!(request, Request::Delete { .. }) => {
                match state.store.get_sandbox(request.sandbox_id()) {
                    Err(ahvm_store::Error::NotFound(_)) => {}
                    Ok(_) => return Err(ApiError::NotFound("sandbox".into())),
                    Err(e) => return Err(e.into()),
                }
            }
            Err(e) => return Err(e),
        }
    }
    let op = LifecycleOperation {
        id: id.clone(),
        owner_user_id: user.0.clone(),
        sandbox_id: request.sandbox_id().into(),
        request: canonical,
        state: "pending".into(),
        status: None,
    };
    if state
        .store
        .admit_lifecycle_operation(&op, crate::unix_now())?
    {
        // No await between committing the receipt and spawning. Dropping the
        // request/JoinHandle does not cancel this task or its lifecycle guards.
        let task_state = state.clone();
        let operation_id = id.clone();
        let mut task = tokio::spawn(async move {
            let response = execute(task_state.clone(), user, request, &operation_id).await;
            if let Err(error) = task_state
                .store
                .finish_lifecycle_operation(&operation_id, response.status().as_u16())
            {
                eprintln!("ahvm-daemon: persist operation {operation_id}: {error}");
            }
        });
        // Fast operations retain the synchronous CLI experience; slow ones have
        // a pollable receipt. This timeout never aborts the executing task.
        let _ = tokio::time::timeout(Duration::from_secs(20), &mut task).await;
    }
    view(
        &state,
        state.store.lifecycle_operation(&id, &op.owner_user_id)?,
    )
    .await
}
async fn execute(state: AppState, user: UserId, request: Request, operation_id: &str) -> Response {
    let s = State(state);
    let u = Extension(user);
    match request {
        Request::Create {
            sandbox_id,
            cpus,
            memory_mb,
        } => sandboxes::create_operation(
            s,
            u,
            Json(sandboxes::CreateBody {
                name: sandbox_id,
                cpus,
                memory_mb,
                desktop: false,
                image: None,
            }),
            Some(operation_id),
        )
        .await
        .into_response(),
        Request::Start { sandbox_id } => {
            sandboxes::start_operation(s, u, Path(sandbox_id), Some(operation_id))
                .await
                .into_response()
        }
        Request::Stop { sandbox_id } => {
            sandboxes::stop_operation(s, u, Path(sandbox_id), Some(operation_id))
                .await
                .into_response()
        }
        Request::Delete { sandbox_id } => {
            sandboxes::destroy_operation(s, u, Path(sandbox_id), Some(operation_id))
                .await
                .into_response()
        }
    }
}

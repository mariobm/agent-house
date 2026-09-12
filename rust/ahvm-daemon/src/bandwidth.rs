//! Optional API payload pacing, shared across a VM's concurrent HTTP/WS calls.
//! Separate from Ethernet caps: JSON/base64 bytes count on this transport.
use crate::{auth::UserId, ApiError, ApiResult, AppState};
use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

const CHUNK: usize = 65536;

#[derive(Debug)]
struct Credit {
    at: Instant,
    bytes: f64,
}
#[derive(Debug)]
pub struct Pace {
    rate: u64,
    credit: tokio::sync::Mutex<Credit>,
}
impl Pace {
    fn new(rate: u64) -> Self {
        Self {
            rate,
            credit: tokio::sync::Mutex::new(Credit {
                at: Instant::now(),
                bytes: CHUNK as f64,
            }),
        }
    }
    pub async fn take(&self, mut bytes: usize) {
        while bytes > 0 {
            let n = bytes.min(CHUNK);
            // Serial per direction; cancellation drops this lock without
            // reserving future credit. Other VMs/directions remain independent.
            let mut credit = self.credit.lock().await;
            loop {
                let now = Instant::now();
                credit.bytes = (credit.bytes + (now - credit.at).as_secs_f64() * self.rate as f64)
                    .min(CHUNK as f64);
                credit.at = now;
                if credit.bytes >= n as f64 {
                    credit.bytes -= n as f64;
                    break;
                }
                tokio::time::sleep(Duration::from_secs_f64(
                    (n as f64 - credit.bytes) / self.rate as f64,
                ))
                .await;
            }
            bytes -= n;
        }
    }
}

#[derive(Debug)]
pub struct Transfer {
    pub input: Arc<Pace>,
    pub output: Arc<Pace>,
    permits: Arc<Semaphore>,
}
#[derive(Debug, Clone)]
pub struct Registry {
    rate: Option<u64>,
    entries: Arc<Mutex<HashMap<String, Arc<Transfer>>>>,
    total: Arc<Semaphore>,
}
impl Default for Registry {
    fn default() -> Self {
        Self::new(None)
    }
}
impl Registry {
    pub fn new(rate: Option<u64>) -> Self {
        assert!(
            rate.is_none_or(|n| (65536..=1_000_000_000).contains(&n)),
            "invalid API bandwidth rate"
        );
        Self {
            rate,
            entries: Arc::default(),
            total: Arc::new(Semaphore::new(32)),
        }
    }
    pub fn enabled(&self) -> bool {
        self.rate.is_some()
    }

    pub fn get(&self, id: &str) -> Option<Arc<Transfer>> {
        let rate = self.rate?;
        let mut entries = self.entries.lock().expect("bandwidth registry poisoned");
        // Entries persist across disconnects: reconnecting cannot reset credit.
        Some(
            entries
                .entry(id.into())
                .or_insert_with(|| {
                    Arc::new(Transfer {
                        input: Arc::new(Pace::new(rate)),
                        output: Arc::new(Pace::new(rate)),
                        permits: Arc::new(Semaphore::new(4)),
                    })
                })
                .clone(),
        )
    }
    pub fn forget(&self, id: &str) {
        self.entries
            .lock()
            .expect("bandwidth registry poisoned")
            .remove(id);
    }
    fn admit(&self, transfer: &Transfer) -> ApiResult<Arc<Admission>> {
        let total = self
            .total
            .clone()
            .try_acquire_owned()
            .map_err(|_| ApiError::Conflict("API transfer capacity busy".into()))?;
        let vm = transfer
            .permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| ApiError::Conflict("VM transfer capacity busy".into()))?;
        Ok(Arc::new(Admission {
            _total: total,
            _vm: vm,
        }))
    }
}
struct Admission {
    _total: OwnedSemaphorePermit,
    _vm: OwnedSemaphorePermit,
}

fn paced(body: Body, pace: Arc<Pace>, guard: Arc<Admission>) -> Body {
    let stream = body.into_data_stream();
    Body::from_stream(futures_util::stream::unfold(
        (stream, Bytes::new(), pace, guard),
        |(mut stream, mut pending, pace, guard)| async move {
            if pending.is_empty() {
                match tokio::time::timeout(Duration::from_secs(30), stream.next()).await {
                    Ok(Some(Ok(bytes))) => pending = bytes,
                    Ok(Some(Err(e))) => {
                        return Some((
                            Err(std::io::Error::other(e)),
                            (stream, pending, pace, guard),
                        ))
                    }
                    Ok(None) => return None,
                    Err(_) => {
                        return Some((
                            Err(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "API body idle timeout",
                            )),
                            (stream, pending, pace, guard),
                        ))
                    }
                }
            }
            let bytes = pending.split_to(pending.len().min(CHUNK));
            pace.take(bytes.len()).await;
            Some((Ok(bytes), (stream, pending, pace, guard)))
        },
    ))
}

pub async fn limit(State(state): State<AppState>, req: Request, next: Next) -> ApiResult<Response> {
    if !state.ops.transfers.enabled() {
        return Ok(next.run(req).await);
    }
    let parts: Vec<_> = req.uri().path().split('/').collect();
    // Only guest data paths. Lifecycle/control calls retain their own budgets.
    if parts.len() < 5
        || parts[1..3] != ["v1", "sandboxes"]
        || !matches!(parts[4], "exec" | "files" | "dir" | "sessions")
    {
        return Ok(next.run(req).await);
    }
    let id = parts[3];
    let user = req
        .extensions()
        .get::<UserId>()
        .ok_or(ApiError::Unauthorized)?;
    crate::routes::owned(&state, &user.0, id).await?;
    let Some(transfer) = state.ops.transfers.get(id) else {
        return Ok(next.run(req).await);
    };
    // WS frames are paced by the bridge; its own admission survives upgrades.
    if parts[4] == "sessions" && parts.len() == 7 && parts[6] == "stream" {
        return Ok(next.run(req).await);
    }
    let guard = state.ops.transfers.admit(&transfer)?;
    let (parts, body) = req.into_parts();
    let response = next
        .run(Request::from_parts(
            parts,
            paced(body, transfer.input.clone(), guard.clone()),
        ))
        .await;
    let (mut parts, body) = response.into_parts();
    // Streaming wrapper has no exact size hint; avoid contradictory framing.
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    Ok(Response::from_parts(
        parts,
        paced(body, transfer.output.clone(), guard),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn reconnect_shares_credit_peer_and_directions_do_not_wait() {
        let registry = Registry::new(Some(65536));
        let a = registry.get("a").unwrap();
        a.input.take(CHUNK).await;
        let again = registry.get("a").unwrap();
        assert!(Arc::ptr_eq(&a, &again));
        let started = Instant::now();
        let task = tokio::spawn(async move {
            again.input.take(CHUNK).await;
        });
        registry.get("b").unwrap().input.take(CHUNK).await;
        a.output.take(CHUNK).await;
        assert!(started.elapsed() < Duration::from_millis(200));
        task.await.unwrap();
        assert!(started.elapsed() >= Duration::from_millis(900));
    }
    #[tokio::test]
    async fn cancellation_does_not_reserve_future_credit() {
        let pace = Arc::new(Pace::new(65536));
        pace.take(CHUNK).await;
        let clone = pace.clone();
        let task = tokio::spawn(async move {
            clone.take(CHUNK * 10).await;
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        task.abort();
        let _ = task.await;
        tokio::time::timeout(Duration::from_secs(2), pace.take(CHUNK))
            .await
            .unwrap();
    }
    #[test]
    fn admission_is_bounded_and_released() {
        let r = Registry::new(Some(65536));
        let a = r.get("a").unwrap();
        let guards: Vec<_> = (0..4).map(|_| r.admit(&a).unwrap()).collect();
        assert!(r.admit(&a).is_err());
        assert!(r.admit(&r.get("b").unwrap()).is_ok());
        drop(guards);
        assert!(r.admit(&a).is_ok());
    }
}

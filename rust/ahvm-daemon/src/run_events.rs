//! Host-owned capture and delivery of bounded guest agent projections.
//! Callback authority never enters the guest and cannot prove tool completion.
use crate::{runs::RunRequest, unix_now, ApiError, ApiResult, AppState};
use ahvm_engine::{Error as EngineError, State as VmState};
use ahvm_store::{ManagedRun, ManagedRunEvent};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fmt, time::Duration};

const MAX_GUEST_LINE: usize = 262144;
const MAX_GUEST_JOURNAL: u64 = 16 * 1024 * 1024;
const CAPTURE_INTERVAL: Duration = Duration::from_millis(250);
const PATCH_ROOTS: &[&str] = &[
    "text",
    "tools",
    "question",
    "phase",
    "detail",
    "githubPublish",
    "admissionError",
];

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunEventDelivery {
    pub url: String,
    pub token: String,
    pub request_hash: String,
}

impl fmt::Debug for RunEventDelivery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunEventDelivery")
            .field("url", &self.url)
            .field("token", &"[redacted]")
            .field("request_hash", &self.request_hash)
            .finish()
    }
}

fn invalid() -> ApiError {
    ApiError::Invalid("invalid managed event delivery or guest record".into())
}

pub(crate) fn configured_origin(origin: &str) -> bool {
    reqwest::Url::parse(origin).is_ok_and(|url| {
        url.scheme() == "https"
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none()
            && url.username().is_empty()
            && url.password().is_none()
    })
}

/// The operator pins the origin. An admin request cannot select arbitrary
/// callback hosts, ports, redirects, embedded auth, query parameters or paths.
fn validate_origin(run: &str, delivery: &RunEventDelivery, origin: &str) -> ApiResult<()> {
    let base = reqwest::Url::parse(origin).map_err(|_| invalid())?;
    let url = reqwest::Url::parse(&delivery.url).map_err(|_| invalid())?;
    if base.scheme() != "https"
        || base.path() != "/"
        || base.query().is_some()
        || base.fragment().is_some()
        || !base.username().is_empty()
        || base.password().is_some()
        || url.origin() != base.origin()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != format!("/internal/agent-events/{run}")
        || url.query().is_some()
        || url.fragment().is_some()
        || delivery.url != url.as_str()
        || !(32..=256).contains(&delivery.token.len())
        || !delivery
            .token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        || delivery.request_hash.len() != 64
        || !delivery
            .request_hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid());
    }
    Ok(())
}

pub(crate) fn validate_delivery(
    run: &str,
    request: &RunRequest,
    delivery: &RunEventDelivery,
) -> ApiResult<()> {
    let origin = std::env::var("AHVM_AGENT_EVENT_ORIGIN")
        .map_err(|_| ApiError::Invalid("managed event origin is not configured".into()))?;
    if !request.session_isolated
        || request.fence_on_failure
        || request.argv
            != [
                "/usr/local/bin/ahvm-dev".to_string(),
                "/usr/bin/env".to_string(),
                "node".to_string(),
                format!("/home/ahvm/.local/state/ahvm-agent/runs/{run}/controller.mjs"),
                format!("/home/ahvm/.local/state/ahvm-agent/runs/{run}/input.json"),
            ]
    {
        return Err(invalid());
    }
    validate_origin(run, delivery, &origin)
}

fn delivery(run: &ManagedRun) -> ApiResult<Option<RunEventDelivery>> {
    let request: RunRequest = serde_json::from_str(&run.request_json).map_err(|_| invalid())?;
    Ok(request.event_delivery)
}

fn bounded_json(value: &Value, depth: usize) -> bool {
    if depth > 8 {
        return false;
    }
    match value {
        Value::Array(a) => a.len() <= 256 && a.iter().all(|v| bounded_json(v, depth + 1)),
        Value::Object(o) => {
            o.len() <= 64
                && o.iter().all(|(k, v)| {
                    !matches!(k.as_str(), "__proto__" | "prototype" | "constructor")
                        && k.len() <= 160
                        && bounded_json(v, depth + 1)
                })
        }
        _ => true,
    }
}

fn valid_native_id(value: &Value, prefix: &str) -> bool {
    value.as_str().is_some_and(|s| {
        s.starts_with(prefix)
            && s.len() > prefix.len()
            && s.len() <= 164
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    })
}

fn validate_checkpoint(value: &Value, run: &ManagedRun, grant: &RunEventDelivery) -> ApiResult<()> {
    let Some(object) = value.as_object() else {
        return Err(invalid());
    };
    let allowed = [
        "schema",
        "harness",
        "runId",
        "requestHash",
        "sessionId",
        "messageId",
        "bootId",
        "phase",
        "text",
        "detail",
        "admissionError",
        "tools",
        "question",
        "githubPublish",
    ];
    if object.keys().any(|key| !allowed.contains(&key.as_str()))
        || value["schema"] != 1
        || value["runId"] != run.id
        || value["requestHash"] != grant.request_hash
        || value["bootId"].as_str() != run.boot_id.as_deref()
        || run.boot_id.is_none()
        || !valid_native_id(&value["sessionId"], "ses_")
        || !valid_native_id(&value["messageId"], "msg_")
        || !matches!(value.get("harness"), None | Some(Value::String(_)))
        || value
            .get("harness")
            .is_some_and(|h| h != "pi" && h != "opencode")
        || !value["phase"].as_str().is_some_and(|s| {
            matches!(
                s,
                "running" | "uncertain" | "succeeded" | "failed" | "interrupted"
            )
        })
        || !value["text"].as_str().is_some_and(|s| s.len() <= 32768)
        || value
            .get("tools")
            .is_some_and(|v| !v.as_array().is_some_and(|a| a.len() <= 32))
        || value
            .get("question")
            .is_some_and(|v| !v.is_null() && (!v.is_object() || v.to_string().len() > 16384))
        || value.get("detail").is_some_and(|v| {
            !v.as_str().is_some_and(|s| {
                !s.is_empty()
                    && s.len() <= 80
                    && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
            })
        })
        || !bounded_json(value, 0)
        || value.to_string().len() > MAX_GUEST_LINE
    {
        return Err(invalid());
    }
    Ok(())
}

fn member_mut<'a>(value: &'a mut Value, path: &[String]) -> ApiResult<&'a mut Value> {
    let mut cursor = value;
    for key in path {
        cursor = match cursor {
            Value::Object(o) => o.get_mut(key).ok_or_else(invalid)?,
            Value::Array(a) => a.get_mut(array_index(key)?).ok_or_else(invalid)?,
            _ => return Err(invalid()),
        };
    }
    Ok(cursor)
}
fn array_index(key: &str) -> ApiResult<usize> {
    if key.is_empty()
        || (key.len() > 1 && key.starts_with('0'))
        || !key.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(invalid());
    }
    key.parse().map_err(|_| invalid())
}

/// The patch is intentionally limited to known projection roots. Strings use
/// UTF-8 byte prefixes at character boundaries, matching the TypeScript writer.
fn apply_patch(checkpoint: &mut Value, operations: &Value) -> ApiResult<()> {
    let operations = operations
        .as_array()
        .filter(|a| !a.is_empty() && a.len() <= 256)
        .ok_or_else(invalid)?;
    for operation in operations {
        let object = operation.as_object().ok_or_else(invalid)?;
        let path = operation["path"]
            .as_array()
            .filter(|p| !p.is_empty() && p.len() <= 8)
            .ok_or_else(invalid)?
            .iter()
            .map(|v| {
                v.as_str()
                    .filter(|k| {
                        !k.is_empty()
                            && k.len() <= 160
                            && !matches!(*k, "__proto__" | "prototype" | "constructor")
                    })
                    .map(String::from)
                    .ok_or_else(invalid)
            })
            .collect::<ApiResult<Vec<_>>>()?;
        if !PATCH_ROOTS.contains(&path[0].as_str()) {
            return Err(invalid());
        }
        let op = operation["op"].as_str().ok_or_else(invalid)?;
        let keys: &[&str] = match op {
            "set" => &["op", "path", "value"],
            "remove" => &["op", "path"],
            "text" => &["op", "path", "prefix", "append"],
            _ => return Err(invalid()),
        };
        if object.len() != keys.len() || object.keys().any(|k| !keys.contains(&k.as_str())) {
            return Err(invalid());
        }
        if op == "text" {
            let prefix = operation["prefix"]
                .as_u64()
                .filter(|n| *n <= MAX_GUEST_LINE as u64)
                .ok_or_else(invalid)? as usize;
            let append = operation["append"].as_str().ok_or_else(invalid)?;
            let target = member_mut(checkpoint, &path)?;
            let text = target
                .as_str()
                .filter(|s| {
                    prefix <= s.len()
                        && s.is_char_boundary(prefix)
                        && prefix + append.len() <= MAX_GUEST_LINE
                })
                .ok_or_else(invalid)?;
            *target = Value::String(format!("{}{append}", &text[..prefix]));
        } else {
            let key = path.last().unwrap();
            let parent = member_mut(checkpoint, &path[..path.len() - 1])?;
            match (op, parent) {
                ("set", Value::Object(o)) => {
                    o.insert(key.clone(), operation["value"].clone());
                }
                ("set", Value::Array(a)) => {
                    let slot = a.get_mut(array_index(key)?).ok_or_else(invalid)?;
                    *slot = operation["value"].clone();
                }
                ("remove", Value::Object(o)) => {
                    if o.remove(key).is_none() {
                        return Err(invalid());
                    }
                }
                // Array structure changes replace the array; no shifting indices.
                _ => return Err(invalid()),
            }
        }
        if !bounded_json(checkpoint, 0) || checkpoint.to_string().len() > MAX_GUEST_LINE {
            return Err(invalid());
        }
    }
    Ok(())
}

fn guest_record(
    line: &[u8],
    source: &ahvm_store::ManagedRunEventStream,
    run: &ManagedRun,
    grant: &RunEventDelivery,
) -> ApiResult<(u64, Value)> {
    if line.len() + 1 > MAX_GUEST_LINE {
        return Err(invalid());
    }
    let record: Value = serde_json::from_slice(line).map_err(|_| invalid())?;
    let object = record.as_object().ok_or_else(invalid)?;
    let seq = record["seq"]
        .as_u64()
        .filter(|s| *s == source.guest_seq + 1)
        .ok_or_else(invalid)?;
    if record["schema"] != 1 || object.len() != 3 {
        return Err(invalid());
    }
    let mut checkpoint = if let Some(value) = record.get("checkpoint") {
        value.clone()
    } else {
        let previous = source.checkpoint_json.as_deref().ok_or_else(invalid)?;
        let mut value = serde_json::from_str(previous).map_err(|_| invalid())?;
        apply_patch(&mut value, record.get("patch").ok_or_else(invalid)?)?;
        value
    };
    validate_checkpoint(&checkpoint, run, grant)?;
    if let Some(previous) = source.checkpoint_json.as_deref() {
        let previous: Value = serde_json::from_str(previous).map_err(|_| invalid())?;
        for key in [
            "schema",
            "harness",
            "runId",
            "requestHash",
            "sessionId",
            "messageId",
            "bootId",
        ] {
            if checkpoint.get(key) != previous.get(key) {
                return Err(invalid());
            }
        }
    }
    // Normalize guest JSON before freezing host payload bytes.
    checkpoint = serde_json::from_str(&checkpoint.to_string()).map_err(|_| invalid())?;
    Ok((seq, checkpoint))
}

/// Caller holds the VM lifecycle lock. Transient file/DB errors leave the
/// native receipt pinned and the managed activity hold installed for retry.
fn capture(
    state: &AppState,
    run: &ManagedRun,
    grant: &RunEventDelivery,
    final_capture: bool,
) -> ApiResult<()> {
    let path = format!(
        "/home/ahvm/.local/state/ahvm-agent/runs/{}/events.ndjson",
        run.id
    );
    let mut iterations = 0;
    loop {
        let source = state.store.managed_run_event_stream(&run.id)?;
        if source.source_failed || source.sealed {
            return Ok(());
        }
        if source.source_offset > MAX_GUEST_JOURNAL {
            state
                .store
                .fail_managed_run_event_source(&run.id, run.epoch)?;
            return Ok(());
        }
        let chunk = match state.backend.file_read(
            &run.sandbox_id,
            &path,
            source.source_offset,
            MAX_GUEST_LINE as u64,
        ) {
            Ok(chunk) => chunk,
            Err(EngineError::NotFound(_)) => {
                if final_capture {
                    state
                        .store
                        .fail_managed_run_event_source(&run.id, run.epoch)?;
                }
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        if chunk.data.is_empty() {
            return Ok(());
        }
        if chunk.data.len() > MAX_GUEST_LINE
            || source.source_offset + chunk.data.len() as u64 > MAX_GUEST_JOURNAL
        {
            state
                .store
                .fail_managed_run_event_source(&run.id, run.epoch)?;
            return Ok(());
        }
        let mut consumed = 0;
        for line in chunk.data.split_inclusive(|b| *b == b'\n') {
            if line.last() != Some(&b'\n') {
                break;
            }
            let current = state.store.managed_run_event_stream(&run.id)?;
            let (seq, checkpoint) =
                match guest_record(&line[..line.len() - 1], &current, run, grant) {
                    Ok(record) => record,
                    Err(_) => {
                        state
                            .store
                            .fail_managed_run_event_source(&run.id, run.epoch)?;
                        eprintln!("managed run {} event source invalid", run.id);
                        return Ok(());
                    }
                };
            consumed += line.len();
            state.store.append_managed_run_checkpoint(
                &run.id,
                run.epoch,
                source.source_offset + consumed as u64,
                seq,
                &checkpoint.to_string(),
                unix_now(),
            )?;
        }
        iterations += 1;
        if consumed < chunk.data.len() && (chunk.eof || consumed == 0) {
            if final_capture || chunk.data.len() == MAX_GUEST_LINE {
                state
                    .store
                    .fail_managed_run_event_source(&run.id, run.epoch)?;
            }
            return Ok(());
        }
        if chunk.eof || (!final_capture && iterations >= 4) {
            return Ok(());
        }
    }
}

pub(crate) fn prepare_terminal(state: &AppState, run: &ManagedRun) -> ApiResult<()> {
    let Some(grant) = delivery(run)? else {
        return Ok(());
    };
    let live = state.backend.status(&run.sandbox_id)?;
    if !matches!(live.state, VmState::Running | VmState::Paused) || run.boot_id.is_none() {
        state
            .store
            .fail_managed_run_event_source(&run.id, run.epoch)?;
        return Ok(());
    }
    let current =
        state
            .backend
            .file_read(&run.sandbox_id, "/proc/sys/kernel/random/boot_id", 0, 64)?;
    if std::str::from_utf8(&current.data).unwrap_or("").trim()
        != run.boot_id.as_deref().unwrap_or("")
    {
        state
            .store
            .fail_managed_run_event_source(&run.id, run.epoch)?;
        return Ok(());
    }
    capture(state, run, &grant, true)
}

pub(crate) fn recover(state: &AppState) -> ApiResult<()> {
    for id in state.store.list_managed_run_event_streams()? {
        start_relay(state.clone(), id);
    }
    Ok(())
}
pub(crate) fn spawn(state: AppState, run: &ManagedRun) {
    if delivery(run).ok().flatten().is_some() {
        start_relay(state, run.id.clone());
    }
}

fn start_relay(state: AppState, id: String) {
    let capture_state = state.clone();
    let capture_id = id.clone();
    tokio::spawn(async move {
        let mut capture_pending = false;
        loop {
            let run = match capture_state.store.get_managed_run(&capture_id) {
                Ok(run) if run.finished_at.is_none() => run,
                Ok(_) | Err(ahvm_store::Error::NotFound(_)) => break,
                Err(_) => {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            let Ok(Some(grant)) = delivery(&run) else {
                break;
            };
            if unix_now() > run.deadline_at.saturating_add(7 * 86400) {
                break;
            }
            if run.boot_id.is_some() {
                let lc = capture_state.lifecycle.lock(&run.sandbox_id).await;
                let permit = capture_state.ops.acquire().await;
                let copy = capture_state.clone();
                let run_id = capture_id.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let (_lc, _permit) = (lc, permit);
                    let run = copy.store.get_managed_run(&run_id)?;
                    if run.finished_at.is_some() {
                        return Ok(());
                    }
                    capture(&copy, &run, &grant, false)
                })
                .await;
                if !matches!(result, Ok(Ok(()))) {
                    // Fixed diagnostic: engine/HTTP errors can contain URLs or
                    // provider bytes and must never enter relay logs.
                    if !capture_pending {
                        eprintln!("managed run {capture_id} event capture pending");
                    }
                    capture_pending = true;
                } else {
                    capture_pending = false;
                }
            }
            tokio::time::sleep(CAPTURE_INTERVAL).await;
        }
    });
    tokio::spawn(async move {
        send_events(state, id).await;
    });
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EventAck {
    schema: u8,
    run_id: String,
    event_seq: u64,
    #[serde(default)]
    retired: bool,
}

fn accepted_ack(body: &[u8], event: &ManagedRunEvent, retirement_required: bool) -> bool {
    serde_json::from_slice::<EventAck>(body).is_ok_and(|ack| {
        ack.schema == 1
            && ack.run_id == event.run_id
            && ack.event_seq == event.seq
            && (!retirement_required || ack.retired)
    })
}

async fn deliver(
    client: &reqwest::Client,
    grant: &RunEventDelivery,
    event: &ManagedRunEvent,
) -> bool {
    let Ok(mut response) = client
        .post(&grant.url)
        .bearer_auth(&grant.token)
        .header("Content-Type", "application/json")
        .body(event.payload_json.clone())
        .send()
        .await
    else {
        return false;
    };
    if !response.status().is_success() && response.status() != reqwest::StatusCode::GONE {
        return false;
    }
    if !response
        .headers()
        .get("Content-Type")
        .and_then(|h| h.to_str().ok())
        .is_some_and(|h| {
            h.split(';')
                .next()
                .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/json"))
        })
    {
        return false;
    }
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if body.len() + chunk.len() <= 4096 => body.extend_from_slice(&chunk),
            Ok(None) => break,
            _ => return false,
        }
    }
    accepted_ack(&body, event, response.status() == reqwest::StatusCode::GONE)
}

async fn send_events(state: AppState, id: String) {
    let Ok((raw, retry_until)) = state.store.managed_run_event_delivery(&id) else {
        return;
    };
    let Ok(grant) = serde_json::from_str::<RunEventDelivery>(&raw) else {
        return;
    };
    let Ok(client) = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .https_only(true)
        .no_proxy()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .build()
    else {
        return;
    };
    let mut failures = 0;
    loop {
        if unix_now() > retry_until {
            eprintln!("managed run {id} event delivery expired; payload retained");
            return;
        }
        let event = match state.store.first_managed_run_event(&id) {
            Ok(Some(event)) => event,
            Ok(None) => {
                match state.store.managed_run_event_stream(&id) {
                    Ok(source) if !source.sealed => {}
                    _ => return,
                }
                tokio::time::sleep(CAPTURE_INTERVAL).await;
                continue;
            }
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let allowed = std::env::var("AHVM_AGENT_EVENT_ORIGIN")
            .ok()
            .is_some_and(|origin| validate_origin(&id, &grant, &origin).is_ok());
        if allowed
            && deliver(&client, &grant, &event).await
            && state
                .store
                .acknowledge_managed_run_event(&id, event.seq)
                .is_ok()
        {
            failures = 0;
            continue;
        }
        failures = (failures + 1u32).min(6);
        if failures == 1 {
            eprintln!("managed run {id} event delivery pending");
        }
        tokio::time::sleep(Duration::from_secs((1u64 << (failures - 1)).min(30))).await;
    }
}

#[cfg(test)]
mod tests;

//! Additive project setup projections. Guest progress never proves termination
//! or promotes a disk; the normal managed receipt remains authoritative.
use super::{
    bounded_json, invalid, ApiResult, ManagedRun, RunEventDelivery, Value, MAX_GUEST_LINE,
};

pub(super) const PATCH_ROOTS: &[&str] = &[
    "phase",
    "revisionId",
    "hookDigest",
    "sourceCommit",
    "installHook",
    "startHook",
    "log",
    "logTruncated",
    "errorCode",
];

fn lower_hex(value: &Value, lengths: &[usize]) -> bool {
    value.as_str().is_some_and(|s| {
        lengths.contains(&s.len())
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn uuid(value: &Value) -> bool {
    value.as_str().is_some_and(|s| {
        s.len() == 36
            && s.bytes().enumerate().all(|(i, b)| {
                if [8, 13, 18, 23].contains(&i) {
                    b == b'-'
                } else {
                    b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
                }
            })
    })
}

fn code(value: &Value) -> bool {
    value.as_str().is_some_and(|s| {
        !s.is_empty() && s.len() <= 80 && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
    })
}

fn phase(value: &Value) -> bool {
    value.as_str().is_some_and(|s| {
        matches!(
            s,
            "installing" | "starting" | "ready" | "failed" | "cancelled"
        )
    })
}

fn progress(value: &Value, phase_key: &str, code_key: &str) -> bool {
    phase(&value[phase_key])
        && value["log"].as_str().is_some_and(|s| s.len() <= 16384)
        && value["logTruncated"].is_boolean()
        && value["installHook"].is_boolean()
        && value["startHook"].is_boolean()
        && value.get(code_key).is_none_or(code)
        && value
            .get("sourceCommit")
            .is_none_or(|v| v.is_null() || lower_hex(v, &[40, 64]))
}

pub(super) fn valid_projection(value: &Value) -> bool {
    let allowed = [
        "schema",
        "projectId",
        "revisionId",
        "hookDigest",
        "stage",
        "detail",
        "log",
        "logTruncated",
        "installHook",
        "startHook",
        "sourceCommit",
    ];
    value.as_object().is_some_and(|o| {
        o.keys().all(|k| allowed.contains(&k.as_str()))
            && value["schema"] == 1
            && uuid(&value["projectId"])
            && lower_hex(&value["revisionId"], &[64])
            && value.get("hookDigest").is_none_or(|v| lower_hex(v, &[64]))
            && progress(value, "stage", "detail")
    })
}

pub(super) fn validate_build(
    value: &Value,
    run: &ManagedRun,
    grant: &RunEventDelivery,
) -> ApiResult<()> {
    let allowed = [
        "schema",
        "kind",
        "harness",
        "runId",
        "buildId",
        "projectId",
        "revisionId",
        "hookDigest",
        "requestHash",
        "bootId",
        "phase",
        "sourceCommit",
        "installHook",
        "startHook",
        "log",
        "logTruncated",
        "errorCode",
    ];
    if !value
        .as_object()
        .is_some_and(|o| o.keys().all(|k| allowed.contains(&k.as_str())))
        || value["schema"] != 1
        || value["kind"] != "project-environment"
        || value["harness"] != "environment"
        || value["runId"] != run.id
        || value["buildId"] != run.id
        || !uuid(&value["buildId"])
        || !uuid(&value["projectId"])
        || value
            .get("revisionId")
            .is_some_and(|v| !v.is_null() && !lower_hex(v, &[64]))
        || value
            .get("hookDigest")
            .is_some_and(|v| !v.is_null() && !lower_hex(v, &[64]))
        || value["requestHash"] != grant.request_hash
        || value["bootId"].as_str() != run.boot_id.as_deref()
        || run.boot_id.is_none()
        || !progress(value, "phase", "errorCode")
        || !bounded_json(value, 0)
        || value.to_string().len() > MAX_GUEST_LINE
    {
        return Err(invalid());
    }
    Ok(())
}

pub(super) fn unchanged_identity(previous: &Value, next: &Value) -> ApiResult<()> {
    if let Some(environment) = previous.get("environment") {
        let Some(current) = next.get("environment") else {
            return Err(invalid());
        };
        for key in ["schema", "projectId", "revisionId"] {
            if environment.get(key) != current.get(key) {
                return Err(invalid());
            }
        }
        unchanged_provenance(environment, current)?;
    }
    if previous["harness"] == "environment" {
        unchanged_provenance(previous, next)?;
    }
    Ok(())
}

fn unchanged_provenance(previous: &Value, next: &Value) -> ApiResult<()> {
    for key in ["sourceCommit", "revisionId", "hookDigest"] {
        if previous.get(key).is_some_and(Value::is_string) && previous.get(key) != next.get(key) {
            return Err(invalid());
        }
    }
    Ok(())
}

use crate::Result;
use reqwest::{blocking::Client, Method, Url};
use serde_json::Value;
use std::{io::Read, time::Duration};

#[derive(Clone)]
pub struct Api {
    client: Client,
    base: Url,
    token: String,
}

impl Api {
    pub fn new(endpoint: &str, token: String, timeout: u64) -> Result<Self> {
        let base = Url::parse(endpoint)?;
        if !matches!(base.scheme(), "http" | "https")
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || base.path() != "/"
        {
            return Err(
                "endpoint must be an http(s) origin without credentials, path, query or fragment"
                    .into(),
            );
        }
        if timeout == 0 {
            return Err("timeout must be positive".into());
        }
        Ok(Self {
            client: Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(timeout))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            base,
            token,
        })
    }

    pub fn stream_request(
        &self,
        id: &str,
        sid: &str,
        seq: u64,
    ) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        if self.token.is_empty() {
            return Err("set AHVM_TOKEN or --token-file to authenticate".into());
        }
        let mut url = self.base.clone();
        url.set_scheme(if self.base.scheme() == "https" {
            "wss"
        } else {
            "ws"
        })
        .map_err(|_| "bad WebSocket scheme")?;
        url.path_segments_mut()
            .map_err(|_| "invalid endpoint")?
            .extend(["v1", "sandboxes", id, "sessions", sid, "stream"]);
        url.query_pairs_mut()
            .append_pair("from_seq", &seq.to_string());
        let mut request = url.as_str().into_client_request()?;
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {}", self.token).parse()?);
        Ok(request)
    }

    pub fn call(
        &self,
        method: Method,
        path: &[&str],
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<Value> {
        if self.token.is_empty() && path != ["healthz"] {
            return Err("set AHVM_TOKEN or --token-file to authenticate".into());
        }
        let mut url = self.base.clone();
        // Each identifier is one encoded segment; slashes cannot change routes.
        url.path_segments_mut()
            .map_err(|_| "invalid endpoint")?
            .extend(std::iter::once("v1").chain(path.iter().copied()));
        if !query.is_empty() {
            url.query_pairs_mut()
                .extend_pairs(query.iter().map(|(k, v)| (*k, v)));
        }
        let mut request = self.client.request(method, url);
        if !self.token.is_empty() {
            request = request.bearer_auth(&self.token);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send()?;
        let status = response.status();
        // Bound a malformed/untrusted server's response. File downloads page.
        let mut bytes = Vec::new();
        response
            .take(16 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 16 * 1024 * 1024 {
            return Err("API response exceeds 16 MiB".into());
        }
        if !status.is_success() {
            let detail = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|v| {
                    v.get("message")
                        .or_else(|| v.get("error"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| {
                    String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]).into_owned()
                });
            return Err(format!("HTTP {status}: {detail}").into());
        }
        if bytes.is_empty() {
            Ok(Value::Null)
        } else {
            Ok(serde_json::from_slice(&bytes)?)
        }
    }
}

pub fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| format!("API response missing {key}").into())
}

pub fn exit_code(value: &Value) -> Result<i32> {
    let code = value["exit_code"]
        .as_i64()
        .ok_or("API response missing exit_code")?;
    Ok(if (0..=255).contains(&code) {
        code as i32
    } else {
        1
    })
}

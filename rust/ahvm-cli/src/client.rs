use crate::Result;
use reqwest::{blocking::Client, Method, Url};
use serde_json::Value;
use std::{io::Read, time::Duration};

#[derive(Clone)]
pub struct Api {
    client: Client,
    base: Url,
    token: String,
    cloud: bool,
    operation_key: Option<String>,
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
        let builder = if base.host_str() == Some("127.0.0.1") || base.host_str() == Some("[::1]") {
            Client::builder().no_proxy()
        } else {
            Client::builder()
        };
        Ok(Self {
            client: builder
                .user_agent(concat!("ahvm/", env!("CARGO_PKG_VERSION")))
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(timeout))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            base,
            token,
            cloud: false,
            operation_key: None,
        })
    }

    pub fn cloud(mut self, key: Option<String>) -> Result<Self> {
        if key.as_ref().is_some_and(|s| {
            !(16..=100).contains(&s.len())
                || !s
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        }) {
            return Err(
                "idempotency key must be 16..100 letters, digits, underscores or hyphens".into(),
            );
        }
        self.cloud = true;
        self.operation_key = key;
        Ok(self)
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

    pub fn desktop_request(
        &self,
        id: &str,
    ) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request> {
        let mut request = self.stream_request(id, "", 0)?;
        let mut url = self.base.clone();
        url.set_scheme(if self.base.scheme() == "https" {
            "wss"
        } else {
            "ws"
        })
        .map_err(|_| "bad WebSocket scheme")?;
        url.path_segments_mut()
            .map_err(|_| "invalid endpoint")?
            .extend(["v1", "sandboxes", id, "desktop", "stream"]);
        *request.uri_mut() = url.as_str().parse()?;
        Ok(request)
    }

    pub fn upload(&self, id: &str, path: &str, body: reqwest::blocking::Body) -> Result<Value> {
        if self.token.is_empty() {
            return Err("set AHVM_TOKEN or --token-file to authenticate".into());
        }
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|_| "invalid endpoint")?
            .extend(["v1", "sandboxes", id, "files", "upload"]);
        url.query_pairs_mut().append_pair("path", path);
        let response = self
            .client
            .put(url)
            .bearer_auth(&self.token)
            .header("Content-Type", "application/octet-stream")
            .body(body)
            .send()?;
        Self::response(response)
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
        let lifecycle = self.cloud
            && ((path == ["sandboxes"] && method == Method::POST)
                || (path.len() == 2 && path[0] == "sandboxes" && method == Method::DELETE)
                || (path.len() == 3
                    && path[0] == "sandboxes"
                    && matches!(path[2], "start" | "stop")
                    && method == Method::POST));
        let key = lifecycle.then(|| {
            self.operation_key
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
        });
        let mut request = self.client.request(method, url);
        if let Some(key) = &key {
            request = request.header("Idempotency-Key", key);
        }
        if !self.token.is_empty() {
            request = request.bearer_auth(&self.token);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().map_err(|error| -> Box<dyn std::error::Error + Send + Sync> {
            if let Some(key) = &key { format!("cloud request interrupted; retry the same command with --idempotency-key {key}: {error}").into() }
            else { error.into() }
        })?;
        if response.status() == reqwest::StatusCode::ACCEPTED && lifecycle {
            return Err(format!("cloud operation is still pending; inspect its status with ahvm --cloud get <name>. Retry this request using --idempotency-key {}", key.as_deref().unwrap_or_default()).into());
        }
        Self::response(response).map_err(|error| {
            if let Some(key) = key {
                format!("{error}; retry this lifecycle request with --idempotency-key {key}").into()
            } else {
                error
            }
        })
    }

    fn response(response: reqwest::blocking::Response) -> Result<Value> {
        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let seconds = response
                .headers()
                .get("Retry-After")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(60)
                .min(3600);
            return Err(
                format!("request rate limit reached; try again in {seconds} seconds").into(),
            );
        }
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

#[cfg(test)]
mod desktop_tests {
    use super::*;
    #[test]
    fn desktop_identifiers_cannot_change_routes_or_leak_token_into_url() {
        let api = Api::new("https://example.test", "private-test-token".into(), 30).unwrap();
        let request = api.desktop_request("dev/other?token=bad").unwrap();
        assert_eq!(request.uri().scheme_str(), Some("wss"));
        assert!(request.uri().path().contains("dev%2Fother%3Ftoken=bad"));
        assert_eq!(request.uri().query(), None);
        assert!(!request.uri().to_string().contains("private-test-token"));
        assert_eq!(
            request.headers()["Authorization"],
            "Bearer private-test-token"
        );
    }
}

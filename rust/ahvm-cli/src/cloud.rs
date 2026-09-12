//! AHVM cloud credentials are separate from self-hosted daemon/SSH credentials.
mod store;
use crate::Result;
use clap::{Args, ValueEnum};
use oauth2::{
    basic::BasicClient, AuthType, AuthUrl, AuthorizationCode, ClientId, CsrfToken,
    PkceCodeChallenge, RedirectUrl, RefreshToken, TokenResponse, TokenUrl,
};
use reqwest::{blocking::Client, Url};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use store::{Metadata, Store};

#[derive(Clone, Copy, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum StoreKind {
    Keyring,
    File,
}
#[derive(Args)]
pub struct Login {
    /// Use a device code instead of a callback to this computer (SSH/headless).
    #[arg(long)]
    device: bool,
    /// Print the approval link without opening a browser on this computer.
    #[arg(long)]
    no_browser: bool,
    /// Prefer the OS credential store. File is an explicit private-file fallback.
    #[arg(long, value_enum, default_value = "keyring")]
    credential_store: StoreKind,
    /// Cloud service origin (separate from the self-hosted --endpoint).
    #[arg(
        long,
        env = "AHVM_CLOUD_ENDPOINT",
        default_value = "https://dashboard.ahvm.app"
    )]
    cloud_endpoint: String,
}
// Do not derive Debug: these values must never appear in diagnostics.
#[derive(Serialize, Deserialize)]
struct Credentials {
    access_token: String,
    refresh_token: String,
    expires_at: u64,
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn origin(value: &str) -> Result<String> {
    let u = Url::parse(value)?;
    let local = u.scheme() == "http" && u.host_str() == Some("127.0.0.1");
    if (u.scheme() != "https" && !local)
        || u.host_str().is_none()
        || !u.username().is_empty()
        || u.password().is_some()
        || u.path() != "/"
        || u.query().is_some()
        || u.fragment().is_some()
    {
        return Err("cloud endpoint must be an HTTPS origin (HTTP is allowed only on 127.0.0.1 for local development)".into());
    }
    Ok(u.origin().ascii_serialization())
}
fn http() -> Result<Client> {
    Ok(Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("ahvm/", env!("CARGO_PKG_VERSION")))
        .build()?)
}
// Adapter for oauth2-rs, sharing the CLI's reqwest version and bounding provider bodies.
fn transport(
    request: oauth2::HttpRequest,
) -> std::result::Result<oauth2::HttpResponse, std::io::Error> {
    let run = || -> Result<oauth2::HttpResponse> {
        let response = http()?
            .request(request.method().clone(), request.uri().to_string())
            .headers(request.headers().clone())
            .body(request.into_body())
            .send()?;
        let status = response.status();
        let headers = response.headers().clone();
        let mut bytes = Vec::new();
        response.take(65537).read_to_end(&mut bytes)?;
        if bytes.len() > 65536 {
            return Err("cloud authentication response too large".into());
        }
        let mut result = oauth2::HttpResponse::new(bytes);
        *result.status_mut() = status;
        *result.headers_mut() = headers;
        Ok(result)
    };
    run().map_err(|_| std::io::Error::other("cloud authentication request failed"))
}
fn oauth(
    endpoint: &str,
) -> Result<
    oauth2::basic::BasicClient<
        oauth2::EndpointSet,
        oauth2::EndpointNotSet,
        oauth2::EndpointNotSet,
        oauth2::EndpointNotSet,
        oauth2::EndpointSet,
    >,
> {
    Ok(BasicClient::new(ClientId::new("ahvm-cli".into()))
        .set_auth_type(AuthType::RequestBody)
        .set_auth_uri(AuthUrl::new(format!("{endpoint}/oauth/authorize"))?)
        .set_token_uri(TokenUrl::new(format!("{endpoint}/oauth/token"))?))
}
fn credentials(token: oauth2::basic::BasicTokenResponse) -> Result<Credentials> {
    let access = token.access_token().secret().clone();
    let refresh = token
        .refresh_token()
        .ok_or("cloud did not issue a refresh credential")?
        .secret()
        .clone();
    let ttl = token
        .expires_in()
        .ok_or("cloud did not issue an access expiry")?
        .as_secs();
    if !is_secret(&access) || !is_secret(&refresh) || ttl == 0 || ttl > 3600 {
        return Err("invalid cloud credentials response".into());
    }
    Ok(Credentials {
        access_token: access,
        refresh_token: refresh,
        expires_at: now() + ttl,
    })
}
fn is_secret(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(url).status();
    #[cfg(target_os = "linux")]
    let result = std::process::Command::new("xdg-open").arg(url).status();
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let result: std::io::Result<std::process::ExitStatus> =
        Err(std::io::Error::other("unsupported platform"));
    if !matches!(result,Ok(s) if s.success()) {
        eprintln!("Open the link above in your browser, or use ahvm login --device from a headless machine.");
    }
}
pub fn login(options: &Login) -> Result<i32> {
    let endpoint = origin(&options.cloud_endpoint)?;
    let store = Store::open()?;
    if store.metadata()?.is_some() {
        return Err(
            "already signed in; use ahvm whoami or ahvm logout before switching accounts".into(),
        );
    }
    let creds = if options.device {
        device_login(&endpoint)?
    } else {
        browser_login(&endpoint, options.no_browser)?
    };
    let metadata = Metadata {
        endpoint,
        kind: options.credential_store,
    };
    if let Err(error) = store.save(&metadata, &creds) {
        let _ = revoke(&metadata.endpoint, &creds.refresh_token);
        return Err(error);
    }
    println!("Signed in to AHVM Cloud. Run ahvm whoami to see your workspace.");
    if matches!(options.credential_store, StoreKind::File) {
        eprintln!("Cloud credentials saved in private files under your AHVM config directory.");
    }
    Ok(0)
}
fn browser_login(endpoint: &str, no_browser: bool) -> Result<Credentials> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let address = listener.local_addr()?;
    listener.set_nonblocking(true)?;
    let redirect = format!("http://{address}/callback");
    let client = oauth(endpoint)?.set_redirect_uri(RedirectUrl::new(redirect)?);
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, state) = client
        .authorize_url(|| CsrfToken::new_random_len(32))
        .set_pkce_challenge(challenge)
        .url();
    eprintln!("Approve this CLI in your browser:\n{url}");
    if !no_browser {
        open_browser(url.as_str());
    }
    let deadline = Instant::now() + Duration::from_secs(600);
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((mut stream, _)) => {
                match callback(&mut stream, &address.to_string(), state.secret(), endpoint) {
                    Ok(Some(code)) => {
                        let result = client
                            .exchange_code(AuthorizationCode::new(code))
                            .set_pkce_verifier(verifier)
                            .request(&transport)
                            .map_err(|_| {
                                "cloud login could not be completed; please start ahvm login again"
                            })
                            .and_then(|t| {
                                credentials(t).map_err(|_| "invalid cloud credentials response")
                            });
                        reply(
                            &mut stream,
                            if result.is_ok() {
                                "CLI authorized. You can close this tab."
                            } else {
                                "Login failed. Return to the terminal and try again."
                            },
                        );
                        return result.map_err(Into::into);
                    }
                    Err(CallbackError::Denied) => {
                        reply(&mut stream, "Login cancelled.");
                        return Err("cloud login cancelled".into());
                    }
                    _ => reply(&mut stream, "Invalid callback. Return to your login tab."),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(30))
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err("cloud login timed out; retry, or use ahvm login --device".into())
}
enum CallbackError {
    Invalid,
    Denied,
}
fn callback(
    stream: &mut TcpStream,
    host: &str,
    state: &str,
    issuer: &str,
) -> std::result::Result<Option<String>, CallbackError> {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|_| CallbackError::Invalid)?;
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    // Bounded headers, short timeout per connection; never read an HTTP body.
    let deadline = Instant::now() + Duration::from_secs(2);
    while bytes.len() < 8192 && Instant::now() < deadline {
        if stream.read(&mut byte).map_err(|_| CallbackError::Invalid)? == 0 {
            break;
        }
        bytes.push(byte[0]);
        if bytes.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let request = std::str::from_utf8(&bytes).map_err(|_| CallbackError::Invalid)?;
    parse_callback(request, host, state, issuer)
}
fn parse_callback(
    request: &str,
    host: &str,
    state: &str,
    issuer: &str,
) -> std::result::Result<Option<String>, CallbackError> {
    if !request.ends_with("\r\n\r\n") {
        return Err(CallbackError::Invalid);
    }
    let mut lines = request.split("\r\n");
    let line = lines.next().ok_or(CallbackError::Invalid)?;
    let words: Vec<_> = line.split(' ').collect();
    if words.len() != 3
        || words[0] != "GET"
        || !words[1].starts_with("/callback?")
        || words[2] != "HTTP/1.1"
    {
        return Err(CallbackError::Invalid);
    }
    let hosts: Vec<_> = lines
        .filter_map(|l| l.split_once(':'))
        .filter(|(k, _)| k.eq_ignore_ascii_case("host"))
        .collect();
    if hosts.len() != 1 || hosts[0].1.trim() != host {
        return Err(CallbackError::Invalid);
    }
    let url =
        Url::parse(&format!("http://{host}{}", words[1])).map_err(|_| CallbackError::Invalid)?;
    let get = |key: &str| -> std::result::Result<String, CallbackError> {
        let values: Vec<_> = url
            .query_pairs()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
            .collect();
        if values.len() != 1 {
            return Err(CallbackError::Invalid);
        }
        Ok(values[0].clone())
    };
    if get("state")? != state || get("iss")? != issuer {
        return Err(CallbackError::Invalid);
    }
    if url.query_pairs().any(|(k, _)| k == "error") {
        return Err(CallbackError::Denied);
    }
    let code = get("code")?;
    if !is_secret(&code) {
        return Err(CallbackError::Invalid);
    }
    Ok(Some(code))
}
fn reply(stream: &mut TcpStream, message: &str) {
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let body = format!("<!doctype html><title>AHVM Cloud</title><p>{message}</p>");
    let _=write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'none'; frame-ancestors 'none'\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",body.len(),body);
}
fn json_response(response: reqwest::blocking::Response) -> Result<serde_json::Value> {
    let mut bytes = Vec::new();
    response.take(65537).read_to_end(&mut bytes)?;
    if bytes.len() > 65536 {
        return Err("cloud response too large".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| "invalid cloud response".into())
}
fn device_login(endpoint: &str) -> Result<Credentials> {
    let http = http()?;
    let response = http
        .post(format!("{endpoint}/oauth/device"))
        .form(&[("client_id", "ahvm-cli")])
        .send()?;
    if !response.status().is_success() {
        return Err("could not start cloud device login".into());
    }
    let body = json_response(response)?;
    let device = body["device_code"]
        .as_str()
        .filter(|s| is_secret(s))
        .ok_or("invalid device login response")?;
    let uri = body["verification_uri"]
        .as_str()
        .ok_or("missing device verification URI")?;
    if uri != format!("{endpoint}/cli/device") {
        return Err("unexpected device verification URI".into());
    }
    let code = body["user_code"].as_str().ok_or("missing device code")?;
    if code.len() != 9
        || !code
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err("invalid device code".into());
    }
    eprintln!("Open {uri} in a browser and enter code {code}.\nOnly approve the code shown in this terminal.");
    let deadline = Instant::now() + Duration::from_secs(600);
    let mut interval = 5;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(interval));
        let response = http
            .post(format!("{endpoint}/oauth/token"))
            .form(&[
                ("client_id", "ahvm-cli"),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", device),
            ])
            .send()?;
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            interval = response
                .headers()
                .get("Retry-After")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(60)
                .clamp(5, 600);
            if Instant::now() + Duration::from_secs(interval) >= deadline {
                break;
            }
            continue;
        }
        let ok = response.status().is_success();
        let result = json_response(response)?;
        if ok {
            let tokens: oauth2::basic::BasicTokenResponse =
                serde_json::from_value(result).map_err(|_| "invalid cloud credentials response")?;
            return credentials(tokens);
        }
        match result["error"].as_str() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval = (interval + 5).min(60),
            Some("access_denied") => return Err("cloud login cancelled".into()),
            _ => return Err("cloud device login expired or failed; please retry".into()),
        }
    }
    Err("cloud device login timed out".into())
}
fn refresh(metadata: &Metadata, store: &Store, creds: &mut Credentials) -> Result<()> {
    let token = oauth(&metadata.endpoint)?
        .exchange_refresh_token(&RefreshToken::new(creds.refresh_token.clone()))
        .request(&transport)
        .map_err(|_| {
            "cloud credential refresh failed; try again later. If the session has expired, run ahvm login"
        })?;
    let next = credentials(token)?;
    if let Err(error) = store.save(metadata, &next) {
        let _ = revoke(&metadata.endpoint, &next.refresh_token);
        return Err(error);
    }
    *creds = next;
    Ok(())
}
/// The credential lock is released before any VM RPC or interactive stream.
pub fn connection() -> Result<(String, String)> {
    let store = Store::open()?;
    let metadata = store.metadata()?.ok_or("not signed in; run ahvm login")?;
    origin(&metadata.endpoint)?;
    let mut creds = store.load(&metadata)?;
    if creds.expires_at <= now() + 30 {
        refresh(&metadata, &store, &mut creds)?;
    }
    Ok((metadata.endpoint, creds.access_token))
}
pub fn whoami(json: bool) -> Result<i32> {
    let store = Store::open()?;
    let metadata = store.metadata()?.ok_or("not signed in; run ahvm login")?;
    origin(&metadata.endpoint)?;
    let mut creds = store.load(&metadata)?;
    if creds.expires_at <= now() + 30 {
        refresh(&metadata, &store, &mut creds)?;
    }
    let response = http()?
        .get(format!("{}/api/me", metadata.endpoint))
        .bearer_auth(&creds.access_token)
        .send()?;
    if !response.status().is_success() {
        return Err("cloud login is no longer authorized; run ahvm logout and ahvm login".into());
    }
    let me = json_response(response)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&me)?);
    } else {
        println!(
            "Signed in as {}",
            me["user"]["display_name"].as_str().unwrap_or("unknown")
        );
        if let Some(orgs) = me["organizations"].as_array() {
            for org in orgs {
                println!(
                    "Workspace: {} ({})",
                    org["name"].as_str().unwrap_or("unknown"),
                    org["id"].as_str().unwrap_or("unknown")
                );
            }
        }
    }
    Ok(0)
}
fn revoke(endpoint: &str, token: &str) -> Result<()> {
    let response = http()?
        .post(format!("{endpoint}/oauth/revoke"))
        .form(&[("client_id", "ahvm-cli"), ("token", token)])
        .send()?;
    if !response.status().is_success() {
        return Err(
            "could not revoke cloud login; local credentials retained so you can retry".into(),
        );
    }
    Ok(())
}
pub fn logout() -> Result<i32> {
    let store = Store::open()?;
    let Some(metadata) = store.metadata()? else {
        println!("Not signed in to AHVM Cloud.");
        return Ok(0);
    };
    origin(&metadata.endpoint)?;
    let creds = store.load(&metadata)?;
    revoke(&metadata.endpoint, &creds.refresh_token)?;
    store.clear(&metadata)?;
    println!("Signed out of AHVM Cloud. Self-hosted connections are unchanged.");
    Ok(0)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn callback_checks_state_issuer_host_and_duplicates() {
        let code = "x".repeat(43);
        let target =
            format!("/callback?code={code}&state=state&iss=https%3A%2F%2Fdashboard.ahvm.app");
        let request = format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1:9999\r\n\r\n");
        assert!(parse_callback(
            &request,
            "127.0.0.1:9999",
            "state",
            "https://dashboard.ahvm.app"
        )
        .is_ok());
        for bad in [
            request.replace("state=state", "state=wrong"),
            request.replace("state=state", "state=state&state=state"),
            request.replace("dashboard.ahvm.app", "evil.test"),
            request.replace("Host: 127.0.0.1:9999", "Host: evil.test"),
        ] {
            assert!(parse_callback(
                &bad,
                "127.0.0.1:9999",
                "state",
                "https://dashboard.ahvm.app"
            )
            .is_err());
        }
    }
    #[test]
    fn endpoint_cannot_send_credentials_over_remote_http_or_to_a_path() {
        for value in [
            "http://evil.test",
            "https://a.test/path",
            "https://user@a.test",
            "https://a.test/?token=x",
        ] {
            assert!(origin(value).is_err());
        }
        assert_eq!(
            origin("https://dashboard.ahvm.app/").unwrap(),
            "https://dashboard.ahvm.app"
        );
    }
}

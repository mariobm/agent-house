//! Private, path-style S3 transport for the bounded protocol experiment.
//! No credential discovery, redirects, proxy inheritance or automatic retries.
use crate::{digest, valid_id, Error, Head, ObjectStore, Result, CHUNK_BYTES, MAX_MANIFEST_BYTES};
use aws_credential_types::Credentials;
use aws_sigv4::{
    http_request::{sign, PayloadChecksumKind, SignableBody, SignableRequest, SigningSettings},
    sign::v4,
};
use reqwest::{
    blocking::{Client, Response},
    header, Method, StatusCode, Url,
};
use serde::Deserialize;
use std::{
    fs::File,
    io::Read,
    path::Path,
    time::{Duration, SystemTime},
};

/// Explicit credentials only; never load an ambient AWS profile or metadata service.
/// Endpoint is a private HTTPS S3 API origin, not a CDN or public bucket URL.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    #[serde(default)]
    pub session_token: Option<String>,
}
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("S3Config([redacted])")
    }
}
impl Config {
    /// On Unix, reject credentials accessible by group/other users. Limit input
    /// before parsing and do not include file contents or parser errors in errors.
    pub fn from_file(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|_| Error::Store)?;
        let metadata = file.metadata().map_err(|_| Error::Store)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(Error::InvalidInput);
            }
        }
        if !metadata.is_file() || metadata.len() > 16384 {
            return Err(Error::InvalidInput);
        }
        let mut bytes = Vec::new();
        file.take(16385)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::Store)?;
        if bytes.len() > 16384 {
            return Err(Error::InvalidInput);
        }
        serde_json::from_slice(&bytes).map_err(|_| Error::InvalidInput)
    }
}

pub struct S3Store {
    config: Config,
    client: Client,
}
impl std::fmt::Debug for S3Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("S3Store([redacted])")
    }
}
impl S3Store {
    pub fn new(config: Config) -> Result<Self> {
        Self::build(config, false, Duration::from_secs(20))
    }
    fn build(config: Config, test_http: bool, timeout: Duration) -> Result<Self> {
        let url = Url::parse(&config.endpoint).map_err(|_| Error::InvalidInput)?;
        let allowed_scheme = url.scheme() == "https"
            || (test_http && url.scheme() == "http" && url.host_str() == Some("127.0.0.1"));
        if !allowed_scheme
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
            || !valid_id(&config.region)
            || config.bucket.len() < 3
            || config.bucket.len() > 63
            || !config
                .bucket
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
            || config.bucket.starts_with('-')
            || config.bucket.ends_with('-')
            || config.prefix.len() > 256
            || !config.prefix.split('/').all(valid_id)
            || config.access_key_id.is_empty()
            || config.secret_access_key.is_empty()
            || config.access_key_id.len() > 256
            || config.secret_access_key.len() > 1024
            || config
                .session_token
                .as_ref()
                .is_some_and(|s| s.is_empty() || s.len() > 8192)
        {
            return Err(Error::InvalidInput);
        }
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(5))
            .timeout(timeout)
            .build()
            .map_err(|_| Error::Store)?;
        Ok(Self { config, client })
    }
    fn key(&self, volume: &str, suffix: &str) -> Result<String> {
        if !valid_id(volume) {
            return Err(Error::InvalidInput);
        }
        Ok(format!(
            "{}/{}/{}/{}/{}",
            self.config.endpoint.trim_end_matches('/'),
            self.config.bucket,
            self.config.prefix,
            volume,
            suffix
        ))
    }
    fn request(
        &self,
        method: Method,
        url: &str,
        condition: Option<(&str, &str)>,
        body: &[u8],
    ) -> Result<Response> {
        let identity = Credentials::new(
            &self.config.access_key_id,
            &self.config.secret_access_key,
            self.config.session_token.clone(),
            None,
            "ahvm-volume-file",
        )
        .into();
        let mut settings = SigningSettings::default();
        settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
        let params = v4::SigningParams::builder()
            .identity(&identity)
            .region(&self.config.region)
            .name("s3")
            .time(SystemTime::now())
            .settings(settings)
            .build()
            .map_err(|_| Error::Store)?
            .into();
        let signable = SignableRequest::new(
            method.as_str(),
            url,
            condition.into_iter(),
            SignableBody::Bytes(body),
        )
        .map_err(|_| Error::Store)?;
        let (instructions, _) = sign(signable, &params)
            .map_err(|_| Error::Store)?
            .into_parts();
        let mut request = self.client.request(method, url).body(body.to_vec());
        if let Some((name, value)) = condition {
            request = request.header(name, value);
        }
        for (name, value) in instructions.headers() {
            request = request.header(name, value);
        }
        request.send().map_err(|_| Error::Store)
    }
    fn get(&self, url: &str, limit: usize) -> Result<Option<(String, Vec<u8>)>> {
        let response = self.request(Method::GET, url, None, &[])?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if response.status() != StatusCode::OK {
            return Err(Error::Store);
        }
        let revision = etag(&response)?;
        if response
            .content_length()
            .is_some_and(|len| len > limit as u64)
        {
            return Err(Error::Corrupt);
        }
        let mut bytes = Vec::new();
        response
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::Store)?;
        if bytes.len() > limit {
            return Err(Error::Corrupt);
        }
        Ok(Some((revision, bytes)))
    }
}
fn etag(response: &Response) -> Result<String> {
    let tag = response
        .headers()
        .get(header::ETAG)
        .and_then(|v| v.to_str().ok())
        .ok_or(Error::Corrupt)?;
    if !valid_etag(tag) {
        return Err(Error::Corrupt);
    }
    Ok(tag.to_owned())
}
fn valid_etag(tag: &str) -> bool {
    tag.len() >= 2
        && tag.len() <= 256
        && tag.starts_with('"')
        && tag.ends_with('"')
        && tag[1..tag.len() - 1]
            .bytes()
            .all(|b| b >= 0x21 && b != b'"' && b < 0x7f)
}
fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
impl ObjectStore for S3Store {
    fn head(&self, volume: &str) -> Result<Option<Head>> {
        Ok(self
            .get(&self.key(volume, "head.json")?, MAX_MANIFEST_BYTES)?
            .map(|(revision, manifest)| Head { revision, manifest }))
    }
    fn chunk(&self, volume: &str, hash: &str) -> Result<Vec<u8>> {
        if !valid_hash(hash) {
            return Err(Error::InvalidInput);
        }
        let (_, bytes) = self
            .get(&self.key(volume, &format!("chunks/{hash}"))?, CHUNK_BYTES)?
            .ok_or(Error::Corrupt)?;
        if bytes.len() != CHUNK_BYTES || digest(&bytes) != hash {
            return Err(Error::Corrupt);
        }
        Ok(bytes)
    }
    fn put_chunk(&self, volume: &str, hash: &str, bytes: &[u8]) -> Result<()> {
        if bytes.len() != CHUNK_BYTES || !valid_hash(hash) || digest(bytes) != hash {
            return Err(Error::InvalidInput);
        }
        let response = self.request(
            Method::PUT,
            &self.key(volume, &format!("chunks/{hash}"))?,
            Some(("if-none-match", "*")),
            bytes,
        )?;
        match response.status() {
            StatusCode::OK => Ok(()),
            StatusCode::PRECONDITION_FAILED => {
                // A matching key alone is not proof that the existing bytes are valid.
                if self.chunk(volume, hash)? != bytes {
                    return Err(Error::Corrupt);
                }
                Ok(())
            }
            _ => Err(Error::Store),
        }
    }
    fn publish(&self, volume: &str, expected: Option<&str>, manifest: &[u8]) -> Result<String> {
        if manifest.is_empty()
            || manifest.len() > MAX_MANIFEST_BYTES
            || expected.is_some_and(|s| !valid_etag(s))
        {
            return Err(Error::InvalidInput);
        }
        let condition = expected
            .map(|v| ("if-match", v))
            .unwrap_or(("if-none-match", "*"));
        let response = self
            .request(
                Method::PUT,
                &self.key(volume, "head.json")?,
                Some(condition),
                manifest,
            )
            .map_err(|_| Error::Uncertain)?;
        match response.status() {
            StatusCode::OK => etag(&response).map_err(|_| Error::Uncertain),
            StatusCode::PRECONDITION_FAILED => Err(Error::Conflict),
            _ => Err(Error::Uncertain),
        }
    }
}

#[cfg(test)]
mod tests;

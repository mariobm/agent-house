//! Signed release metadata and bounded, verified downloads.
use crate::Result;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Write},
    path::Path,
    time::Duration,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Artifact {
    pub version: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub unpacked_size: u64,
    pub guest_abi: u32,
    #[serde(default)]
    pub state_abi: u32,
}
#[derive(Deserialize)]
pub struct Catalog {
    pub expires: u64,
    pub images: std::collections::BTreeMap<String, Artifact>,
    #[serde(default)]
    pub cli: std::collections::BTreeMap<String, Artifact>,
    #[serde(default)]
    pub client: std::collections::BTreeMap<String, Artifact>,
    #[serde(default)]
    pub server: std::collections::BTreeMap<String, Artifact>,
}
fn client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(1200))
        .https_only(true)
        .build()?)
}
pub fn catalog(url: &str) -> Result<Catalog> {
    let response = client()?
        .get(url)
        .timeout(Duration::from_secs(10))
        .send()?
        .error_for_status()?;
    let mut bytes = Vec::new();
    response.take(262145).read_to_end(&mut bytes)?;
    if bytes.len() > 262144 {
        return Err("catalog exceeds size limit".into());
    }
    verify(&bytes)
}
fn verify(bytes: &[u8]) -> Result<Catalog> {
    let pem = include_str!("../../../packaging/keys/releases.pem");
    let der = B64.decode(
        pem.lines()
            .filter(|l| !l.starts_with('-'))
            .collect::<String>(),
    )?;
    // Ed25519 SubjectPublicKeyInfo: fixed algorithm identifier + 32-byte key.
    if der.len() != 44 || der[..12] != [48, 42, 48, 5, 6, 3, 43, 101, 112, 3, 33, 0] {
        return Err("invalid embedded release key".into());
    }
    verify_with_key(bytes, &VerifyingKey::from_bytes(der[12..].try_into()?)?)
}
fn verify_with_key(bytes: &[u8], key: &VerifyingKey) -> Result<Catalog> {
    #[derive(Deserialize)]
    struct Envelope {
        payload: String,
        signature: String,
    }
    let envelope: Envelope = serde_json::from_slice(bytes)?;
    let payload = B64.decode(envelope.payload)?;
    let sig = Signature::from_slice(&B64.decode(envelope.signature)?)?;
    key.verify_strict(&payload, &sig)?;
    let catalog: Catalog = serde_json::from_slice(&payload)?;
    if catalog.expires <= now() {
        return Err("release catalog expired; refresh it before downloading".into());
    }
    Ok(catalog)
}
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub fn download(artifact: &Artifact, file: &mut File) -> Result<()> {
    if artifact.size == 0
        || artifact.size > 4 * 1024 * 1024 * 1024
        || artifact.sha256.len() != 64
        || !artifact.sha256.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err("invalid artifact metadata".into());
    }
    let mut response = client()?
        .get(&artifact.url)
        .send()?
        .error_for_status()?
        .take(artifact.size + 1);
    let mut digest = Sha256::new();
    let mut total = 0;
    let mut buf = [0u8; 131072];
    loop {
        let n = response.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > artifact.size {
            return Err("download exceeds signed size".into());
        }
        digest.update(&buf[..n]);
        file.write_all(&buf[..n])?;
    }
    if total != artifact.size || format!("{:x}", digest.finalize()) != artifact.sha256 {
        return Err("artifact checksum/size mismatch".into());
    }
    file.sync_all()?;
    Ok(())
}
pub fn unpack_gzip(source: &Path, output: &mut File, size: u64) -> Result<()> {
    use std::io::{Seek, SeekFrom};
    if size == 0 || size > 64 * 1024 * 1024 * 1024 {
        return Err("invalid unpacked size".into());
    }
    let mut input = flate2::read::GzDecoder::new(File::open(source)?).take(size + 1);
    let mut total = 0;
    let mut buffer = [0u8; 131072];
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > size {
            return Err("unpacked size exceeds signed limit".into());
        }
        if buffer[..n].iter().all(|b| *b == 0) {
            output.seek(SeekFrom::Current(n as i64))?;
        } else {
            output.write_all(&buffer[..n])?;
        }
    }
    if total != size {
        return Err("unpacked size mismatch".into());
    }
    output.set_len(size)?;
    output.sync_all()?;
    Ok(())
}

pub fn catalog_url() -> String {
    std::env::var("AHVM_CATALOG_URL")
        .unwrap_or_else(|_| "https://images.ahvm.app/catalog.json".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn signature_binds_content_key_and_expiry() {
        use ed25519_dalek::{Signer, SigningKey};
        let key = SigningKey::from_bytes(&[7; 32]);
        let envelope = |payload: &[u8]| {
            serde_json::to_vec(&serde_json::json!({"payload": B64.encode(payload), "signature": B64.encode(key.sign(payload).to_bytes())})).unwrap()
        };
        let payload = format!("{{\"expires\":{},\"images\":{{}}}}", now() + 3600);
        let signed = envelope(payload.as_bytes());
        assert!(verify_with_key(&signed, &key.verifying_key()).is_ok());
        assert!(
            verify_with_key(&signed, &SigningKey::from_bytes(&[8; 32]).verifying_key()).is_err()
        );
        let mut tampered: serde_json::Value = serde_json::from_slice(&signed).unwrap();
        tampered["payload"] = B64.encode(b"{}").into();
        assert!(verify_with_key(
            &serde_json::to_vec(&tampered).unwrap(),
            &key.verifying_key()
        )
        .is_err());
        assert!(verify_with_key(
            &envelope(br#"{"expires":1,"images":{}}"#),
            &key.verifying_key()
        )
        .is_err());
    }
    #[test]
    fn unsigned_catalog_is_rejected() {
        assert!(verify(br#"{"payload":"e30=","signature":"AA=="}"#).is_err());
    }
    #[test]
    fn decompression_enforces_signed_size() {
        let mut source = tempfile::NamedTempFile::new().unwrap();
        let mut encoder = flate2::write::GzEncoder::new(&mut source, flate2::Compression::fast());
        encoder.write_all(b"1234").unwrap();
        encoder.finish().unwrap();
        let mut output = tempfile::tempfile().unwrap();
        assert!(unpack_gzip(source.path(), &mut output, 3).is_err());
    }
}

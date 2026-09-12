use super::*;
use std::{
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    thread,
};

fn config(endpoint: String) -> Config {
    Config {
        endpoint,
        region: "auto".into(),
        bucket: "test-bucket".into(),
        prefix: "qualification".into(),
        access_key_id: "test-key".into(),
        secret_access_key: "test-secret".into(),
        session_token: None,
    }
}
/// Minimal scripted HTTP peer. Each response gets a new connection; records the
/// actual wire request and consumes PUT bytes before injecting a lost response.
fn server(responses: Vec<Vec<u8>>) -> (S3Store, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let worker = thread::spawn(move || {
        let mut requests = Vec::new();
        for response in responses {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(stream);
            let mut request = String::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                assert!(!line.is_empty());
                if let Some(s) = line.to_lowercase().strip_prefix("content-length:") {
                    length = s.trim().parse().unwrap();
                }
                request.push_str(&line);
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            requests.push(request);
            reader.get_mut().write_all(&response).unwrap();
        }
        requests
    });
    (
        S3Store::build(config(endpoint), true, Duration::from_secs(2)).unwrap(),
        worker,
    )
}
fn response(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n",
        body.len()
    )
    .into_bytes();
    out.extend(body);
    out
}
#[test]
fn signs_preconditions_and_keeps_opaque_etags() {
    let (store, worker) = server(vec![
        response("200 OK", "ETag: \"opaque-1\"\r\n", &[]),
        response("412 Precondition Failed", "", &[]),
    ]);
    assert_eq!(store.publish("disk", None, b"{}").unwrap(), "\"opaque-1\"");
    assert!(matches!(
        store.publish("disk", Some("\"opaque-1\""), b"{}"),
        Err(Error::Conflict)
    ));
    let requests = worker.join().unwrap();
    assert!(requests[0].starts_with("PUT /test-bucket/qualification/disk/head.json "));
    assert!(requests[0].contains("if-none-match: *"));
    assert!(requests[1].contains("if-match: \"opaque-1\""));
    for r in requests {
        assert!(r.contains("AWS4-HMAC-SHA256"));
        assert!(r.contains("x-amz-content-sha256:"));
        assert!(!r.contains("test-secret"));
    }
}
#[test]
fn lost_or_bad_publication_responses_are_uncertain() {
    for reply in [
        Vec::new(),
        response("200 OK", "", &[]),
        response("503 Unavailable", "", &[]),
    ] {
        let (store, worker) = server(vec![reply]);
        assert!(matches!(
            store.publish("disk", None, b"{}"),
            Err(Error::Uncertain)
        ));
        assert_eq!(worker.join().unwrap().len(), 1);
    }
}
#[test]
fn refuses_redirects_and_does_not_read_error_bodies() {
    let (store, worker) = server(vec![response(
        "307 Redirect",
        "Location: http://127.0.0.1:1/steal\r\n",
        b"secret-server-message",
    )]);
    assert!(matches!(store.head("disk"), Err(Error::Store)));
    worker.join().unwrap();
}
#[test]
fn bounded_get_handles_declared_and_streamed_oversize() {
    let responses = [
        format!(
            "HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nContent-Length: {}\r\n\r\n",
            MAX_MANIFEST_BYTES + 1
        )
        .into_bytes(),
        {
            let mut out = b"HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n".to_vec();
            out.extend(vec![b'x'; MAX_MANIFEST_BYTES + 1]);
            out
        },
    ];
    for reply in responses {
        let (store, worker) = server(vec![reply]);
        assert!(matches!(store.head("disk"), Err(Error::Corrupt)));
        worker.join().unwrap();
    }
}
#[test]
fn duplicate_chunk_is_verified_and_missing_data_fails_closed() {
    let bytes = vec![7; CHUNK_BYTES];
    let hash = digest(&bytes);
    let (store, worker) = server(vec![
        response("412 Precondition Failed", "", &[]),
        response("200 OK", "ETag: \"data\"\r\n", &bytes),
        response("404 Not Found", "", &[]),
    ]);
    store.put_chunk("disk", &hash, &bytes).unwrap();
    assert!(matches!(store.chunk("disk", &hash), Err(Error::Corrupt)));
    worker.join().unwrap();
    let (store, worker) = server(vec![
        response("412 Precondition Failed", "", &[]),
        response("200 OK", "ETag: \"data\"\r\n", &vec![8; CHUNK_BYTES]),
    ]);
    assert!(matches!(
        store.put_chunk("disk", &hash, &bytes),
        Err(Error::Corrupt)
    ));
    worker.join().unwrap();
}
#[test]
fn missing_head_is_distinct_from_permission_failure() {
    let (store, worker) = server(vec![
        response("404 Not Found", "", &[]),
        response("403 Forbidden", "", &[]),
    ]);
    assert!(store.head("disk").unwrap().is_none());
    assert!(matches!(store.head("disk"), Err(Error::Store)));
    worker.join().unwrap();
}
#[test]
fn config_rejects_unsafe_urls_keys_and_redacts_debug() {
    for endpoint in [
        "http://example.com",
        "https://user:password@example.com",
        "https://example.com/path",
        "https://example.com/?query",
        "https://example.com/#fragment",
    ] {
        assert!(matches!(
            S3Store::new(config(endpoint.into())),
            Err(Error::InvalidInput)
        ));
    }
    for prefix in ["", "../escape", "a//b", "a/%2F", "/a"] {
        let mut c = config("https://example.com".into());
        c.prefix = prefix.into();
        assert!(matches!(S3Store::new(c), Err(Error::InvalidInput)));
    }
    let c = config("https://example.com".into());
    assert!(!format!("{c:?}").contains("test-secret"));
    let store = S3Store::new(c).unwrap();
    assert!(!format!("{store:?}").contains("test-key"));
    assert!(matches!(store.head("../disk"), Err(Error::InvalidInput)));
    assert!(matches!(
        store.publish("disk", Some("*"), b"{}"),
        Err(Error::InvalidInput)
    ));
    assert!(matches!(
        store.chunk("disk", "../data"),
        Err(Error::InvalidInput)
    ));
}
#[test]
fn slow_response_hits_total_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let store = S3Store::build(
        config(format!("http://{}", listener.local_addr().unwrap())),
        true,
        Duration::from_millis(50),
    )
    .unwrap();
    let worker = thread::spawn(move || {
        let (_stream, _) = listener.accept().unwrap();
        thread::sleep(Duration::from_millis(150));
    });
    assert!(matches!(store.head("disk"), Err(Error::Store)));
    worker.join().unwrap();
}
#[cfg(unix)]
#[test]
fn credential_file_requires_private_permissions_and_is_bounded() {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::temp_dir().join(format!("ahvm-volume-config-{}", std::process::id()));
    std::fs::write(&path, b"{}").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(Config::from_file(&path), Err(Error::InvalidInput)));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(&path, vec![b' '; 16385]).unwrap();
    assert!(matches!(Config::from_file(&path), Err(Error::InvalidInput)));
    std::fs::remove_file(path).unwrap();
}

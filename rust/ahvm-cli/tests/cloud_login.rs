use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use reqwest::Url;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    process::{Command, Stdio},
    time::Duration,
};

fn cli(dir: &std::path::Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_ahvm"));
    c.env("AHVM_CONFIG_DIR", dir)
        .env_remove("AHVM_CLOUD_ENDPOINT");
    c
}
fn receive(listener: &TcpListener) -> (TcpStream, String, String) {
    let (mut stream, _) = listener.accept().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut reader = BufReader::new(&mut stream);
    let mut headers = String::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        headers.push_str(&line);
        if line == "\r\n" {
            break;
        }
    }
    let len = headers
        .lines()
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .map(|(_, v)| v.trim().parse::<usize>().unwrap())
        .unwrap_or(0);
    let mut body = vec![0; len];
    reader.read_exact(&mut body).unwrap();
    (stream, headers, String::from_utf8(body).unwrap())
}
fn send(mut s: TcpStream, body: serde_json::Value) {
    let text = body.to_string();
    write!(s,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",text.len(),text).unwrap();
}
fn token(a: char, r: char) -> serde_json::Value {
    json!({"access_token":a.to_string().repeat(43),"refresh_token":r.to_string().repeat(43),"expires_in":900,"token_type":"Bearer"})
}
#[test]
fn browser_pkce_roundtrip_ignores_invalid_callback_and_preserves_hosts() {
    let temp = tempfile::tempdir().unwrap();
    let hosts = r#"{"default":"home","hosts":{"home":{"ssh":"example","api_port":8080}}}"#;
    std::fs::write(temp.path().join("hosts.json"), hosts).unwrap();
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", server.local_addr().unwrap());
    let mut child = cli(temp.path())
        .args([
            "login",
            "--no-browser",
            "--credential-store",
            "file",
            "--cloud-endpoint",
            &origin,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();
    stderr.read_line(&mut line).unwrap();
    line.clear();
    stderr.read_line(&mut line).unwrap();
    let authorize = Url::parse(line.trim()).unwrap();
    let params: std::collections::HashMap<_, _> = authorize
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let redirect = Url::parse(&params["redirect_uri"]).unwrap();
    let mut callback = redirect.clone();
    callback
        .query_pairs_mut()
        .append_pair("state", &params["state"])
        .append_pair("iss", &origin)
        .append_pair("code", &"c".repeat(43));
    let host = format!("127.0.0.1:{}", redirect.port().unwrap());
    let invalid = reqwest::blocking::Client::new()
        .get(format!("{redirect}?state=invalid"))
        .send()
        .unwrap();
    assert!(invalid.status().is_success());
    let h = std::thread::spawn(move || {
        let (s, headers, body) = receive(&server);
        assert!(headers.starts_with("POST /oauth/token "));
        let fields: std::collections::HashMap<_, _> = Url::parse(&format!("http://local/?{body}"))
            .unwrap()
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(fields["client_id"], "ahvm-cli");
        assert_eq!(fields["grant_type"], "authorization_code");
        assert_eq!(fields["redirect_uri"], redirect.as_str());
        assert_eq!(
            URL_SAFE_NO_PAD.encode(Sha256::digest(fields["code_verifier"].as_bytes())),
            params["code_challenge"]
        );
        send(s, token('a', 'r'));
    });
    let mut s = TcpStream::connect(host).unwrap();
    write!(
        s,
        "GET {}?{} HTTP/1.1\r\nHost: {}\r\n\r\n",
        callback.path(),
        callback.query().unwrap(),
        callback.authority()
    )
    .unwrap();
    let mut response = String::new();
    s.read_to_string(&mut response).unwrap();
    assert!(response.contains("CLI authorized"));
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    h.join().unwrap();
    assert_eq!(
        std::fs::read_to_string(temp.path().join("hosts.json")).unwrap(),
        hosts
    );
    assert!(!String::from_utf8(output.stdout)
        .unwrap()
        .contains(&"r".repeat(43)));
}
#[test]
fn device_login_refresh_whoami_logout_leave_self_hosted_config_untouched() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("hosts.json"), "self-hosted sentinel").unwrap();
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", server.local_addr().unwrap());
    let server_origin = origin.clone();
    let h = std::thread::spawn(move || {
        let (s, headers, _) = receive(&server);
        assert!(headers.starts_with("POST /oauth/device "));
        send(
            s,
            json!({"device_code":"d".repeat(43),"user_code":"ABCD-EFGH","verification_uri":format!("{server_origin}/cli/device"),"expires_in":600,"interval":5}),
        );
        let (s, headers, body) = receive(&server);
        assert!(headers.starts_with("POST /oauth/token "));
        assert!(body.contains("device_code"));
        send(s, token('a', 'r'));
        let (s, _, body) = receive(&server);
        assert!(body.contains("grant_type=refresh_token"));
        assert!(body.contains(&format!("refresh_token={}", "r".repeat(43))));
        send(s, token('b', 's'));
        let (s, headers, _) = receive(&server);
        assert!(headers.starts_with("GET /api/me "));
        assert!(headers.contains(&format!("Bearer {}", "b".repeat(43))));
        send(
            s,
            json!({"user":{"display_name":"Tester"},"organizations":[{"id":"org","name":"Workspace"}]}),
        );
        let (mut s, headers, _) = receive(&server);
        assert!(headers.starts_with("GET /v1/sandboxes?limit=100 "));
        assert!(headers.contains(&format!("Bearer {}", "b".repeat(43))));
        write!(s,"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 17\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        drop(s);
        for pending in [true, false] {
            let (mut s, headers, body) = receive(&server);
            assert!(headers.starts_with("POST /v1/sandboxes "));
            assert!(headers
                .to_lowercase()
                .contains("idempotency-key: same-request-key-1234"));
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&body).unwrap()["name"],
                "dev"
            );
            if pending {
                write!(
                    s,
                    "HTTP/1.1 202 Accepted\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
                )
                .unwrap();
            } else {
                send(s, json!({"name":"dev","state":"running"}));
            }
        }
        let (mut s, headers, _) = receive(&server);
        assert!(headers.starts_with("POST /v1/sandboxes/dev/stop "));
        let failed = json!({"error":"operation_failed"}).to_string();
        write!(
            s,
            "HTTP/1.1 409 Conflict\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            failed.len(),
            failed
        )
        .unwrap();
        drop(s);
        let (s, headers, body) = receive(&server);
        assert!(headers.starts_with("POST /oauth/revoke "));
        assert!(body.contains(&format!("token={}", "s".repeat(43))));
        send(s, json!({}));
    });
    let login = cli(temp.path())
        .args([
            "login",
            "--device",
            "--credential-store",
            "file",
            "--cloud-endpoint",
            &origin,
        ])
        .output()
        .unwrap();
    assert!(login.status.success());
    let path = temp.path().join("cloud/credentials.json");
    let mut saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    saved["expires_at"] = json!(0);
    std::fs::write(&path, saved.to_string()).unwrap();
    let who = cli(temp.path())
        .args(["whoami", "--json"])
        .output()
        .unwrap();
    assert!(who.status.success());
    assert!(String::from_utf8(who.stdout).unwrap().contains("Tester"));
    let limited = cli(temp.path()).args(["--cloud", "list"]).output().unwrap();
    assert!(!limited.status.success());
    assert!(String::from_utf8(limited.stderr)
        .unwrap()
        .contains("17 seconds"));
    for pending in [true, false] {
        let create = cli(temp.path())
            .args([
                "--cloud",
                "--idempotency-key",
                "same-request-key-1234",
                "create",
                "dev",
            ])
            .output()
            .unwrap();
        assert_eq!(create.status.success(), !pending);
        if pending {
            assert!(String::from_utf8(create.stderr)
                .unwrap()
                .contains("same-request-key-1234"));
        }
    }
    let failed = cli(temp.path())
        .args(["--cloud", "stop", "dev"])
        .output()
        .unwrap();
    assert!(!failed.status.success());
    let message = String::from_utf8(failed.stderr).unwrap();
    assert!(message.contains("choose a new key"));
    assert!(!message.contains("retry this lifecycle request with"));
    let logout = cli(temp.path()).arg("logout").output().unwrap();
    assert!(logout.status.success());
    assert!(!path.exists());
    assert_eq!(
        std::fs::read_to_string(temp.path().join("hosts.json")).unwrap(),
        "self-hosted sentinel"
    );
    h.join().unwrap();
}

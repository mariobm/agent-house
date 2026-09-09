//! Opt-in Linux networking lifecycle gate, using disposable guests and real netd.
#![cfg(target_os = "linux")]
use ahvm_engine::{
    Backend, BackendKind, KrucibleBackend, KrucibleConfig, NetworkConfig, SandboxSpec, Worker,
};
use std::path::PathBuf;
use std::time::{Duration, Instant};

struct Gate {
    backend: Option<KrucibleBackend>,
    dir: PathBuf,
}
impl Drop for Gate {
    fn drop(&mut self) {
        if let Some(be) = &self.backend {
            for id in ["a", "b", "copy"] {
                for name in ["vmm.log", "netd.log", "spec.json", "net.json"] {
                    let _ = std::fs::copy(
                        self.dir.join(id).join(name),
                        self.dir.join(format!("{id}-{name}")),
                    );
                }
                let _ = be.destroy(id);
            }
        }
    }
}
fn sh(s: &str) -> Vec<String> {
    vec!["/bin/sh".into(), "-ec".into(), s.into()]
}
fn exec(be: &KrucibleBackend, id: &str, command: &str) {
    let r = be.exec(id, &sh(command)).unwrap();
    assert_eq!(r.exit_code, 0, "{id}: {command}: {:?}", r.stderr);
}
fn network(be: &KrucibleBackend, id: &str) {
    exec(be, id, "curl -4 -fsS --connect-timeout 5 --max-time 15 https://example.com/ -o /tmp/network-check; test -s /tmp/network-check");
}
fn kill(w: &Worker) {
    assert_eq!(ahvm_engine::process_starttime(w.pid), w.starttime);
    assert!(std::process::Command::new("/bin/kill")
        .args(["-KILL", &w.pid.to_string()])
        .status()
        .unwrap()
        .success());
}
fn eventually(mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < end, "deadline expired");
        std::thread::sleep(Duration::from_millis(100));
    }
}
#[test]
fn isolated_gateways_recover_without_disturbing_peers() {
    if std::env::var("AHVM_KVM_NETWORK_TEST").as_deref() != Ok("1") {
        eprintln!(
            "SKIP kvm_network: set AHVM_KVM_NETWORK_TEST=1 with VMM, netd and Alpine guest image"
        );
        return;
    }
    let dir = PathBuf::from(std::env::var("AHVM_NETWORK_TEST_DIR").unwrap());
    assert!(!dir.exists(), "requires a fresh disposable directory");
    let mut cfg = KrucibleConfig::new(
        std::env::var("AHVM_VMM_BIN").unwrap().into(),
        std::env::var("AHVM_GUEST_IMAGE").unwrap().into(),
        dir.clone(),
        std::env::var("LD_LIBRARY_PATH").unwrap(),
    );
    cfg.network = Some(NetworkConfig {
        private_access: Default::default(),
        netd_bin: std::env::var("AHVM_NETD_BIN").unwrap().into(),
        resolver: std::env::var("AHVM_DNS_RESOLVER").unwrap().parse().unwrap(),
    });
    let mut gate = Gate {
        backend: Some(KrucibleBackend::open(cfg.clone()).unwrap()),
        dir,
    };
    let be = gate.backend.as_ref().unwrap();
    for id in ["a", "b"] {
        be.create(&SandboxSpec {
            name: id.into(),
            cpus: 1,
            memory_mb: 256,
            backend: BackendKind::Krucible,
            root_image: None,
            kernel_image: None,
            extra_env: Default::default(),
        })
        .unwrap();
        network(be, id);
    }
    let a = gate.dir.join("a/net-state.json");
    let b = gate.dir.join("b/net-state.json");
    let original_b = Worker::load(&b).unwrap();
    for _ in 0..3 {
        let old = Worker::load(&a).unwrap();
        kill(&old);
        network(be, "b");
        eventually(|| {
            Worker::load(&a).is_ok_and(|w| w.pid != old.pid && ahvm_engine::is_alive(w.pid))
        });
        network(be, "a");
        assert_eq!(Worker::load(&b).unwrap().pid, original_b.pid);
    }
    // Same private IP, different links: B's listener must not be reachable in A.
    let server_session = be.session_create(
        "b",
        &["python3".into(), "-u".into(), "-c".into(), "import socket
s=socket.socket(); s.bind(('0.0.0.0',18080)); s.listen(); print('ready')
while True:
 c,_=s.accept(); c.recv(4096); c.sendall(b'HTTP/1.1 200 OK\\r\\nContent-Length: 6\\r\\nConnection: close\\r\\n\\r\\nb-only'); c.close()".into()],
        false,
    )
    .unwrap();
    let output = be
        .session_read("b", &server_session, 0, Duration::from_millis(200))
        .unwrap();
    eprintln!("server session: {:?}", output);
    exec(be, "b", "curl --retry 4 --retry-connrefused --retry-delay 1 -fsS http://100.64.0.2:18080/ -o /tmp/local-check");
    exec(
        be,
        "a",
        "! curl --connect-timeout 1 --max-time 2 -fsS http://100.64.0.2:18080/",
    );
    // Positive host control: the denied destination really has a listening service.
    let route = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    route.connect("1.1.1.1:53").unwrap();
    let host = route.local_addr().unwrap().ip();
    let listener = std::net::TcpListener::bind((host, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let _control = std::net::TcpStream::connect(address).unwrap();
    listener.accept().unwrap();
    listener.set_nonblocking(true).unwrap();
    exec(
        be,
        "a",
        &format!("! curl --connect-timeout 1 --max-time 2 -fsS http://{address}/"),
    );
    assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    for target in ["127.0.0.1", "169.254.169.254", "10.0.0.1", "100.64.0.3"] {
        exec(
            be,
            "a",
            &format!("! curl --connect-timeout 1 --max-time 2 -fsS http://{target}:18080/"),
        );
    }
    let snapshot = be.snapshot("a", "net-copy").unwrap();

    be.stop("a").unwrap();
    assert!(!a.exists());
    be.start("a").unwrap();
    network(be, "a");
    let vm = Worker::load(gate.dir.join("a/state.json")).unwrap();
    kill(&vm);
    eventually(|| be.status("a").unwrap().state == ahvm_engine::State::Failed);
    be.start("a").unwrap();
    network(be, "a");
    network(be, "b");
    let before = Worker::load(&b).unwrap();
    drop(gate.backend.take());
    gate.backend = Some(KrucibleBackend::open(cfg).unwrap());
    let be = gate.backend.as_ref().unwrap();
    assert_eq!(Worker::load(&b).unwrap().pid, before.pid);
    network(be, "a");
    network(be, "b");
    // Keep the live-guest budget at two throughout restore-as-new.
    be.destroy("b").unwrap();
    be.restore(&snapshot, "copy").unwrap();
    network(be, "copy");
    let dead = Worker::load(&a).unwrap();
    kill(&dead);
    eventually(|| Worker::load(&a).is_ok_and(|w| w.pid != dead.pid));
    network(be, "a");
    let net_pids: Vec<_> = ["a", "copy"]
        .iter()
        .map(|id| {
            Worker::load(gate.dir.join(id).join("net-state.json"))
                .unwrap()
                .pid
        })
        .collect();
    for id in ["a", "copy"] {
        be.destroy(id).unwrap();
    }
    assert!(be.list().unwrap().is_empty());
    for pid in net_pids {
        assert!(!ahvm_engine::is_alive(pid));
    }
    eprintln!("PASS: TCP/DNS, 3 isolated restarts, host/peer isolation, snapshot restore, stop/start, VM crash, adoption, restart after adoption, cleanup");
}

#[test]
fn dns_rejects_wrong_replies_and_retries_truncation_over_tcp() {
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, TcpListener, UdpSocket};
    if std::env::var("AHVM_KVM_NETWORK_TEST").as_deref() != Ok("1") {
        eprintln!("SKIP DNS gate: requires AHVM_KVM_NETWORK_TEST=1");
        return;
    }
    // A disposable resolver fixture on Linux's loopback /8. Port 53 is required
    // by the product contract; never replace or reconfigure the host resolver.
    let pid = std::process::id();
    let resolver = Ipv4Addr::new(127, 77, (pid >> 8) as u8, pid as u8);
    let udp = UdpSocket::bind((resolver, 53)).unwrap();
    let tcp = TcpListener::bind((resolver, 53)).unwrap();
    let forbidden = TcpListener::bind((resolver, 54)).unwrap();
    let control = std::net::TcpStream::connect((resolver, 54)).unwrap();
    forbidden.accept().unwrap();
    drop(control);
    forbidden.set_nonblocking(true).unwrap();
    udp.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    tcp.set_nonblocking(true).unwrap();
    let fixture = std::thread::spawn(move || {
        let mut buf = [0; 512];
        let (n, peer) = udp.recv_from(&mut buf).unwrap();
        let query = &buf[..n];
        let mut reply = query.to_vec();
        reply[2] |= 0x82; // QR + TC: force the client onto TCP.
        let mut wrong_id = reply.clone();
        wrong_id[0] ^= 1;
        udp.send_to(&wrong_id, peer).unwrap();
        let mut wrong_question = reply.clone();
        wrong_question[13] ^= 1;
        udp.send_to(&wrong_question, peer).unwrap();
        // A valid oversized UDP reply without TC must be reduced by netd to a
        // small TC response. Otherwise the Ethernet MTU silently drops it.
        let mut oversized = reply.clone();
        oversized[2] &= !2;
        oversized[11] = 1;
        // EDNS OPT with a 1600-byte padding option.
        oversized.extend_from_slice(&[0, 0, 41, 16, 0, 0, 0, 0, 0, 6, 68, 0, 12, 6, 64]);
        oversized.resize(oversized.len() + 1600, 0);
        udp.send_to(&oversized, peer).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            match tcp.accept() {
                Ok((s, _)) => break s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "no TCP retry after truncated DNS"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("DNS TCP fixture: {e}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut length = [0; 2];
        stream.read_exact(&mut length).unwrap();
        let mut request = vec![0; u16::from_be_bytes(length) as usize];
        stream.read_exact(&mut request).unwrap();
        assert_eq!(request, query);
        reply[2] &= !2;
        reply[7] = 1; // One A answer, compressed owner name.
        reply
            .extend_from_slice(b"\xc0\x0c\x00\x01\x00\x01\x00\x00\x00\x01\x00\x04\xc0\x00\x02\x7b");
        stream
            .write_all(&(reply.len() as u16).to_be_bytes())
            .unwrap();
        stream.write_all(&reply[..7]).unwrap();
        stream.write_all(&reply[7..]).unwrap();
    });
    let dir = PathBuf::from(format!(
        "{}-dns",
        std::env::var("AHVM_NETWORK_TEST_DIR").unwrap()
    ));
    assert!(!dir.exists(), "requires a fresh disposable directory");
    let mut cfg = KrucibleConfig::new(
        std::env::var("AHVM_VMM_BIN").unwrap().into(),
        std::env::var("AHVM_GUEST_IMAGE").unwrap().into(),
        dir.clone(),
        std::env::var("LD_LIBRARY_PATH").unwrap(),
    );
    cfg.network = Some(NetworkConfig {
        private_access: Default::default(),
        netd_bin: std::env::var("AHVM_NETD_BIN").unwrap().into(),
        resolver,
    });
    let gate = Gate {
        backend: Some(KrucibleBackend::open(cfg).unwrap()),
        dir,
    };
    let be = gate.backend.as_ref().unwrap();
    be.create(&SandboxSpec {
        name: "a".into(),
        cpus: 1,
        memory_mb: 256,
        backend: BackendKind::Krucible,
        root_image: None,
        kernel_image: None,
        extra_env: Default::default(),
    })
    .unwrap();
    exec(
        be,
        "a",
        r#"python3 - <<'PY'
import socket, struct
query = b'\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x07example\x04test\x00\x00\x01\x00\x01'
with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
    s.settimeout(5)
    s.sendto(query, ('100.64.0.1', 53))
    reply = s.recv(512)
    assert reply[:2] == query[:2] and reply[12:] == query[12:], reply
    assert reply[2] & 2, reply
with socket.create_connection(('100.64.0.1', 53), timeout=5) as s:
    def exact(n):
        data = b''
        while len(data) < n:
            chunk = s.recv(n - len(data))
            assert chunk, 'unexpected DNS EOF'
            data += chunk
        return data
    wire = struct.pack('!H', len(query)) + query
    s.sendall(wire[:1])
    s.sendall(wire[1:])
    reply = exact(struct.unpack('!H', exact(2))[0])
    assert reply[:2] == query[:2] and not reply[2] & 2, reply
    assert reply[-4:] == bytes([192, 0, 2, 123]), reply
# The DNS exception must not permit arbitrary gateway ports.
try:
    s = socket.create_connection(('100.64.0.1', 54), timeout=0.5)
except OSError:
    pass
else:
    s.close()
    raise AssertionError('unexpected gateway access')
print('DNS-FALLBACK-OK')
PY"#,
    );
    fixture.join().unwrap();
    assert!(matches!(forbidden.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    be.destroy("a").unwrap();
}

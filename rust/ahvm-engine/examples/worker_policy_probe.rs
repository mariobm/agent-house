//! Opt-in Linux probe for the exact policy emitted by a qualification VM.
#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use ahvm_engine::WorkerSandbox;
    use std::{fs, io, net::TcpStream, path::PathBuf, time::Duration};

    if std::env::var("AHVM_WORKER_REPLICATED_TEST").as_deref() != Ok("1") {
        return Err("set AHVM_WORKER_REPLICATED_TEST=1 for the isolated fixture".into());
    }
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 4 {
        return Err(
            "usage: worker_policy_probe SPEC_JSON OWN_DEVICE PEER_FILE HOST_TCP_ADDRESS".into(),
        );
    }
    let spec: serde_json::Value = serde_json::from_slice(&fs::read(&args[0])?)?;
    let policy: WorkerSandbox = serde_json::from_value(spec["worker_sandbox"].clone())?;
    if spec["trusted_host_socket_access"] != false || spec["root_disk"] != args[1] {
        return Err("expected ordinary worker and its exact raw device".into());
    }
    let runtime = PathBuf::from(&args[0])
        .parent()
        .ok_or("missing VM directory")?
        .join("runtime/policy-proof");
    let address = args[3].parse()?;
    let denied = |error: io::Error| {
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
    };
    // Check the same UID's DAC and a live host listener before restricting.
    drop(
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&args[2])?,
    );
    drop(TcpStream::connect_timeout(
        &address,
        Duration::from_secs(1),
    )?);
    drop(std::net::TcpListener::bind("127.0.0.1:0")?);
    let pathname = policy.restrict(false)?;
    let _disk = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&args[1])?;
    fs::write(&runtime, b"own-runtime")?;
    fs::remove_file(runtime)?;
    denied(fs::read(&args[2]).unwrap_err());
    denied(fs::write(&args[2], b"forbidden").unwrap_err());
    denied(TcpStream::connect_timeout(&address, Duration::from_secs(1)).unwrap_err());
    denied(std::net::TcpListener::bind("127.0.0.1:0").unwrap_err());
    println!("PASS exact worker policy: own NBD/runtime allowed; peer contents and TCP bind/connect denied; pathname_uds_restricted={pathname}");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("worker_policy_probe requires Linux Landlock ABI 6+");
    std::process::exit(2);
}

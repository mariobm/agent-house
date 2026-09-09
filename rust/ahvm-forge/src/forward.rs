//! Bounded loopback-only guest tunnels. After one framed response, bytes are raw.
use crate::agent::Conn;
use ahvm_proto::{write_frame, Frame, FrameType};
use std::io::{BufReader, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
struct Permit;
impl Drop for Permit {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

pub fn serve(mut reader: BufReader<Conn>, mut writer: Conn, payload: &[u8]) {
    let run = || -> std::io::Result<()> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Request {
            port: u16,
        }
        let request: Request = serde_json::from_slice(payload)?;
        if request.port == 0
            || ACTIVE
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                    (n < 64).then_some(n + 1)
                })
                .is_err()
        {
            return Err(std::io::Error::other(
                "invalid port or preview capacity exhausted",
            ));
        }
        let _permit = Permit;
        let tcp = TcpStream::connect_timeout(
            &([127, 0, 0, 1], request.port).into(),
            Duration::from_secs(5),
        )?;
        let mut upstream = Conn::Tcp(tcp);
        upstream.timeout(Duration::from_secs(1))?;
        writer.timeout(Duration::from_secs(1))?;
        let mut downstream = writer.try_clone()?;
        let mut upstream_read = upstream.try_clone()?;
        write_frame(
            &mut writer,
            &Frame {
                msg_type: FrameType::ForwardResp,
                payload: vec![],
            },
        )
        .map_err(std::io::Error::other)?;
        let last = AtomicU64::new(0);
        let cancelled = AtomicBool::new(false);
        let start = Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(|| relay(&mut reader, &mut upstream, start, &last, &cancelled));
            relay(
                &mut upstream_read,
                &mut downstream,
                start,
                &last,
                &cancelled,
            );
        });
        Ok(())
    };
    // On handshake errors the caller receives a frame, never raw error text.
    let mut run = run;
    if let Err(e) = run() {
        let _ = write_frame(
            &mut writer,
            &Frame {
                msg_type: FrameType::Error,
                payload: serde_json::to_vec(&serde_json::json!({"message":e.to_string()})).unwrap(),
            },
        );
    }
}
fn relay(
    reader: &mut impl Read,
    writer: &mut Conn,
    start: Instant,
    last: &AtomicU64,
    cancelled: &AtomicBool,
) {
    let mut buf = [0; 16384];
    while !cancelled.load(Ordering::Relaxed) {
        let elapsed = start.elapsed().as_secs();
        if elapsed >= 300 || elapsed.saturating_sub(last.load(Ordering::Relaxed)) >= 30 {
            break;
        }
        match reader.read(&mut buf) {
            Ok(0) => {
                writer.shutdown(Shutdown::Write);
                return;
            }
            Ok(n) => {
                if writer.write_all(&buf[..n]).is_err() {
                    break;
                }
                last.store(start.elapsed().as_secs(), Ordering::Relaxed);
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(_) => break,
        }
    }
    cancelled.store(true, Ordering::Relaxed);
    writer.shutdown(Shutdown::Both);
}

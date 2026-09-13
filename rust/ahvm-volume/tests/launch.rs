#![cfg(target_os = "linux")]
use std::{
    io::Write,
    process::{Command, Stdio},
};
#[test]
fn child_does_not_execute_until_parent_commits_launch() {
    // Exit status 42 proves whether exec happened, without touching any device.
    let mut cancelled = Command::new(env!("CARGO_BIN_EXE_ahvm-volumed"))
        .args(["_attach", "/bin/sh", "-c", "exit 42"])
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    drop(cancelled.stdin.take());
    assert_ne!(cancelled.wait().unwrap().code(), Some(42));
    let mut allowed = Command::new(env!("CARGO_BIN_EXE_ahvm-volumed"))
        .args(["_attach", "/bin/sh", "-c", "exit 42"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    allowed.stdin.take().unwrap().write_all(b"G").unwrap();
    assert_eq!(allowed.wait().unwrap().code(), Some(42));
}

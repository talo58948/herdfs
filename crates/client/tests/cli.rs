//! How `herdfs mount` starts, which the lab relies on. No contract line: these check the
//! flags in docs/architecture.md.

#![allow(clippy::unwrap_used)]

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::process::{Command, Output};
use std::{fs, os::unix::fs::symlink, path::Path, thread};

fn control_server(
    listener: UnixListener,
    answer: serde_json::Value,
) -> thread::JoinHandle<serde_json::Value> {
    thread::spawn(move || {
        loop {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            if request.is_empty() {
                continue;
            }
            writeln!(stream, "{answer}").unwrap();
            return serde_json::from_str(&request).unwrap();
        }
    })
}

fn register(dir: &Path, mountpoint: &Path, socket: &Path) {
    let config = dir.join("config/herdfs/mounts");
    fs::create_dir_all(&config).unwrap();
    let registration = serde_json::json!({
        "id":"test", "space":"team", "authority":"http://authority",
        "mountpoint":mountpoint, "socket":socket, "pid":std::process::id()
    });
    fs::write(
        config.join("test.json"),
        serde_json::to_vec(&registration).unwrap(),
    )
    .unwrap();
}

fn mount(dir: &Path, strategy: &str) -> io::Result<Output> {
    let token = dir.join("token");
    std::fs::write(&token, "t")?;
    Command::new(env!("CARGO_BIN_EXE_herdfs"))
        .args([
            "mount",
            "--authority",
            "http://127.0.0.1:1",
            "--space",
            "team",
            "--token-file",
        ])
        .arg(token)
        .arg("--cache")
        .arg(dir.join("cache"))
        .args(["--cache-bytes", "1000000", "--journal"])
        .arg(dir.join("journal"))
        .args(["--strategy", strategy, "--resolve", "s3.lab=127.0.0.2"])
        .arg(dir.join("mnt"))
        .output()
}

#[test]
fn mount_fails_clearly_when_the_authority_is_unreachable() {
    let dir = tempfile::tempdir().unwrap();
    let output = mount(dir.path(), "chaos:3").unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("cannot reach space \"team\" at the authority http://127.0.0.1:1"),
        "{stderr}"
    );
}

#[test]
fn mount_rejects_an_unknown_strategy_and_jev_without_its_key() {
    let dir = tempfile::tempdir().unwrap();
    for (strategy, error) in [("nope", "unknown strategy"), ("jev", "--jev-key-file")] {
        let output = mount(dir.path(), strategy).unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{stderr}");
        assert!(stderr.contains(error), "{stderr}");
    }
}

// contract: P3, U1
#[test]
fn explain_uses_the_registered_mount_control_socket() {
    let dir = tempfile::tempdir().unwrap();
    let mountpoint = dir.path().join("mount");
    fs::create_dir(&mountpoint).unwrap();
    let path = mountpoint.join("file");
    fs::write(&path, b"bytes").unwrap();
    let plan = br#"{"plan":{"prefetch":"index","read_ahead":"large","keep":"no"},"source":"jev","bytes_fetched":42}"#;
    let socket = dir.path().join("control.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let expected: serde_json::Value = serde_json::from_slice(plan).unwrap();
    let answer = serde_json::json!({"ok":true,"plan":expected["plan"],"source":"jev","bytes_fetched":42});
    let server = control_server(listener, answer);
    register(dir.path(), &mountpoint, &socket);
    let output = Command::new(env!("CARGO_BIN_EXE_herdfs"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .arg("--json")
        .arg("explain")
        .arg(&path)
        .output()
        .unwrap();
    let request = server.join().unwrap();
    assert_eq!(request["op"], "explain");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let explained: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(explained["plan"], expected["plan"]);
    assert_eq!(explained["ok"], true);
}

// contract: U2
#[test]
fn deleted_relative_path_maps_through_dotdot_and_a_symlinked_mountpoint() {
    let dir = tempfile::tempdir().unwrap();
    let mountpoint = dir.path().join("mount");
    fs::create_dir_all(mountpoint.join("child")).unwrap();
    symlink(&mountpoint, dir.path().join("alias")).unwrap();
    let socket = dir.path().join("control.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = control_server(listener, serde_json::json!({"ok":true}));
    register(dir.path(), &mountpoint, &socket);

    let output = Command::new(env!("CARGO_BIN_EXE_herdfs"))
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .args(["--json", "unpin", "alias/child/../deleted"])
        .output()
        .unwrap();
    let request = server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(request["op"], "unpin");
    assert_eq!(
        request["path"],
        mountpoint.join("deleted").to_string_lossy().as_ref()
    );
}

// contract: U1
#[test]
fn json_control_error_is_printed_once_and_fails() {
    let dir = tempfile::tempdir().unwrap();
    let mountpoint = dir.path().join("mount");
    fs::create_dir(&mountpoint).unwrap();
    let socket = dir.path().join("control.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = control_server(listener, serde_json::json!({"ok":false,"error":"failed once"}));
    register(dir.path(), &mountpoint, &socket);

    let output = Command::new(env!("CARGO_BIN_EXE_herdfs"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .args(["--json", "status"])
        .arg(&mountpoint)
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(!output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["error"], "failed once");
}

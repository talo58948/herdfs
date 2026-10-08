//! How `herdfs mount` starts, which the lab relies on. No contract line: these check the
//! flags in docs/architecture.md.

use std::process::{Command, Output};
use std::{io, path::Path};

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

// contract: P3
#[test]
fn explain_prints_the_plan_xattr_as_json() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file");
    std::fs::write(&path, b"bytes").unwrap();
    let plan = br#"{"plan":{"prefetch":"index","read_ahead":"large","keep":"no"},"source":"jev","bytes_fetched":42}"#;
    xattr::set(&path, "user.herdfs.plan", plan).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_herdfs"))
        .arg("explain")
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let explained: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let expected: serde_json::Value = serde_json::from_slice(plan).unwrap();
    assert_eq!(explained, expected);
}

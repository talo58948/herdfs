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
fn mount_rejects_an_unknown_strategy() {
    let dir = tempfile::tempdir().unwrap();
    let output = mount(dir.path(), "jev").unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("unknown strategy"), "{stderr}");
}

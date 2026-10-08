//! Durable keep-downloaded data and the desktop control channel through the mounted filesystem.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use herdfs_lab::{Lab, LabConfig, Links, Profile, Strategy, herdfs_command};
use serde_json::Value;

const WAIT: Duration = Duration::from_secs(45);
const CACHE_BYTES: u64 = 128 << 10;

fn start() -> Result<Option<Lab>> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(None);
    }
    let slow_store = Profile::Custom {
        round_trip_ms: 5,
        kilobytes_per_sec: 512,
        store_first_byte_ms: 10,
    };
    let config = LabConfig::default()
        .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
        .client("b", "bob", &[], slow_store, Strategy::Stream)
        .cache_bytes("b", CACHE_BYTES);
    Lab::start(config).map(Some)
}

fn command(lab: &Lab, op: &str, path: Option<&Path>) -> Result<Value> {
    let mut command = herdfs_command()?;
    command.args(["--json", op]);
    command.arg(path.unwrap_or_else(|| lab.mount_path("b")));
    let output = command.output()?;
    ensure!(
        output.status.success(),
        "{op} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn status(lab: &Lab) -> Result<Value> {
    command(lab, "status", None)
}

fn wait_ready(lab: &Lab, ready: bool) -> Result<Value> {
    let deadline = Instant::now() + WAIT;
    loop {
        let status = status(lab)?;
        let is_ready = status["kept"]["pins"]
            .as_array()
            .is_some_and(|pins| pins.iter().all(|pin| pin["ready"] == ready));
        if is_ready
            && status["kept"]["pins"]
                .as_array()
                .is_some_and(|pins| !pins.is_empty())
        {
            return Ok(status);
        }
        ensure!(
            Instant::now() < deadline,
            "pin did not become ready={ready}: {status}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_pinned_over_budget(lab: &Lab, path: &Path) -> Result<Value> {
    let deadline = Instant::now() + WAIT;
    loop {
        assert_eq!(command(lab, "pin", Some(path))?["ok"], true);
        let status = wait_ready(lab, true)?;
        if status["cache"]["used"]
            .as_u64()
            .is_some_and(|used| used > CACHE_BYTES)
        {
            return Ok(status);
        }
        ensure!(
            Instant::now() < deadline,
            "kept root did not fill beyond its cache budget: {status}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn save(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn socket_request(lab: &Lab, request: &Value) -> Result<Value> {
    let mut stream = UnixStream::connect(lab.client_dir("b").join("journal.state/control.sock"))?;
    writeln!(stream, "{}", serde_json::to_string(&request)?)?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

// contract: P3, P4, C10, C12, U1, U2
#[test]
fn kept_folder_survives_restart_follows_heads_and_becomes_evictable() -> Result<()> {
    let Some(mut lab) = start()? else { return Ok(()) };
    let folder = lab.mount_path("a").join("kept");
    fs::create_dir(&folder)?;
    let files = [("one", vec![1; 96 << 10]), ("two", vec![2; 96 << 10])];
    for (name, bytes) in &files {
        save(&folder.join(name), bytes)?;
    }
    lab.wait_committed("a")?;

    let pinned = lab.mount_path("b").join("kept");
    let mode = fs::metadata(lab.client_dir("b").join("journal.state/control.sock"))?
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(
        socket_request(&lab, &serde_json::json!({"op":"stats"}))?["ok"],
        true
    );
    let ready = wait_pinned_over_budget(&lab, &pinned)?;
    assert!(ready["kept"]["bytes"].as_u64().is_some_and(|bytes| bytes > 0));
    assert_eq!(command(&lab, "file", Some(&pinned.join("one")))?["kept"], true);

    lab.restart_client("b")?;
    wait_ready(&lab, true)?;
    lab.cut("b", Links::Both)?;
    for (name, bytes) in &files {
        assert_eq!(fs::read(pinned.join(name))?, *bytes);
    }
    assert_eq!(wait_ready(&lab, true)?["kept"]["pins"][0]["ready"], true);
    assert_eq!(command(&lab, "explain", Some(&pinned.join("one")))?["ok"], true);

    let replacement = vec![3; 2 << 20];
    save(&folder.join("one"), &replacement)?;
    lab.wait_committed("a")?;
    command(&lab, "pin", Some(&pinned))?;
    thread::sleep(Duration::from_millis(100));
    assert_eq!(wait_ready(&lab, true)?["kept"]["pins"][0]["ready"], true);
    lab.restore("b", Links::Both)?;
    command(&lab, "pin", Some(&pinned))?;
    wait_ready(&lab, false).context("new remote head never entered downloading state")?;
    assert_eq!(
        command(&lab, "file", Some(&pinned.join("one")))?["downloading"],
        true
    );
    wait_ready(&lab, true)?;
    assert_eq!(fs::read(pinned.join("one"))?, replacement);

    assert_eq!(command(&lab, "unpin", Some(&pinned))?["ok"], true);
    let status = status(&lab)?;
    assert_eq!(status["kept"]["pins"], serde_json::json!([]));
    assert!(
        status["cache"]["used"]
            .as_u64()
            .is_some_and(|used| used <= CACHE_BYTES)
    );
    Ok(())
}

// contract: U1, U2, C4a
#[test]
fn pause_holds_sealed_writes_and_resume_commits_them() -> Result<()> {
    let Some(lab) = start()? else { return Ok(()) };
    let path = lab.mount_path("b").join("paused");
    assert_eq!(command(&lab, "pause", None)?["ok"], true);
    fs::write(&path, b"waiting")?;
    let deadline = Instant::now() + WAIT;
    loop {
        let status = status(&lab)?;
        if status["paused"] == true
            && status["pending"]
                .as_array()
                .is_some_and(|items| !items.is_empty())
        {
            assert_eq!(status["pending"][0]["path"], "paused");
            assert_eq!(status["pending"][0]["bytes"], 7);
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "write did not remain pending: {status}"
        );
        thread::sleep(Duration::from_millis(20));
    }
    thread::sleep(Duration::from_secs(2));
    ensure!(
        !status(&lab)?["pending"].as_array().unwrap().is_empty(),
        "paused write committed"
    );
    assert_eq!(command(&lab, "file", Some(&path))?["pending"], true);
    assert_eq!(command(&lab, "resume", None)?["ok"], true);
    lab.wait_committed("b")?;
    assert!(status(&lab)?["pending"].as_array().unwrap().is_empty());
    Ok(())
}

use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Perm, Request};
use herdfs_lab::{ADMIN, Lab, LabConfig, Profile, SPACE, Strategy, mounted, herdfs_command};

use super::support;

#[test]
fn browser_login_refreshes_for_a_mount_and_logout_removes_the_token() -> Result<()> {
    // contract: U1, S3
    let lab = Lab::start(LabConfig::default().client("a", "alice", &[], Profile::Lan, Strategy::Stream))?;
    let dir = tempfile::tempdir()?;
    let token = dir.path().join("token");
    let mut login = herdfs_command()?;
    login
        .args(["login", "--issuer", lab.issuer_url(), "--client-id", "lab-client"])
        .arg("--token-file")
        .arg(&token)
        .env("PATH", "")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut login = login.spawn()?;
    let stdout = login.stdout.take().context("login stdout")?;
    let mut lines = BufReader::new(stdout).lines();
    let authorize = lines.next().context("login did not print its URL")??;
    let authorize_url = reqwest::Url::parse(&authorize)?;
    let redirect = authorize_url
        .query_pairs()
        .find_map(|(key, value)| (key == "redirect_uri").then(|| value.into_owned()))
        .context("authorization URL has no redirect_uri")?;
    assert_eq!(redirect, "http://127.0.0.1:53682/callback");
    let response = reqwest::blocking::get(authorize)?;
    ensure!(response.status().is_success(), "loopback callback failed");
    ensure!(login.wait()?.success(), "login command failed");
    assert_eq!(fs::metadata(&token)?.permissions().mode() & 0o777, 0o600);

    lab.rpc(
        ADMIN,
        &Request::SetAcl {
            space: SPACE.into(),
            path: SpacePath::root(),
            principal: "user:login".into(),
            perm: Some(Perm::Read),
        },
    )?;
    let mountpoint = dir.path().join("mount");
    fs::create_dir(&mountpoint)?;
    let authority = lab.authority_url("a");
    let host = reqwest::Url::parse(&authority)?
        .host_str()
        .context("authority host")?
        .to_owned();
    let refresh_count = lab.issuer_refresh_count();
    let mut mount = herdfs_command()?;
    mount
        .args([
            "mount",
            "--authority",
            &authority,
            "--space",
            SPACE,
            "--token-file",
        ])
        .arg(&token)
        .arg("--cache")
        .arg(dir.path().join("cache"))
        .args(["--cache-bytes", "1048576", "--journal"])
        .arg(dir.path().join("journal"))
        .args(["--resolve", &format!("s3.lab={host}")])
        .arg(&mountpoint)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut mount = mount.spawn()?;
    let mounted_result = (|| {
        wait_mounted(&mut mount, &mountpoint)?;
        assert_eq!(lab.issuer_refresh_count(), refresh_count);
        thread::sleep(Duration::from_secs(36));
        let mut commands = Vec::new();
        for _ in 0..4 {
            commands.push(support::command(&lab, "a", &token, &["whoami"])?.spawn()?);
        }
        fs::read_dir(&mountpoint)?.collect::<std::io::Result<Vec<_>>>()?;
        for command in &mut commands {
            ensure!(command.wait()?.success(), "concurrent command failed");
        }
        assert_eq!(
            lab.issuer_refresh_count(),
            refresh_count + 1,
            "mount and commands did not share one rotating refresh token"
        );
        Ok(())
    })();
    let _ = mount.kill();
    let _ = mount.wait();
    let detached = std::process::Command::new("fusermount3")
        .args(["-u", "-z"])
        .arg(&mountpoint)
        .status()?;
    ensure!(detached.success(), "could not detach the test mount");
    mounted_result?;
    let me = support::json(&mut support::command(&lab, "a", &token, &["whoami"])?)?;
    assert_eq!(me["principal"], "user:login");
    let status = herdfs_command()?
        .args(["logout", "--token-file"])
        .arg(&token)
        .status()?;
    ensure!(status.success(), "logout failed");
    assert!(!token.exists());
    Ok(())
}

fn wait_mounted(child: &mut std::process::Child, mountpoint: &std::path::Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !mounted(mountpoint) {
        ensure!(child.try_wait()?.is_none(), "mount exited before mounting");
        ensure!(Instant::now() < deadline, "mount did not become ready");
        thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

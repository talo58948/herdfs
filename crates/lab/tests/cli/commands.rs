use std::fs;

use anyhow::{Result, bail};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Request, Response};
use herdfs_lab::{ADMIN, Lab, LabConfig, Links, Profile, SPACE, Strategy};

use super::support;

#[test]
fn every_authority_command_uses_the_real_protocol_and_enforcement() -> Result<()> {
    // contract: U3, S3, S4, S8, C13, C14
    let lab = Lab::start(LabConfig::default().client("a", "alice", &[], Profile::Lan, Strategy::Stream))?;
    let admin_token = lab.client_dir("a").join("admin-token");
    let late_token = lab.client_dir("a").join("late-token");
    fs::write(&admin_token, lab.token(ADMIN, &[]))?;
    fs::write(&late_token, lab.token("late", &[]))?;
    let file = lab.mount_path("a").join("notes.txt");
    save(&lab, &file, b"one")?;
    save(&lab, &file, b"two")?;
    let before = history(&lab, ADMIN)?;

    let me = call(&lab, &admin_token, &["whoami"])?;
    assert_eq!(me["principal"], "user:admin");
    call(
        &lab,
        &admin_token,
        &["share", SPACE, "notes.txt", "user:late", "read"],
    )?;
    let grants = call(&lab, &admin_token, &["access", SPACE, "notes.txt"])?;
    assert!(grants.as_array().is_some_and(|rows| {
        rows.iter()
            .any(|row| row["principal"] == "user:late" && row["perm"] == "read")
    }));
    let denied =
        support::command(&lab, "a", &late_token, &["access", SPACE, "notes.txt", "--json"])?.output()?;
    assert!(!denied.status.success());
    assert!(!String::from_utf8_lossy(&denied.stderr).contains("user:admin"));

    lab.rpc(
        ADMIN,
        &Request::CreateSpace {
            space: "secret".into(),
        },
    )?;
    let spaces = call(&lab, &late_token, &["spaces"])?;
    assert_eq!(spaces.as_array().map(Vec::len), Some(1));
    assert_eq!(spaces[0]["space"], SPACE);

    save(&lab, &file, b"three")?;
    let expected = history(&lab, "late")?;
    let visible = call(&lab, &late_token, &["history", SPACE, "notes.txt"])?;
    assert_eq!(visible, serde_json::to_value(&expected)?);
    assert_eq!(expected.len(), 1, "a late grantee sees only the later commit");
    let shown = lab.client_dir("a").join("shown");
    call(
        &lab,
        &late_token,
        &[
            "show",
            SPACE,
            "notes.txt",
            "--version",
            &expected[0].id.0.to_string(),
            "--output",
            shown.to_str().unwrap_or_default(),
        ],
    )?;
    assert_eq!(fs::read(&shown)?, b"three");
    fs::write(&shown, b"keep")?;
    lab.cut("a", Links::Store)?;
    let failed = support::command(
        &lab,
        "a",
        &late_token,
        &[
            "show",
            SPACE,
            "notes.txt",
            "--version",
            &expected[0].id.0.to_string(),
            "--output",
            shown.to_str().unwrap_or_default(),
        ],
    )?
    .output()?;
    assert!(!failed.status.success());
    assert_eq!(
        fs::read(&shown)?,
        b"keep",
        "a failed show preserves its destination"
    );
    lab.restore("a", Links::Store)?;

    call(&lab, &admin_token, &["revoke", SPACE, "notes.txt", "user:late"])?;
    let revoked = support::command(
        &lab,
        "a",
        &late_token,
        &[
            "show",
            SPACE,
            "notes.txt",
            "--version",
            &expected[0].id.0.to_string(),
            "--output",
            shown.to_str().unwrap_or_default(),
        ],
    )?
    .output()?;
    assert!(!revoked.status.success(), "revocation stopped a new show grant");

    call(
        &lab,
        &admin_token,
        &[
            "restore",
            SPACE,
            "notes.txt",
            "--version",
            &before[0].id.0.to_string(),
        ],
    )?;
    std::thread::sleep(std::time::Duration::from_millis(1_100));
    assert_eq!(fs::read(&file)?, b"one");
    call(&lab, &admin_token, &["fork", SPACE, "copy"])?;
    call(&lab, &admin_token, &["create-space", "temporary"])?;
    call(&lab, &admin_token, &["delete-space", "temporary"])?;
    call(&lab, &admin_token, &["purge", SPACE, "notes.txt"])?;
    assert_eq!(history(&lab, ADMIN)?.len(), 1);
    Ok(())
}

fn call(lab: &Lab, token: &std::path::Path, arguments: &[&str]) -> Result<serde_json::Value> {
    support::json(&mut support::command(lab, "a", token, arguments)?)
}

fn save(lab: &Lab, path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    fs::write(path, bytes)?;
    lab.wait_committed("a")
}

fn history(lab: &Lab, user: &str) -> Result<Vec<herdfs_core::proto::Version>> {
    let request = Request::History {
        space: SPACE.into(),
        path: SpacePath::try_from("notes.txt")?,
    };
    let Response::History { versions } = lab.rpc(user, &request)? else {
        bail!("history returned another response")
    };
    Ok(versions)
}

//! S8 and S4: a late grant shows the head but no earlier history, and a grant URL dies with its
//! TTL.

use std::fs;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use herdfs_core::SpacePath;
use herdfs_core::proto::{ErrorCode, Perm, Request, Response, Version, VersionId};
use herdfs_lab::{ADMIN, Lab, SPACE};
use reqwest::StatusCode;

use crate::{ALICE, BOB, QUICK, history, random_bytes, start, write};

fn set_acl(lab: &Lab, path: &str, principal: &str, perm: Option<Perm>) -> Result<()> {
    let request = Request::SetAcl {
        space: SPACE.into(),
        path: SpacePath::try_from(path)?,
        principal: principal.into(),
        perm,
    };
    lab.rpc(ADMIN, &request)?;
    Ok(())
}

fn grant(version: &Version) -> Request {
    Request::Grant {
        space: SPACE.into(),
        entry: version.entry,
        version: version.id,
        chunks: vec![0],
    }
}

fn restore(path: &str, version: VersionId) -> Result<Request> {
    Ok(Request::Restore {
        space: SPACE.into(),
        path: SpacePath::try_from(path)?,
        version,
    })
}

// contract: S8, C14
#[test]
fn a_late_grant_shows_the_head_but_not_earlier_history() -> Result<()> {
    let Some(lab) = start()? else {
        return Ok(());
    };
    set_acl(&lab, "", "user:bob", None)?;
    fs::create_dir(lab.mount_path(ALICE).join("plans"))?;
    let path = "plans/q4.txt";
    let first = write(&lab, path, b"first plan")?;
    let second = write(&lab, path, b"second plan")?;
    set_acl(&lab, "plans", "user:bob", Some(Perm::Write))?;
    assert_eq!(
        history(&lab, BOB, path)?,
        std::slice::from_ref(&second),
        "bob sees the head only"
    );
    for request in [grant(&first), restore(path, first.id)?] {
        let refusal = lab
            .rpc(BOB, &request)
            .expect_err("bob may not reach history before his grant");
        assert_eq!(refusal.code, ErrorCode::Denied, "{request:?}");
        let named = refusal.message.contains("plans") || refusal.message.contains("q4");
        assert!(!named, "the refusal names the path: {refusal}");
    }
    lab.rpc(BOB, &grant(&second))?;

    let third = write(&lab, path, b"third plan")?;
    assert_eq!(
        history(&lab, BOB, path)?,
        std::slice::from_ref(&third),
        "the old head left with the new one"
    );
    let refusal = lab.rpc(BOB, &grant(&second)).expect_err("no longer the head");
    assert_eq!(refusal.code, ErrorCode::Denied);
    let all = [first, second, third];
    assert_eq!(history(&lab, ADMIN, path)?, all, "an admin sees everything");
    assert_eq!(
        history(&lab, ALICE, path)?,
        all,
        "alice could read the path from the start"
    );
    Ok(())
}

// contract: S4, S2
#[test]
fn an_outstanding_grant_url_dies_after_the_ttl() -> Result<()> {
    let Some(lab) = start()? else {
        return Ok(());
    };
    let version = write(&lab, "shared.bin", &random_bytes(1 << 20))?;
    let Response::Grants { urls, ttl_ms } = lab.rpc(BOB, &grant(&version))? else {
        bail!("grant answers with URLs");
    };
    let granted = Instant::now();
    let ttl = Duration::from_millis(ttl_ms);
    assert!(ttl <= QUICK.grant_ttl, "URLs valid for {ttl:?}, beyond the TTL");
    let http = lab.http(BOB)?;
    assert_eq!(http.get(&urls[0]).send()?.status(), StatusCode::OK);
    thread::sleep((granted + ttl + Duration::from_secs(1)).saturating_duration_since(Instant::now()));
    assert_eq!(http.get(&urls[0]).send()?.status(), StatusCode::FORBIDDEN);
    Ok(())
}

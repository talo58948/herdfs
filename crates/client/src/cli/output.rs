//! Human and JSON output for direct authority commands.

use herdfs_core::proto::{AclRow, Perm, SpaceAccess, Version};
use serde::Serialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub(super) fn json<T: Serialize + ?Sized>(value: &T) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}

pub(super) fn done(json_output: bool) -> anyhow::Result<()> {
    if json_output {
        json(&serde_json::json!({"ok": true}))?;
    }
    Ok(())
}

pub(super) fn versions(versions: &[Version], json_output: bool) -> anyhow::Result<()> {
    if json_output {
        return json(versions);
    }
    for version in versions {
        let nanos = i128::from(version.created_ms) * 1_000_000;
        let time = OffsetDateTime::from_unix_timestamp_nanos(nanos)
            .ok()
            .and_then(|time| time.format(&Rfc3339).ok())
            .unwrap_or_else(|| format!("{} ms since epoch", version.created_ms));
        println!(
            "{}\t{}\t{}\t{} B",
            version.id.0, time, version.author, version.size
        );
    }
    Ok(())
}

pub(super) fn spaces(spaces: &[SpaceAccess], json_output: bool) -> anyhow::Result<()> {
    if json_output {
        return json(spaces);
    }
    for space in spaces {
        let perm = space.root_perm.map_or("-", perm_name);
        println!("{}\t{perm}", space.space);
    }
    Ok(())
}

pub(super) fn grants(grants: &[AclRow], json_output: bool) -> anyhow::Result<()> {
    if json_output {
        return json(grants);
    }
    for grant in grants {
        println!("{}\t{}\t{}", grant.path, grant.principal, perm_name(grant.perm));
    }
    Ok(())
}

fn perm_name(perm: Perm) -> &'static str {
    match perm {
        Perm::Read => "read",
        Perm::Write => "write",
        Perm::Admin => "admin",
    }
}

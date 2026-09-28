//! Ring R6 helpers for `scripts/wan`.
//!
//! `r6 issuer DIR USER...` runs the lab's OIDC issuer for a real authority: it writes
//! `DIR/USER.token` for each user, prints `issuer=URL`, and serves until killed.
//!
//! `r6 twin NAME=RTT_MS:KB_S:FIRST_BYTE_MS...` runs the emulated twin of the real topology: a lab
//! with client `maker` on a LAN link and each NAME on a link of that round trip and bandwidth, to
//! a store with that time to first byte. It prints
//! `NAME=MOUNT` for every client, `authority=URL`, `admin=TOKEN` and `ready`. Then, until its
//! input ends, it obeys `cut NAME store|authority`, `restore NAME store|authority` and
//! `fresh NAME` (restart with an empty cache) lines, answering `ok` to each.

use std::io::{self, BufRead};
use std::{env, fs, thread};

use anyhow::{Context, Result, bail};
use herdfs_lab::{ADMIN, Issuer, Lab, LabConfig, Links, Profile, Strategy};

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.split_first() {
        Some((command, rest)) if command == "issuer" => issuer(rest),
        Some((command, rest)) if command == "twin" => twin(rest),
        _ => bail!("usage: r6 issuer DIR USER... | r6 twin NAME=RTT_MS:KB_S:FIRST_BYTE_MS..."),
    }
}

fn issuer(args: &[String]) -> Result<()> {
    let (dir, users) = args.split_first().context("issuer needs a directory")?;
    let issuer = Issuer::start()?;
    for user in users {
        fs::write(format!("{dir}/{user}.token"), issuer.token(user, &[] as &[&str]))?;
    }
    println!("issuer={}", issuer.url());
    loop {
        thread::park();
    }
}

fn twin(args: &[String]) -> Result<()> {
    let mut names = vec!["maker"];
    let mut config = LabConfig::default().client("maker", "maker", &[], Profile::Lan, Strategy::Stream);
    for arg in args {
        let (name, profile) =
            parse_link(arg).with_context(|| format!("{arg:?} is not NAME=RTT_MS:KB_S:FIRST_BYTE_MS"))?;
        config = config.client(name, name, &[], profile, Strategy::Stream);
        names.push(name);
    }
    let mut lab = Lab::start(config)?;
    for name in &names {
        println!("{name}={}", lab.mount_path(name).display());
    }
    println!("authority={}", lab.authority_url("maker"));
    println!("admin={}", lab.token(ADMIN, &[]));
    println!("ready");
    for line in io::stdin().lock().lines() {
        let line = line?;
        match line.split_whitespace().collect::<Vec<_>>()[..] {
            ["cut", name, link] => lab.cut(name, links(link)?)?,
            ["restore", name, link] => lab.restore(name, links(link)?)?,
            ["fresh", name] => fresh(&mut lab, name)?,
            _ => bail!("unknown command {line:?}"),
        }
        println!("ok");
    }
    Ok(())
}

fn links(name: &str) -> Result<Links> {
    match name {
        "store" => Ok(Links::Store),
        "authority" => Ok(Links::Authority),
        _ => bail!("unknown link {name:?}"),
    }
}

/// Restarts client `name` with an empty cache, so that its next reads are cold.
fn fresh(lab: &mut Lab, name: &str) -> Result<()> {
    lab.kill_client(name)?;
    let cache = lab.client_dir(name).join("cache");
    fs::remove_dir_all(&cache)?;
    fs::create_dir(&cache)?;
    lab.restart_client(name)
}

fn parse_link(arg: &str) -> Option<(&str, Profile)> {
    let (name, link) = arg.split_once('=')?;
    let [round_trip, rate, first_byte] = link.split(':').collect::<Vec<_>>()[..] else {
        return None;
    };
    let profile = Profile::Custom {
        round_trip_ms: round_trip.parse().ok()?,
        kilobytes_per_sec: rate.parse().ok()?,
        store_first_byte_ms: first_byte.parse().ok()?,
    };
    Some((name, profile))
}

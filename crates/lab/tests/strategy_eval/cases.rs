//! The cases of R5 and their ground truth: each workload runs on a cold client under every
//! strategy, which records its wall time and the bytes it fetched.

use std::fs::{self, File};
use std::io::Write;
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use herdfs_client::{AccessPlan, Keep, Prefetch, ReadAhead};
use herdfs_lab::{Lab, LabConfig, Profile, Strategy as Mounted};
use rand::{Rng, SeedableRng, rngs::SmallRng};

use crate::workload::Workload;

pub const MIB: u64 = 1 << 20;
const SIZES_MIB: [u64; 3] = [4, 64, 512];
pub const LINKS: [Profile; 2] = [Profile::Lan, Profile::Wan];
pub const PLANS: [(&str, AccessPlan, Mounted); 24] = [
    ("stream", AccessPlan::STREAM, Mounted::Stream),
    (
        "none-none-yes",
        AccessPlan::new(Prefetch::None, ReadAhead::None, Keep::Yes),
        Mounted::Plan("none/none/yes"),
    ),
    (
        "none-small-no",
        AccessPlan::new(Prefetch::None, ReadAhead::Small, Keep::No),
        Mounted::Plan("none/small/no"),
    ),
    (
        "none-small-yes",
        AccessPlan::new(Prefetch::None, ReadAhead::Small, Keep::Yes),
        Mounted::Plan("none/small/yes"),
    ),
    (
        "read-ahead",
        AccessPlan::new(Prefetch::None, ReadAhead::Large, Keep::No),
        Mounted::Plan("none/large/no"),
    ),
    (
        "none-large-yes",
        AccessPlan::new(Prefetch::None, ReadAhead::Large, Keep::Yes),
        Mounted::Plan("none/large/yes"),
    ),
    (
        "first-none-no",
        AccessPlan::new(Prefetch::First, ReadAhead::None, Keep::No),
        Mounted::Plan("first/none/no"),
    ),
    (
        "first-none-yes",
        AccessPlan::new(Prefetch::First, ReadAhead::None, Keep::Yes),
        Mounted::Plan("first/none/yes"),
    ),
    (
        "first-small-no",
        AccessPlan::new(Prefetch::First, ReadAhead::Small, Keep::No),
        Mounted::Plan("first/small/no"),
    ),
    (
        "first-small-yes",
        AccessPlan::new(Prefetch::First, ReadAhead::Small, Keep::Yes),
        Mounted::Plan("first/small/yes"),
    ),
    (
        "first-large-no",
        AccessPlan::new(Prefetch::First, ReadAhead::Large, Keep::No),
        Mounted::Plan("first/large/no"),
    ),
    (
        "first-large-yes",
        AccessPlan::new(Prefetch::First, ReadAhead::Large, Keep::Yes),
        Mounted::Plan("first/large/yes"),
    ),
    (
        "index",
        AccessPlan::new(Prefetch::Index, ReadAhead::None, Keep::No),
        Mounted::Plan("index/none/no"),
    ),
    (
        "index-none-yes",
        AccessPlan::new(Prefetch::Index, ReadAhead::None, Keep::Yes),
        Mounted::Plan("index/none/yes"),
    ),
    (
        "index-small-no",
        AccessPlan::new(Prefetch::Index, ReadAhead::Small, Keep::No),
        Mounted::Plan("index/small/no"),
    ),
    (
        "index-small-yes",
        AccessPlan::new(Prefetch::Index, ReadAhead::Small, Keep::Yes),
        Mounted::Plan("index/small/yes"),
    ),
    (
        "index-large-no",
        AccessPlan::new(Prefetch::Index, ReadAhead::Large, Keep::No),
        Mounted::Plan("index/large/no"),
    ),
    (
        "index-large-yes",
        AccessPlan::new(Prefetch::Index, ReadAhead::Large, Keep::Yes),
        Mounted::Plan("index/large/yes"),
    ),
    ("fetch", AccessPlan::FETCH, Mounted::Fetch),
    ("pin", AccessPlan::PIN, Mounted::Pin),
    (
        "whole-small-no",
        AccessPlan::new(Prefetch::Whole, ReadAhead::Small, Keep::No),
        Mounted::Plan("whole/small/no"),
    ),
    (
        "whole-small-yes",
        AccessPlan::new(Prefetch::Whole, ReadAhead::Small, Keep::Yes),
        Mounted::Plan("whole/small/yes"),
    ),
    (
        "whole-large-no",
        AccessPlan::new(Prefetch::Whole, ReadAhead::Large, Keep::No),
        Mounted::Plan("whole/large/no"),
    ),
    (
        "whole-large-yes",
        AccessPlan::new(Prefetch::Whole, ReadAhead::Large, Keep::Yes),
        Mounted::Plan("whole/large/yes"),
    ),
];
/// Strategies within this factor of the fastest time tie, and the one that fetched fewer bytes wins.
const TIE: f64 = 1.1;

/// A workload on one file over one link, with each strategy's wall time and bytes fetched.
pub struct Case {
    pub mib: u64,
    pub workload: Workload,
    pub link: Profile,
    pub seconds: Vec<f64>,
    pub bytes: Vec<u64>,
    /// Seconds, as a fresh client of the link measures it by its first open.
    pub round_trip: f64,
}

impl Case {
    pub fn name(&self) -> String {
        format!("{:?} {}MiB {:?}", self.link, self.mib, self.workload).to_lowercase()
    }

    /// The fastest strategy, or of those that tie with it, the one that fetched the fewest bytes.
    pub fn best(&self) -> AccessPlan {
        let fastest = self.seconds.iter().copied().fold(f64::INFINITY, f64::min);
        let mut ranked: Vec<usize> = (0..PLANS.len()).collect();
        ranked.sort_by_key(|&index| (self.seconds[index] > fastest * TIE, self.bytes[index]));
        PLANS[ranked[0]].1
    }

    pub fn regret(&self, plan: AccessPlan) -> f64 {
        let Some(index) = PLANS.iter().position(|(_, fixed, _)| *fixed == plan) else {
            panic!("R5 did not measure {}", plan.name());
        };
        let Some(best) = PLANS.iter().position(|(_, fixed, _)| *fixed == self.best()) else {
            panic!("R5's best plan was not measured");
        };
        self.seconds[index] / self.seconds[best]
    }
}

/// Every case, measured under each strategy.
pub fn measure_all() -> Result<Vec<Case>> {
    let mut lab = start_lab()?;
    write_files(&lab)?;
    let layout = LINKS
        .into_iter()
        .flat_map(|link| SIZES_MIB.map(|mib| (link, mib)));
    let mut cases = Vec::new();
    for ((link, mib), workload) in layout.flat_map(|pair| Workload::ALL.map(|workload| (pair, workload))) {
        let round_trip = round_trip(&mut lab, link, mib)?;
        let mut case = Case {
            mib,
            workload,
            link,
            seconds: vec![0.0; PLANS.len()],
            bytes: vec![0; PLANS.len()],
            round_trip,
        };
        for (index, (name, _, _)) in PLANS.iter().enumerate() {
            let client = client(link, name);
            (case.seconds[index], case.bytes[index]) = measure(&mut lab, &client, &case)?;
        }
        eprintln!(
            "{}: {:.2?} s, {:?} bytes, {round_trip:.4} s round trip",
            case.name(),
            case.seconds,
            case.bytes
        );
        cases.push(case);
    }
    Ok(cases)
}

/// A writer, and on each link one client per strategy.
fn start_lab() -> Result<Lab> {
    // On tmpfs: the files and caches of this lab outgrow the CI host's spare disk.
    let config = LabConfig::default().on_tmpfs();
    let mut config = config.client("writer", "alice", &[], Profile::Lan, Mounted::Stream);
    for (link, strategy) in LINKS.into_iter().flat_map(|link| PLANS.map(|plan| (link, plan))) {
        config = config.client(&client(link, strategy.0), "alice", &[], link, strategy.2);
    }
    Lab::start(config)
}

fn client(link: Profile, plan: &str) -> String {
    format!("{link:?}-{plan}").to_lowercase()
}

fn file_name(mib: u64) -> String {
    format!("{mib}m.bin")
}

/// Writes a file of random bytes of each size through the writer.
fn write_files(lab: &Lab) -> Result<()> {
    let (mut rng, mut block) = (SmallRng::seed_from_u64(1), vec![0; MIB as usize]);
    for mib in SIZES_MIB {
        let mut file = File::create(lab.mount_path("writer").join(file_name(mib)))?;
        for _ in 0..mib {
            rng.fill_bytes(&mut block);
            file.write_all(&block)?;
        }
        // fsync returns once the authority has committed the version.
        file.sync_all()?;
    }
    Ok(())
}

/// Runs the case's workload on a cold mount of `client`: its wall time in seconds, and the
/// bytes the client had fetched when it ended.
fn measure(lab: &mut Lab, client: &str, case: &Case) -> Result<(f64, u64)> {
    lab.restart_client(client)?;
    let script = case
        .workload
        .script(&lab.mount_path(client).join(file_name(case.mib)), case.mib);
    let started = Instant::now();
    let status = Command::new("sh").args(["-c", &script]).status()?;
    let seconds = started.elapsed().as_secs_f64();
    ensure!(status.success(), "{} on {client}: {status}", case.name());
    let bytes = lab.stats(client)?["bytes_fetched"]
        .as_u64()
        .context("stats lack bytes_fetched")?;
    // Also stops a Fetch or Pin prefetch from competing with the next run.
    stop_cold(lab, client)?;
    Ok((seconds, bytes))
}

/// Stops `client` and empties its cache, so that it holds no bytes until it mounts again cold.
fn stop_cold(lab: &mut Lab, client: &str) -> Result<()> {
    lab.kill_client(client)?;
    let cache = lab.client_dir(client).join("cache");
    fs::remove_dir_all(&cache)?;
    Ok(fs::create_dir(&cache)?)
}

/// The round trip, in seconds, that a fresh client of `link` has measured when it first opens
/// the file of `mib` MiB, as its stats report it.
fn round_trip(lab: &mut Lab, link: Profile, mib: u64) -> Result<f64> {
    let client = client(link, "stream");
    lab.restart_client(&client)?;
    File::open(lab.mount_path(&client).join(file_name(mib)))?;
    let millis = lab.stats(&client)?["round_trip_ms"].as_f64();
    stop_cold(lab, &client)?;
    Ok(millis.context("stats lack round_trip_ms")? / 1e3)
}

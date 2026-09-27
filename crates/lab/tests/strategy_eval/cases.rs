//! The cases of R5 and their ground truth: each workload runs on a cold client under every
//! strategy, which records its wall time and the bytes it fetched.

use std::fs::File;
use std::io::Write;
use std::iter;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use herdfs_client::Strategy;
use herdfs_lab::{Lab, LabConfig, Profile, Strategy as Mounted};
use rand::{Rng, RngExt, SeedableRng, rngs::SmallRng};

pub const MIB: u64 = 1 << 20;
const SIZES_MIB: [u64; 3] = [4, 64, 512];
pub const LINKS: [Profile; 2] = [Profile::Lan, Profile::Wan];
/// Strategies within this factor of the fastest time tie, and the one that fetched fewer bytes wins.
const TIE: f64 = 1.1;

#[derive(Clone, Copy, Debug)]
pub enum Workload {
    Head,
    Read,
    ReadThrice,
    Random,
}

impl Workload {
    const ALL: [Self; 4] = [Self::Head, Self::Read, Self::ReadThrice, Self::Random];

    /// The process that opens the file.
    pub fn comm(self) -> &'static str {
        match self {
            Self::Head => "head",
            Self::Read | Self::ReadThrice => "sha256sum",
            Self::Random => "dd",
        }
    }

    /// A shell script that reads `file`, of `mib` MiB, this way.
    fn script(self, file: &Path, mib: u64) -> String {
        let file = file.display();
        let mut rng = SmallRng::seed_from_u64(mib);
        let random_read = || {
            let skip = rng.random_range(0..mib);
            format!("dd if={file} of=/dev/null bs=1M count=1 skip={skip} status=none")
        };
        match self {
            Self::Head => format!("head -c 65536 {file} >/dev/null"),
            Self::Read => format!("sha256sum {file} >/dev/null"),
            Self::ReadThrice => format!("for i in 1 2 3; do sha256sum {file} >/dev/null; done"),
            Self::Random => iter::repeat_with(random_read)
                .take(20)
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

/// A workload on one file over one link, with each strategy's wall time and bytes fetched.
pub struct Case {
    pub mib: u64,
    pub workload: Workload,
    pub link: Profile,
    pub seconds: [f64; 3],
    pub bytes: [u64; 3],
}

impl Case {
    pub fn name(&self) -> String {
        format!("{:?} {}MiB {:?}", self.link, self.mib, self.workload).to_lowercase()
    }

    /// The fastest strategy, or of those that tie with it, the one that fetched the fewest bytes.
    pub fn best(&self) -> Strategy {
        let fastest = self.seconds.into_iter().fold(f64::INFINITY, f64::min);
        let mut ranked = Strategy::ALL;
        ranked.sort_by_key(|&strategy| {
            let index = strategy as usize;
            (self.seconds[index] > fastest * TIE, self.bytes[index])
        });
        ranked[0]
    }

    pub fn regret(&self, strategy: Strategy) -> f64 {
        self.seconds[strategy as usize] / self.seconds[self.best() as usize]
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
        let mut case = Case {
            mib,
            workload,
            link,
            seconds: [0.0; 3],
            bytes: [0; 3],
        };
        for strategy in Strategy::ALL {
            let client = client(link, strategy);
            (case.seconds[strategy as usize], case.bytes[strategy as usize]) =
                measure(&mut lab, &client, &case)?;
        }
        eprintln!("{}: {:.2?} s, {:?} bytes", case.name(), case.seconds, case.bytes);
        cases.push(case);
    }
    Ok(cases)
}

/// A writer, and on each link one client per strategy.
fn start_lab() -> Result<Lab> {
    let mut config = LabConfig::default().client("writer", "alice", &[], Profile::Lan, Mounted::Stream);
    for (link, strategy) in LINKS
        .into_iter()
        .flat_map(|link| Strategy::ALL.map(|strategy| (link, strategy)))
    {
        let mounted = match strategy {
            Strategy::Stream => Mounted::Stream,
            Strategy::Fetch => Mounted::Fetch,
            Strategy::Pin => Mounted::Pin,
        };
        config = config.client(&client(link, strategy), "alice", &[], link, mounted);
    }
    Lab::start(config)
}

fn client(link: Profile, strategy: Strategy) -> String {
    format!("{link:?}-{}", strategy.name()).to_lowercase()
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
    lab.restart_client_cold(client)?;
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
    // Stops a Fetch or Pin prefetch from competing with the next run.
    lab.kill_client(client)?;
    Ok((seconds, bytes))
}

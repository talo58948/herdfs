use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use herdfs_core::SpacePath;
use herdfs_core::proto::{Perm, Request, Response, Version};
use herdfs_lab::{ADMIN, Lab, LabConfig, Profile, SPACE, Strategy};

pub(crate) const PIN_OLD_LEN: usize = 512 << 10;
pub(crate) const PIN_NEW_LEN: usize = 8 << 20;
pub(crate) const PIN_TRIALS: usize = 3;
pub(crate) const SLOW_ANSWERS: Profile = Profile::Custom {
    round_trip_ms: 1_000,
    kilobytes_per_sec: 100_000,
    store_first_byte_ms: 0,
};
const COMMITTED_WITHIN: Duration = Duration::from_secs(30);

pub(crate) struct TestChild(Child);

impl TestChild {
    pub(crate) fn agents() -> Result<Command> {
        let test = std::env::current_exe()?;
        let binary = test
            .parent()
            .and_then(Path::parent)
            .context("test binary is under target/debug/deps")?
            .join("herdfs-agents");
        Ok(Command::new(binary))
    }

    pub(crate) fn spawn(mut command: Command) -> Result<Self> {
        Ok(Self(command.spawn()?))
    }
}

impl Drop for TestChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub(crate) fn two_clients() -> Result<Option<Lab>> {
    start(two_client_config())
}

pub(crate) fn cache_lab(cache_bytes: u64) -> Result<Option<Lab>> {
    start(two_client_config().cache_bytes("b", cache_bytes))
}

fn two_client_config() -> LabConfig {
    LabConfig::default()
        .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
        .client("b", "bob", &[], Profile::Lan, Strategy::Stream)
}

fn start(config: LabConfig) -> Result<Option<Lab>> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(None);
    }
    Lab::start(config).map(Some)
}

pub(crate) fn pin_lab() -> Result<Option<Lab>> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(None);
    }
    let config = LabConfig::default()
        .client("a", "alice", &[], Profile::Lan, Strategy::Stream)
        .client("b", "bob", &[], Profile::Lan, Strategy::Pin)
        .cache_bytes("b", 1 << 20);
    Lab::start(config).map(Some)
}

pub(crate) fn stays_pinned(lab: &Lab, file: &File, bytes: &[u8], what: &str) -> Result<bool> {
    assert_bytes(&read_at(file, 0, bytes.len())?, bytes, what);
    lab.cut("b", herdfs_lab::Links::Store)?;
    let reread = read_at(file, 0, 4096);
    lab.restore("b", herdfs_lab::Links::Store)?;
    Ok(reread.is_ok_and(|page| page == bytes[..4096]))
}

pub(crate) fn replay_history(lab: &Lab, path: &str) -> Result<Vec<Version>> {
    let request = Request::History {
        space: SPACE.into(),
        path: SpacePath::try_from(path)?,
    };
    match lab.rpc(ADMIN, &request)? {
        Response::History { versions } => Ok(versions),
        reply => bail!("history answers with versions, not {reply:?}"),
    }
}

pub(crate) fn wait_for_rewrite(lab: &Lab, path: &str, known: &Version) -> Result<Version> {
    let deadline = Instant::now() + COMMITTED_WITHIN;
    loop {
        if let Some(version) = replay_history(lab, path)?
            .into_iter()
            .find(|version| version != known)
        {
            return Ok(version);
        }
        ensure!(Instant::now() < deadline, "no commit after {COMMITTED_WITHIN:?}");
        thread::sleep(Duration::from_millis(2));
    }
}

pub(crate) fn journaled(lab: &Lab) -> Result<bool> {
    for record in std::fs::read_dir(lab.client_dir("a").join("journal"))? {
        if record?.path().extension().is_some_and(|extension| extension == "json") {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn set_alice(lab: &Lab, perm: Option<Perm>) -> Result<()> {
    lab.rpc(
        ADMIN,
        &Request::SetAcl {
            space: SPACE.into(),
            path: SpacePath::root(),
            principal: "user:alice".into(),
            perm,
        },
    )?;
    Ok(())
}

pub(crate) fn save(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

pub(crate) fn wait_pending(lab: &Lab) -> Result<()> {
    for _ in 0..200 {
        if lab.stats("b")?["pending_commits"] == 1 {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
    bail!("offline write was not journaled")
}

pub(crate) fn read_at(file: &File, off: u64, len: usize) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0; len];
    file.read_exact_at(&mut bytes, off)?;
    Ok(bytes)
}

pub(crate) fn stat_count(lab: &Lab, name: &str) -> Result<u64> {
    lab.stats("b")?[name]
        .as_u64()
        .with_context(|| format!("stats count {name}"))
}

pub(crate) fn chunk_starts(version: &Version, bytes: &[u8], count: usize) -> Vec<(u64, u8)> {
    let mut offset = 0_u64;
    version
        .chunks
        .iter()
        .take(count)
        .map(|chunk| {
            let start = (offset, bytes[offset as usize]);
            offset += u64::from(chunk.len);
            start
        })
        .collect()
}

pub(crate) fn read_starts(file: &File, starts: &[(u64, u8)]) -> Result<()> {
    thread::scope(|scope| {
        let reads = starts
            .iter()
            .map(|&(offset, expected)| scope.spawn(move || (read_at(file, offset, 1), expected)))
            .collect::<Vec<_>>();
        for read in reads {
            let (bytes, expected) = read.join().map_err(|_| anyhow::anyhow!("read thread panicked"))?;
            ensure!(bytes? == [expected], "wrong byte at a chunk boundary");
        }
        Ok(())
    })
}

pub(crate) fn random_bytes(len: usize) -> Vec<u8> {
    let mut bytes = vec![0; len];
    rand::fill(&mut bytes[..]);
    bytes
}

pub(crate) fn assert_bytes(got: &[u8], expected: &[u8], what: &str) {
    let differs = got.iter().zip(expected).position(|(got, expected)| got != expected);
    assert!(
        got == expected,
        "{what}: {} bytes, {} expected; first difference at {differs:?}",
        got.len(),
        expected.len()
    );
}

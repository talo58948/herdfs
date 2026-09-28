//! R1: a seeded random workload drives `Authority` directly, next to a
//! reference model, and checks the invariants after every step.
//!
//! `SIM_SEEDS=n` runs seeds `0..n` (default 200); `SIM_SEED=s` replays one.
//! Time comes from the sim and every choice from its seeded generator, so a
//! seed always replays the same run.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helpers outside #[test] fns are test code too"
)]

mod apply;
mod check;
mod choose;
mod content;
mod model;
mod steps;
mod versions;

use std::{
    panic,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use choose::Rng;
use model::Model;
use herdfs_authority::{Authority, Outcome, Principal};
use herdfs_core::{
    ChunkRef, SpacePath,
    proto::{EntryId, ErrorCode, Perm, Request},
};
use tempfile::TempDir;

const STEPS: usize = 150;
/// Names no message contains by chance (`x` and `y` are not hex), so a leaked path shows.
const NAMES: [&str; 2] = ["x7", "y8"];
const SPACES: [&str; 5] = ["s", "t", "u", "Bad", ".x"];
const GRANTEES: [&str; 4] = ["user:bob", "user:carol", "group:eng", "carol"];
/// Index of the auditor in `Sim::actors`; the auditor only observes.
const AUDITOR: usize = 3;
const NO_ENTRY: EntryId = EntryId(1 << 40);

#[test]
fn sim() {
    let seeds: Vec<u64> = match env("SIM_SEED") {
        Some(seed) => vec![seed],
        None => (0..env("SIM_SEEDS").unwrap_or(200)).collect(),
    };
    let next = AtomicUsize::new(0);
    let failed = Mutex::new(Vec::new());
    let threads = std::thread::available_parallelism().map_or(1, usize::from);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                while let Some(&seed) = seeds.get(next.fetch_add(1, Ordering::Relaxed)) {
                    if !failed.lock().unwrap().is_empty() {
                        break;
                    }
                    if panic::catch_unwind(|| Sim::new(seed).run()).is_err() {
                        eprintln!("sim seed {seed} failed; replay it with SIM_SEED={seed}");
                        failed.lock().unwrap().push(seed);
                    }
                }
            });
        }
    });
    let mut failed = failed.into_inner().unwrap();
    failed.sort_unstable();
    assert!(
        failed.is_empty(),
        "sim failed for seeds {failed:?}; replay one with SIM_SEED=<seed>"
    );
}

fn env(name: &str) -> Option<u64> {
    let value = std::env::var(name).ok()?;
    Some(
        value
            .parse()
            .unwrap_or_else(|_| panic!("{name} must be a number, not {value:?}")),
    )
}

pub struct Sim {
    rng: Rng,
    now_ms: i64,
    dir: TempDir,
    authority: Authority,
    model: Model,
    /// alice, bob, carol (in group eng) and the auditor.
    actors: Vec<Principal>,
    /// Every commit request sent, for retries.
    sent: Vec<(usize, Request)>,
    chunks: Vec<ChunkRef>,
}

impl Sim {
    fn new(seed: u64) -> Self {
        // The sim checks logic, not the disk, and on tmpfs fsync costs nothing.
        let dir = tempfile::tempdir_in("/dev/shm")
            .or_else(|_| tempfile::tempdir())
            .unwrap();
        let authority = Authority::open(&dir.path().join("sim.db")).unwrap();
        let actor = |name: &str, groups: &[&str]| Principal {
            user: format!("user:{name}"),
            groups: groups.iter().map(|group| (*group).to_owned()).collect(),
        };
        let actors = vec![
            actor("alice", &[]),
            actor("bob", &[]),
            actor("carol", &["eng"]),
            actor("auditor", &[]),
        ];
        let rng = Rng(seed);
        let (model, sent, chunks) = (Model::default(), Vec::new(), Vec::new());
        let mut sim = Self {
            rng,
            now_ms: 0,
            dir,
            authority,
            model,
            actors,
            sent,
            chunks,
        };
        sim.exec(0, &Request::CreateSpace { space: "s".into() }).unwrap();
        sim.let_auditor_in("s");
        sim
    }

    fn run(mut self) {
        for _ in 0..STEPS {
            self.step();
            self.check();
        }
    }

    fn step(&mut self) {
        match self.rng.below(100) {
            0..=2 => self.restart(),
            3 | 4 => self.new_space(),
            5 => self.deep_rename(),
            6..=25 => self.make(),
            26..=30 => self.remove(),
            31..=39 => self.rename(),
            40..=42 => self.set_attr(),
            43..=59 => self.commit(),
            60..=63 => self.race(),
            64..=67 => self.retry(),
            68..=71 => self.restore(),
            72..=79 => self.set_acl(),
            80..=90 => self.read(),
            _ => self.grant(),
        }
    }

    /// Sends `request` as actor `who`, checks the outcome against the model,
    /// and folds an acknowledged one into it.
    fn exec(&mut self, who: usize, request: &Request) -> Result<Outcome, ErrorCode> {
        self.now_ms += 1 + self.rng.below(1_000) as i64;
        let principal = self.actors[who].clone();
        let want = self.model.outcome(&principal, request);
        let got = self.authority.handle(&principal, request.clone(), self.now_ms);
        if let (Request::Commit { .. } | Request::Grant { .. }, Err(error)) = (request, &got) {
            self.check_refusal(&principal, &error.message);
        }
        let got = got.map_err(|error| error.code);
        assert_eq!(
            got.as_ref().err(),
            want.as_ref().err(),
            "{} sent {request:?}; left is the authority, right the model",
            principal.user
        );
        let Ok(outcome) = &got else { return got };
        match request {
            Request::Fork { space, name } => {
                let snapshot = self.observe(name);
                self.model.adopt_fork(space, name, &snapshot);
            }
            _ => self.model.apply(&principal, request, outcome, self.now_ms),
        }
        got
    }

    fn send(&mut self, who: usize, request: &Request) {
        let _ = self.exec(who, request);
    }

    fn restart(&mut self) {
        self.authority = Authority::open(&self.dir.path().join("sim.db")).unwrap();
    }

    fn let_auditor_in(&mut self, space: &str) {
        let principal = "user:auditor".into();
        let request = Request::SetAcl {
            space: space.into(),
            path: SpacePath::root(),
            principal,
            perm: Some(Perm::Admin),
        };
        let creator = self.model.spaces[space]
            .acl
            .keys()
            .find_map(|(_, who)| self.actors.iter().position(|actor| actor.user == *who));
        self.exec(creator.expect("the space has an admin"), &request)
            .unwrap();
    }
}

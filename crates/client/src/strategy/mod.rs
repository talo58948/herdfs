//! Access strategies and the deciders that choose one per open (P1).

mod jev;

use std::{str::FromStr, sync::Arc};

use rand::{RngExt, SeedableRng, rngs::SmallRng};
use tokio::sync::oneshot;

pub use jev::Jev;

use crate::stats::Stats;

/// The rule decider fetches files below this size whole.
const FETCH_BELOW: u64 = 64 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// Fetch chunks as reads reach them.
    Stream,
    /// Prefetch every chunk in the background.
    Fetch,
    /// Prefetch every chunk and keep it cached while the file is open.
    Pin,
}

impl Strategy {
    pub const ALL: [Self; 3] = [Self::Stream, Self::Fetch, Self::Pin];

    pub fn name(self) -> &'static str {
        match self {
            Self::Stream => "stream",
            Self::Fetch => "fetch",
            Self::Pin => "pin",
        }
    }

    /// The rule decider's choice: fetch files under 64 MiB whole, stream larger ones.
    pub fn by_rule(context: &OpenContext) -> Self {
        match context.size {
            ..FETCH_BELOW => Self::Fetch,
            _ => Self::Stream,
        }
    }
}

/// What a decider knows about one open.
#[derive(Clone)]
pub struct OpenContext {
    pub size: u64,
    pub extension: Option<String>,
    /// The opening process's name, from `/proc/<pid>/comm`.
    pub comm: String,
    /// The fraction of the version's bytes already cached.
    pub coverage: f64,
    /// Recent download throughput, in bytes per second.
    pub throughput: f64,
}

/// A strategy Jev chose after an open that began with Stream.
pub type Upgrade = oneshot::Receiver<Strategy>;
/// The strategy for an open, and a later one from Jev if the open misses its answers.
pub type Decision = (Strategy, Option<Upgrade>);

#[derive(Clone)]
pub enum Decider {
    Fixed(Strategy),
    Rule,
    /// Seeded random choices, so tests reach every strategy reproducibly (C8).
    Chaos(SmallRng),
    /// Jev's remembered answers, else the rule's choice while Jev is asked.
    Jev(Arc<Jev>, Arc<Stats>),
}

impl Decider {
    pub fn decide(&mut self, context: &OpenContext) -> Decision {
        match self {
            Self::Fixed(strategy) => (*strategy, None),
            Self::Rule => (Strategy::by_rule(context), None),
            Self::Chaos(rng) => (Strategy::ALL[rng.random_range(0..Strategy::ALL.len())], None),
            Self::Jev(jev, stats) => jev.decide(context, stats),
        }
    }
}

/// A `--strategy` value. Jev needs the key from `--jev-key-file` before it can decide.
#[derive(Clone)]
pub enum StrategyArg {
    Decider(Decider),
    Jev,
}

impl FromStr for StrategyArg {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, String> {
        let fixed = Strategy::ALL.into_iter().find(|s| s.name() == value);
        let decider = match (value, fixed) {
            ("jev", _) => return Ok(Self::Jev),
            ("rule", _) => Decider::Rule,
            (_, Some(strategy)) => Decider::Fixed(strategy),
            _ => {
                let unknown =
                    || format!("unknown strategy {value:?}; use stream, fetch, pin, rule, chaos:SEED or jev");
                let seed = value.strip_prefix("chaos:").ok_or_else(unknown)?;
                let seed = seed.parse().map_err(|_| format!("invalid chaos seed {seed:?}"))?;
                Decider::Chaos(SmallRng::seed_from_u64(seed))
            }
        };
        Ok(Self::Decider(decider))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(size: u64) -> OpenContext {
        OpenContext {
            size,
            extension: None,
            comm: "cat".into(),
            coverage: 0.0,
            throughput: 10e6,
        }
    }

    fn decider(value: &str) -> Decider {
        let Ok(StrategyArg::Decider(decider)) = value.parse() else {
            panic!("{value} names no local decider");
        };
        decider
    }

    // contract: P1
    #[test]
    fn rule_fetches_small_files_and_streams_large_ones() {
        let mut rule = decider("rule");
        assert_eq!(rule.decide(&context(FETCH_BELOW - 1)).0, Strategy::Fetch);
        assert_eq!(rule.decide(&context(FETCH_BELOW)).0, Strategy::Stream);
        assert_eq!(decider("pin").decide(&context(1)).0, Strategy::Pin);
    }

    // contract: P1, C8
    #[test]
    fn chaos_is_reproducible_and_reaches_every_strategy() {
        let picks = |seed: &str| {
            let mut chaos = decider(seed);
            (0..64).map(|_| chaos.decide(&context(1)).0).collect::<Vec<_>>()
        };
        let first = picks("chaos:7");
        assert_eq!(first, picks("chaos:7"));
        assert_ne!(first, picks("chaos:8"));
        assert!(Strategy::ALL.iter().all(|strategy| first.contains(strategy)));
    }

    #[test]
    fn names_jev_and_rejects_unknown_strategies() {
        assert!(matches!("jev".parse(), Ok(StrategyArg::Jev)));
        assert!("chaos:x".parse::<StrategyArg>().is_err());
        assert!("jevv".parse::<StrategyArg>().is_err());
    }
}

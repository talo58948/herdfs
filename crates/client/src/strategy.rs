//! Access strategies and the deciders that choose one per open (P1).

use std::str::FromStr;

use rand::{RngExt, SeedableRng, rngs::SmallRng};

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
}

/// What a decider knows about one open.
pub struct OpenContext {
    pub size: u64,
    pub extension: Option<String>,
    /// The opening process's name, from `/proc/<pid>/comm`.
    pub comm: String,
    /// The fraction of the version's bytes already cached.
    pub coverage: f64,
}

#[derive(Clone, Debug)]
pub enum Decider {
    Fixed(Strategy),
    /// Fetch files under 64 MiB, stream larger ones.
    Rule,
    /// Seeded random choices, so tests reach every strategy reproducibly (C8).
    Chaos(SmallRng),
}

impl Decider {
    pub fn decide(&mut self, context: &OpenContext) -> Strategy {
        match self {
            Self::Fixed(strategy) => *strategy,
            Self::Rule if context.size < FETCH_BELOW => Strategy::Fetch,
            Self::Rule => Strategy::Stream,
            Self::Chaos(rng) => Strategy::ALL[rng.random_range(0..Strategy::ALL.len())],
        }
    }
}

impl FromStr for Decider {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, String> {
        if let Some(seed) = value.strip_prefix("chaos:") {
            let seed = seed.parse().map_err(|_| format!("invalid chaos seed {seed:?}"))?;
            return Ok(Self::Chaos(SmallRng::seed_from_u64(seed)));
        }
        if value == "rule" {
            return Ok(Self::Rule);
        }
        Strategy::ALL
            .into_iter()
            .find(|strategy| strategy.name() == value)
            .map(Self::Fixed)
            .ok_or_else(|| format!("unknown strategy {value:?}; use stream, fetch, pin, rule or chaos:SEED"))
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
        }
    }

    // contract: P1
    #[test]
    fn rule_fetches_small_files_and_streams_large_ones() {
        let mut rule: Decider = "rule".parse().unwrap();
        assert_eq!(rule.decide(&context(FETCH_BELOW - 1)), Strategy::Fetch);
        assert_eq!(rule.decide(&context(FETCH_BELOW)), Strategy::Stream);
        let mut pin: Decider = "pin".parse().unwrap();
        assert_eq!(pin.decide(&context(1)), Strategy::Pin);
    }

    // contract: P1, C8
    #[test]
    fn chaos_is_reproducible_and_reaches_every_strategy() {
        let picks = |seed: &str| {
            let mut chaos: Decider = seed.parse().unwrap();
            (0..64).map(|_| chaos.decide(&context(1))).collect::<Vec<_>>()
        };
        let first = picks("chaos:7");
        assert_eq!(first, picks("chaos:7"));
        assert_ne!(first, picks("chaos:8"));
        assert!(Strategy::ALL.iter().all(|strategy| first.contains(strategy)));
    }

    #[test]
    fn rejects_unknown_strategies() {
        assert!("jev".parse::<Decider>().is_err());
        assert!("chaos:x".parse::<Decider>().is_err());
    }
}

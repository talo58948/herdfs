//! Access plans and the deciders that choose one per access (P1-P3).

mod arg;
mod jev;
mod record;

use std::sync::Arc;

use rand::{RngExt, rngs::SmallRng};
use serde::{Serialize, Serializer};
use tokio::sync::oneshot;

pub(crate) use arg::StrategyArg;
pub use jev::Jev;
pub(crate) use record::AccessRecord;

use crate::stats::Stats;

/// Files smaller than 64 MiB are fetched whole; larger files stream.
const FETCH_BELOW: u64 = 64 << 20;

pub(crate) trait Setting: Copy + 'static {
    const ALL: &'static [Self];

    fn name(self) -> &'static str;

    fn parse(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|setting| setting.name() == name)
    }
}

macro_rules! settings {
    ($name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
        pub enum $name {
            $($variant),+
        }

        impl Setting for $name {
            const ALL: &'static [Self] = &[$(Self::$variant),+];

            fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $value),+
                }
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.name())
            }
        }
    };
}

settings!(Prefetch {
    None => "none",
    First => "first",
    Index => "index",
    Whole => "whole",
});
settings!(ReadAhead {
    None => "none",
    Small => "small",
    Large => "large",
});
settings!(Keep { No => "no", Yes => "yes" });

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct AccessPlan {
    pub prefetch: Prefetch,
    pub read_ahead: ReadAhead,
    pub keep: Keep,
}

impl AccessPlan {
    pub(crate) const NONE: Self = Self::new(Prefetch::None, ReadAhead::None, Keep::No);
    pub const STREAM: Self = Self::new(Prefetch::None, ReadAhead::None, Keep::No);
    pub const FETCH: Self = Self::new(Prefetch::Whole, ReadAhead::None, Keep::No);
    pub const PIN: Self = Self::new(Prefetch::Whole, ReadAhead::None, Keep::Yes);

    pub const fn new(prefetch: Prefetch, read_ahead: ReadAhead, keep: Keep) -> Self {
        Self {
            prefetch,
            read_ahead,
            keep,
        }
    }

    /// Applies a late decision without reducing any setting already in effect (P1).
    pub fn merged(self, later: Self) -> Self {
        Self::new(
            add_prefetch(self.prefetch, later.prefetch),
            self.read_ahead.max(later.read_ahead),
            self.keep.max(later.keep),
        )
    }

    pub fn name(self) -> String {
        let (prefetch, read_ahead, keep) = (self.prefetch.name(), self.read_ahead.name(), self.keep.name());
        format!("{prefetch}/{read_ahead}/{keep}")
    }
}

fn add_prefetch(now: Prefetch, later: Prefetch) -> Prefetch {
    match (now, later) {
        (Prefetch::None, other) | (other, Prefetch::None) => other,
        (now, later) if now == later => now,
        // First and format-index regions need not overlap, so their representable union is whole.
        _ => Prefetch::Whole,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PlanSource {
    Remembered,
    Rule,
    Jev,
    Fixed,
    Chaos,
}

/// What a decider knows about one access.
#[derive(Clone, Debug)]
pub struct OpenContext {
    pub size: u64,
    pub extension: Option<String>,
    /// The opening process's name, when the platform provides it.
    pub comm: String,
    /// The fraction of the version's bytes already cached.
    pub coverage: f64,
    /// Recent download throughput in bytes per second, if measured yet.
    pub throughput: Option<f64>,
    /// Recent round-trip time to the authority, in seconds.
    pub round_trip: f64,
    /// Recent accesses by this process to the same entry.
    pub opens: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
struct Chosen<T> {
    value: T,
    confidence: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct JevAnswer {
    prefetch: Option<Chosen<Prefetch>>,
    read_ahead: Option<Chosen<ReadAhead>>,
    keep: Option<Chosen<Keep>>,
}

impl JevAnswer {
    pub fn plan(self, fallback: AccessPlan) -> AccessPlan {
        AccessPlan::new(
            accepted(self.prefetch).map_or(fallback.prefetch, |answer| answer.value),
            accepted(self.read_ahead).map_or(fallback.read_ahead, |answer| answer.value),
            accepted(self.keep).map_or(fallback.keep, |answer| answer.value),
        )
    }

    fn confidences(self) -> [Option<f64>; 3] {
        [
            self.prefetch.map(|answer| answer.confidence),
            self.read_ahead.map(|answer| answer.confidence),
            self.keep.map(|answer| answer.confidence),
        ]
    }

    fn any_confident(self) -> bool {
        self.confidences()
            .into_iter()
            .flatten()
            .any(|confidence| confidence >= Jev::CONFIDENT)
    }

    pub(crate) fn source(self, confident: PlanSource) -> PlanSource {
        if self.any_confident() {
            confident
        } else {
            PlanSource::Rule
        }
    }
}

fn accepted<T>(answer: Option<Chosen<T>>) -> Option<Chosen<T>> {
    answer.filter(|answer| answer.confidence >= Jev::CONFIDENT)
}

pub(crate) type Upgrade = oneshot::Receiver<JevAnswer>;

#[derive(Clone, Copy, Serialize)]
pub(crate) struct Decided {
    pub plan: AccessPlan,
    pub source: PlanSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<JevAnswer>,
}

pub(crate) struct Decision {
    pub decided: Decided,
    pub upgrade: Option<Upgrade>,
}

#[derive(Clone)]
pub(crate) enum Decider {
    /// The plan explicitly selected on the command line.
    Fixed(AccessPlan),
    /// Fetch files under 64 MiB and stream larger ones.
    Rule,
    /// Seeded random settings let tests reach every plan reproducibly (C8).
    Chaos(SmallRng),
    /// Jev's remembered answers, else the rule while one request runs.
    Jev(Arc<Jev>, Arc<Stats>),
}

impl Decider {
    pub(crate) fn decide(&mut self, context: &OpenContext) -> Decision {
        let (plan, source) = match self {
            Self::Fixed(plan) => (*plan, PlanSource::Fixed),
            Self::Rule => (rule(context), PlanSource::Rule),
            Self::Chaos(rng) => (chaos(rng), PlanSource::Chaos),
            Self::Jev(jev, stats) => return jev.decide(context, stats),
        };
        Decision {
            decided: Decided {
                plan,
                source,
                answer: None,
            },
            upgrade: None,
        }
    }
}

pub(super) fn rule(context: &OpenContext) -> AccessPlan {
    if context.size < FETCH_BELOW {
        AccessPlan::FETCH
    } else {
        AccessPlan::STREAM
    }
}

fn chaos(rng: &mut SmallRng) -> AccessPlan {
    AccessPlan::new(
        Prefetch::ALL[rng.random_range(0..Prefetch::ALL.len())],
        ReadAhead::ALL[rng.random_range(0..ReadAhead::ALL.len())],
        Keep::ALL[rng.random_range(0..Keep::ALL.len())],
    )
}

#[cfg(test)]
mod tests;

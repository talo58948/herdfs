//! Access plans and the deciders that choose one per access (P1-P3).

mod arg;
mod jev;
mod record;

use std::sync::Arc;

use rand::{RngExt, rngs::SmallRng};
use serde::Serialize;
use tokio::sync::oneshot;

pub use arg::StrategyArg;
pub use jev::Jev;
pub(crate) use record::AccessRecord;

use crate::stats::Stats;

/// The rule keeps today's cutoff: small files are fetched whole, larger files stream.
const FETCH_BELOW: u64 = 64 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Prefetch {
    None,
    First,
    Index,
    Whole,
}

impl Prefetch {
    pub const ALL: [Self; 4] = [Self::None, Self::First, Self::Index, Self::Whole];

    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Index => "index",
            Self::First => "first",
            Self::Whole => "whole",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReadAhead {
    None,
    Small,
    Large,
}

impl ReadAhead {
    pub const ALL: [Self; 3] = [Self::None, Self::Small, Self::Large];

    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Small => "small",
            Self::Large => "large",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Keep {
    No,
    Yes,
}

impl Keep {
    pub const ALL: [Self; 2] = [Self::No, Self::Yes];

    pub fn name(self) -> &'static str {
        match self {
            Self::No => "no",
            Self::Yes => "yes",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct AccessPlan {
    pub prefetch: Prefetch,
    pub read_ahead: ReadAhead,
    pub keep: Keep,
}

impl AccessPlan {
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
        (Prefetch::None, later) => later,
        (now, Prefetch::None) => now,
        (now, later) if now == later => now,
        (Prefetch::Whole, _) | (_, Prefetch::Whole) => Prefetch::Whole,
        // First and format-index regions need not overlap, so their representable union is whole.
        _ => Prefetch::Whole,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanSource {
    Remembered,
    Rule,
    Jev,
    Fixed,
    Chaos,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct Confidence {
    pub prefetch: Option<f64>,
    pub read_ahead: Option<f64>,
    pub keep: Option<f64>,
}

/// What a decider knows about one access.
#[derive(Clone, Debug)]
pub struct OpenContext {
    pub size: u64,
    pub extension: Option<String>,
    /// The opening process's name, when the adapter can report one.
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

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Setting<T> {
    pub value: T,
    pub confidence: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct JevAnswer {
    pub prefetch: Option<Setting<Prefetch>>,
    pub read_ahead: Option<Setting<ReadAhead>>,
    pub keep: Option<Setting<Keep>>,
}

impl JevAnswer {
    pub fn plan(self, fallback: AccessPlan, confident: f64) -> AccessPlan {
        AccessPlan::new(
            accepted(self.prefetch, confident).map_or(fallback.prefetch, |answer| answer.value),
            accepted(self.read_ahead, confident).map_or(fallback.read_ahead, |answer| answer.value),
            accepted(self.keep, confident).map_or(fallback.keep, |answer| answer.value),
        )
    }

    pub fn confidence(self) -> Confidence {
        Confidence {
            prefetch: self.prefetch.map(|answer| answer.confidence),
            read_ahead: self.read_ahead.map(|answer| answer.confidence),
            keep: self.keep.map(|answer| answer.confidence),
        }
    }

    pub fn any_confident(self, confident: f64) -> bool {
        [
            self.prefetch.map(|answer| answer.confidence),
            self.read_ahead.map(|answer| answer.confidence),
            self.keep.map(|answer| answer.confidence),
        ]
        .into_iter()
        .flatten()
        .any(|confidence| confidence >= confident)
    }
}

fn accepted<T>(answer: Option<Setting<T>>, confident: f64) -> Option<Setting<T>> {
    answer.filter(|answer| answer.confidence >= confident)
}

pub type Upgrade = oneshot::Receiver<JevAnswer>;

pub struct Decision {
    pub plan: AccessPlan,
    pub source: PlanSource,
    pub confidence: Option<Confidence>,
    pub upgrade: Option<Upgrade>,
}

#[derive(Clone)]
pub enum Decider {
    Fixed(AccessPlan),
    Rule,
    Chaos(SmallRng),
    Jev(Arc<Jev>, Arc<Stats>),
}

impl Decider {
    pub fn decide(&mut self, context: &OpenContext) -> Decision {
        let (plan, source) = match self {
            Self::Fixed(plan) => (*plan, PlanSource::Fixed),
            Self::Rule => (rule(context), PlanSource::Rule),
            Self::Chaos(rng) => (chaos(rng), PlanSource::Chaos),
            Self::Jev(jev, stats) => return jev.decide(context, stats),
        };
        Decision {
            plan,
            source,
            confidence: None,
            upgrade: None,
        }
    }
}

pub fn rule(context: &OpenContext) -> AccessPlan {
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

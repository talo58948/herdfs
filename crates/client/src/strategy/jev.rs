//! Jev asks three independent plan questions in one request. Accesses use remembered settings or
//! the rule immediately; a reply can only add work to an access that is still open (P1-P3).

use std::collections::hash_map::{Entry, HashMap};
use std::sync::{Arc, Mutex};
use std::{fs, path::Path, time::Duration};

use serde_json::{Value, json};
use tokio::sync::oneshot;

use super::{Decision, JevAnswer, Keep, OpenContext, PlanSource, Prefetch, ReadAhead, Setting, rule};
use crate::stats::{Stats, bump};
use crate::{Result, lock};

const MODEL: &str = "typesafe-ai/jev";
const TIMEOUT: Duration = Duration::from_secs(2);
const FIRST_THROUGHPUT: f64 = 10e6;

pub struct Jev {
    http: reqwest::Client,
    url: String,
    key: String,
    /// `None` while the one request for this phrased context is in flight.
    answers: Mutex<HashMap<String, Option<JevAnswer>>>,
}

impl Jev {
    /// R5's earlier measurements found answers at or above 0.3 reliable.
    pub const CONFIDENT: f64 = 0.3;

    pub fn new(key_file: &Path, url: &str) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder().timeout(TIMEOUT).build()?,
            url: url.to_owned(),
            key: fs::read_to_string(key_file)?.trim().to_owned(),
            answers: Mutex::default(),
        })
    }

    /// Asks all three named `choice` questions in one Gateway request.
    pub async fn ask(&self, context: &OpenContext) -> Result<JevAnswer> {
        let request = json!({
            "model": MODEL,
            "state": phrase(context),
            "questions": {
                "prefetch": {
                    "type": "choice",
                    "instructions": "What should be prefetched when this access starts?",
                    "criteria": {
                        "none": "Nothing beyond bytes the program reads.",
                        "index": "Only the format index regions computed by the client.",
                        "first": "The first small chunk window.",
                        "whole": "The whole file."
                    }
                },
                "read_ahead": {
                    "type": "choice",
                    "instructions": "How much should sequential reads read ahead?",
                    "criteria": {
                        "none": "No read-ahead.",
                        "small": "A small chunk window.",
                        "large": "A large chunk window."
                    }
                },
                "keep": {
                    "type": "choice",
                    "instructions": "Should chunks stay cached until this access ends?",
                    "criteria": {
                        "no": "Normal cache eviction applies.",
                        "yes": "Keep accessed chunks through the end of this access."
                    }
                }
            }
        });
        let reply: Value = self
            .http
            .post(&self.url)
            .bearer_auth(&self.key)
            .json(&request)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let answers = &reply["answers"];
        Ok(JevAnswer {
            prefetch: choice(&answers["prefetch"], &Prefetch::ALL, Prefetch::name),
            read_ahead: choice(&answers["read_ahead"], &ReadAhead::ALL, ReadAhead::name),
            keep: choice(&answers["keep"], &Keep::ALL, Keep::name),
        })
    }

    pub(super) fn decide(self: &Arc<Self>, open: &OpenContext, stats: &Arc<Stats>) -> Decision {
        let (key, fallback) = (phrase(open).to_string(), rule(open));
        match lock(&self.answers).entry(key.clone()) {
            Entry::Occupied(known) => {
                let Some(answer) = *known.get() else {
                    return decision(fallback, PlanSource::Rule, None, None);
                };
                let source = if answer.any_confident(Self::CONFIDENT) {
                    PlanSource::Remembered
                } else {
                    PlanSource::Rule
                };
                return decision(
                    answer.plan(fallback, Self::CONFIDENT),
                    source,
                    Some(answer.confidence()),
                    None,
                );
            }
            Entry::Vacant(asking) => {
                asking.insert(None);
            }
        }
        let (upgrade, upgraded) = oneshot::channel();
        let (jev, stats, open) = (self.clone(), stats.clone(), open.clone());
        tokio::spawn(async move {
            if let Some(answer) = jev.learn(key, &open, &stats).await {
                let _ = upgrade.send(answer);
            }
        });
        decision(fallback, PlanSource::Rule, None, Some(upgraded))
    }

    async fn learn(&self, key: String, context: &OpenContext, stats: &Stats) -> Option<JevAnswer> {
        let answer = match self.ask(context).await {
            Ok(answer) => answer,
            Err(err) => {
                tracing::warn!(%err, "asking Jev failed; the rule decides");
                bump(&stats.jev_errors, 3);
                lock(&self.answers).remove(&key);
                return None;
            }
        };
        for confidence in [
            answer.prefetch.map(|answer| answer.confidence),
            answer.read_ahead.map(|answer| answer.confidence),
            answer.keep.map(|answer| answer.confidence),
        ] {
            match confidence {
                Some(value) if value >= Self::CONFIDENT => bump(&stats.jev_answers, 1),
                Some(_) => bump(&stats.jev_unsure, 1),
                None => bump(&stats.jev_errors, 1),
            }
        }
        lock(&self.answers).insert(key, Some(answer));
        Some(answer)
    }
}

fn decision(
    plan: super::AccessPlan,
    source: PlanSource,
    confidence: Option<super::Confidence>,
    upgrade: Option<super::Upgrade>,
) -> Decision {
    Decision {
        plan,
        source,
        confidence,
        upgrade,
    }
}

fn choice<T: Copy>(answer: &Value, values: &[T], name: impl Fn(T) -> &'static str) -> Option<Setting<T>> {
    let chosen = answer["choice"].as_str()?;
    let value = values.iter().copied().find(|value| name(*value) == chosen)?;
    let confidence = answer["confidence"].as_f64()?;
    confidence.is_finite().then_some(Setting { value, confidence })
}

fn phrase(context: &OpenContext) -> Value {
    let cached = match (context.coverage * 100.0) as u8 {
        0 => "none of it",
        1..50 => "some of it",
        50..99 => "most of it",
        _ => "all of it",
    };
    let opened = match context.opens {
        0 => "for the first time",
        1 => "once before",
        _ => "several times in the last few minutes",
    };
    let size = context.size as f64;
    let download = size / context.throughput.unwrap_or(FIRST_THROUGHPUT);
    json!({
        "program": context.comm,
        "opened by this program": opened,
        "file extension": context.extension.as_deref().unwrap_or("none"),
        "file size": magnitude(size / 1e6, "megabyte"),
        "already cached": cached,
        "network round trip": magnitude(context.round_trip * 1e3, "millisecond"),
        "time to download the whole file": magnitude(download, "second"),
    })
}

fn magnitude(count: f64, unit: &str) -> String {
    match count.log10().floor() as i32 {
        ..0 => format!("under a {unit}"),
        0 => format!("a few {unit}s"),
        1 => format!("tens of {unit}s"),
        2 => format!("hundreds of {unit}s"),
        _ => format!("thousands of {unit}s"),
    }
}

#[cfg(test)]
mod tests;

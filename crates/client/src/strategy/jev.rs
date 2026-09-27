//! Jev, the AI Gateway's evaluation model, as a decider. `open` never waits for it (P1): it
//! answers from what Jev said about similar opens, and asks Jev in the background on a miss.

use std::collections::hash_map::{Entry, HashMap};
use std::sync::{Arc, Mutex};
use std::{fs, path::Path, time::Duration};

use serde_json::{Value, json};
use tokio::sync::oneshot;

use super::{Decider, Decision, OpenContext, Strategy};
use crate::stats::{Stats, bump};
use crate::{error::Error, lock};

const MODEL: &str = "typesafe-ai/jev";
/// Jev answers warm in about 350 ms; past this the rule's choice stands (P2).
const TIMEOUT: Duration = Duration::from_secs(2);
/// Download throughput assumed until one is measured, in bytes per second.
const FIRST_THROUGHPUT: f64 = 10e6;

pub struct Jev {
    http: reqwest::Client,
    url: String,
    key: String,
    /// Answers by phrased context: `None` while Jev is being asked, or after it was unsure.
    answers: Mutex<HashMap<String, Option<Strategy>>>,
}

impl Jev {
    /// Answers below this confidence leave the rule's choice (P2). R5's mean regret of Jev's top
    /// choice by confidence: 0.2-0.3 n=1 1.15; 0.3-0.4 n=2 1.00; 0.4-0.6 n=6 1.00; 0.6+ n=15 1.00.
    pub const CONFIDENT: f64 = 0.3;

    /// Jev at `url`, with the AI Gateway key in `key_file`.
    pub fn new(key_file: &Path, url: &str) -> Result<Self, Error> {
        Ok(Self {
            http: reqwest::Client::builder().timeout(TIMEOUT).build()?,
            url: url.to_owned(),
            key: fs::read_to_string(key_file)?.trim().to_owned(),
            answers: Mutex::default(),
        })
    }

    /// Jev's choice for `context`, with its confidence.
    pub async fn ask(&self, context: &OpenContext) -> Result<(Strategy, f64), Error> {
        let question = json!({
            "model": MODEL,
            "state": phrase(context),
            "questions": {"strategy": {
                "type": "choice",
                "instructions": "How should this file be read so that the program finishes soonest?",
                "criteria": {
                    "stream": "Download only the parts the program reads, as it reads them. \
                        Best when it reads a small part of a large file.",
                    "fetch": "Download the whole file in the background right away. \
                        Best when it reads most of the file once.",
                    "pin": "Download the whole file and keep it cached. \
                        Best when it reads the file repeatedly or must keep it available.",
                },
            }},
        });
        let request = self.http.post(&self.url).bearer_auth(&self.key).json(&question);
        let reply: Value = request.send().await?.error_for_status()?.json().await?;
        let answer = &reply["answers"]["strategy"];
        let choice = answer["choice"].as_str();
        let strategy = Strategy::ALL.into_iter().find(|s| Some(s.name()) == choice);
        let found = strategy.zip(answer["confidence"].as_f64());
        found.ok_or_else(|| Error::Protocol(format!("Jev answered {answer}")))
    }

    /// The remembered answer at once; on a miss, the rule's choice while Jev is asked, once per context.
    pub(super) fn decide(self: &Arc<Self>, open: &OpenContext, stats: &Arc<Stats>) -> Decision {
        let (key, rule) = (phrase(open).to_string(), Decider::Rule.decide(open).0);
        match lock(&self.answers).entry(key.clone()) {
            Entry::Occupied(known) => return (known.get().unwrap_or(rule), None),
            Entry::Vacant(asking) => asking.insert(None),
        };
        let (upgrade, upgraded) = oneshot::channel();
        let (jev, stats, open) = (self.clone(), stats.clone(), open.clone());
        tokio::spawn(async move {
            if let Some(answer) = jev.learn(key, &open, &stats).await {
                // Only an open that began with Stream listens.
                let _ = upgrade.send(answer);
            }
        });
        (rule, (rule == Strategy::Stream).then_some(upgraded))
    }

    /// Asks Jev about `context` and remembers the answer under `key`, unless the ask failed.
    async fn learn(&self, key: String, context: &OpenContext, stats: &Stats) -> Option<Strategy> {
        let (answer, counter) = match self.ask(context).await {
            Ok((choice, confidence)) if confidence >= Self::CONFIDENT => (Some(choice), &stats.jev_answers),
            Ok(_) => (None, &stats.jev_unsure),
            Err(err) => {
                tracing::warn!(%err, "asking Jev failed; the rule decides");
                bump(&stats.jev_errors, 1);
                lock(&self.answers).remove(&key);
                return None;
            }
        };
        bump(counter, 1);
        lock(&self.answers).insert(key, answer);
        answer
    }
}

/// The open in words, with the arithmetic done here, since Jev is weak at it.
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

/// `count` `unit`s in words, to an order of magnitude.
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
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

    use axum::http::{HeaderMap, StatusCode};
    use axum::{Json, Router};

    use super::*;

    const BIG: u64 = 512 << 20;

    fn context(size: u64) -> OpenContext {
        OpenContext {
            size,
            extension: Some("mp4".into()),
            comm: "ffprobe".into(),
            coverage: 0.0,
            throughput: Some(12.5e6),
            round_trip: 0.09,
            opens: 0,
        }
    }

    /// Jev on a local endpoint that gives `answer` after `delay`, and a count of its questions.
    async fn fake_jev(answer: Value, delay: Duration) -> (Arc<Jev>, Arc<AtomicUsize>) {
        let asked = Arc::new(AtomicUsize::new(0));
        let counter = asked.clone();
        let reply = move |headers: HeaderMap| {
            counter.fetch_add(1, Relaxed);
            let answer = json!({"answers": {"strategy": answer}});
            async move {
                tokio::time::sleep(delay).await;
                let keyed = headers["authorization"] == "Bearer test-key";
                keyed.then_some(Json(answer)).ok_or(StatusCode::UNAUTHORIZED)
            }
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/evaluate", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, Router::new().fallback(reply)).await });
        let key = tempfile::NamedTempFile::new().unwrap();
        fs::write(key.path(), "test-key\n").unwrap();
        (Arc::new(Jev::new(key.path(), &url).unwrap()), asked)
    }

    // contract: P1
    #[test]
    fn contexts_are_sent_as_words() {
        let expected = json!({
            "program": "ffprobe",
            "opened by this program": "for the first time",
            "file extension": "mp4",
            "file size": "hundreds of megabytes",
            "already cached": "none of it",
            "network round trip": "tens of milliseconds",
            "time to download the whole file": "tens of seconds",
        });
        assert_eq!(phrase(&context(BIG)), expected);
        let cached = OpenContext {
            extension: None,
            coverage: 1.0,
            throughput: None,
            round_trip: 0.0004,
            opens: 2,
            ..context(4 << 20)
        };
        let expected = json!({
            "program": "ffprobe",
            "opened by this program": "several times in the last few minutes",
            "file extension": "none",
            "file size": "a few megabytes",
            "already cached": "all of it",
            "network round trip": "under a millisecond",
            "time to download the whole file": "under a second",
        });
        assert_eq!(phrase(&cached), expected);
    }

    // contract: P1
    #[tokio::test]
    async fn a_miss_gets_the_rules_choice_and_a_hit_gets_jevs_without_asking() {
        let (jev, asked) = fake_jev(json!({"choice": "pin", "confidence": 0.9}), Duration::ZERO).await;
        let stats: Arc<Stats> = Arc::default();
        let (now, upgrade) = jev.decide(&context(BIG), &stats);
        assert_eq!(now, Strategy::Stream, "the rule's choice, without waiting");
        assert_eq!(
            jev.decide(&context(BIG), &stats).0,
            Strategy::Stream,
            "still asking"
        );
        assert_eq!(upgrade.unwrap().await.unwrap(), Strategy::Pin);
        assert!(matches!(jev.decide(&context(BIG), &stats), (Strategy::Pin, None)));
        assert_eq!((asked.load(Relaxed), stats.jev_answers.load(Relaxed)), (1, 1));
    }

    // contract: P2
    #[tokio::test]
    async fn an_unsure_answer_leaves_the_rules_choice_and_is_not_asked_again() {
        let (jev, asked) = fake_jev(json!({"choice": "fetch", "confidence": 0.2}), Duration::ZERO).await;
        let stats: Arc<Stats> = Arc::default();
        let (now, upgrade) = jev.decide(&context(BIG), &stats);
        assert!(upgrade.unwrap().await.is_err(), "no upgrade");
        assert_eq!(now, Strategy::Stream);
        assert!(matches!(
            jev.decide(&context(BIG), &stats),
            (Strategy::Stream, None)
        ));
        assert_eq!((asked.load(Relaxed), stats.jev_unsure.load(Relaxed)), (1, 1));
    }

    // contract: P2
    #[tokio::test]
    async fn a_slow_jev_times_out_and_is_asked_again_on_a_later_open() {
        let slow = TIMEOUT + Duration::from_secs(1);
        let (jev, _) = fake_jev(json!({"choice": "fetch", "confidence": 1.0}), slow).await;
        let stats: Arc<Stats> = Arc::default();
        let (now, upgrade) = jev.decide(&context(BIG), &stats);
        assert!(upgrade.unwrap().await.is_err(), "no upgrade");
        assert_eq!((now, stats.jev_errors.load(Relaxed)), (Strategy::Stream, 1));
        assert!(jev.decide(&context(BIG), &stats).1.is_some(), "asks again");
    }
}

use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use axum::http::{HeaderMap, StatusCode};
use axum::{Json, Router};

use super::*;
use crate::strategy::{AccessPlan, Decider, Keep, PlanSource, Prefetch, ReadAhead};

const BIG: u64 = 512 << 20;

fn context() -> OpenContext {
    OpenContext {
        size: BIG,
        extension: Some("mp4".into()),
        comm: "ffprobe".into(),
        coverage: 0.0,
        throughput: Some(12.5e6),
        round_trip: 0.09,
        opens: 0,
    }
}

fn setting(choice: &str, confidence: f64) -> Value {
    json!({"type": "choice", "choice": choice, "confidence": confidence})
}

fn answers(prefetch: &Value, read_ahead: &Value, keep: &Value) -> Value {
    json!({"answers": {"prefetch": prefetch, "read_ahead": read_ahead, "keep": keep}})
}

async fn fake_jev(answer: Value, delay: Duration) -> (Arc<Jev>, Arc<AtomicUsize>) {
    let asked = Arc::new(AtomicUsize::new(0));
    let count = asked.clone();
    let reply = move |headers: HeaderMap, Json(body): Json<Value>| {
        count.fetch_add(1, Relaxed);
        let answer = answer.clone();
        async move {
            tokio::time::sleep(delay).await;
            let keyed = headers["authorization"] == "Bearer test-key";
            let three = body["questions"]
                .as_object()
                .is_some_and(|questions| questions.len() == 3);
            (keyed && three)
                .then_some(Json(answer))
                .ok_or(StatusCode::BAD_REQUEST)
        }
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/evaluate", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, Router::new().fallback(reply)).await });
    let key = tempfile::NamedTempFile::new().unwrap();
    fs::write(key.path(), "test-key\n").unwrap();
    (Arc::new(Jev::new(key.path(), &url).unwrap()), asked)
}

// contract: P1, P2, P3
#[tokio::test]
async fn a_miss_uses_the_rule_then_remembers_each_confident_setting() {
    let reply = answers(
        &setting("index", 0.9),
        &setting("large", 0.8),
        &setting("yes", 0.7),
    );
    let (jev, asked) = fake_jev(reply, Duration::ZERO).await;
    let stats: Arc<Stats> = Arc::default();
    let now = jev.decide(&context(), &stats);
    assert_eq!(now.decided.plan, AccessPlan::STREAM);
    let answer = now.upgrade.unwrap().await.unwrap();
    assert_eq!(answer.plan(AccessPlan::STREAM).name(), "index/large/yes");
    let remembered = jev.decide(&context(), &stats);
    assert_eq!(remembered.decided.plan.name(), "index/large/yes");
    assert_eq!(remembered.decided.source, PlanSource::Remembered);
    assert_eq!((asked.load(Relaxed), stats.jev_answers.load(Relaxed)), (1, 3));
}

// contract: P2, P3
#[tokio::test]
async fn uncertainty_and_one_malformed_answer_fall_back_per_setting() {
    let reply = answers(
        &setting("index", 0.9),
        &setting("large", 0.1),
        &json!({"choice": "yes"}),
    );
    let (jev, _) = fake_jev(reply, Duration::ZERO).await;
    let stats: Arc<Stats> = Arc::default();
    let answer = jev.ask(&context()).await.unwrap();
    assert_eq!(answer.plan(AccessPlan::STREAM).name(), "index/none/no");
    assert_eq!(answer.confidences()[1], Some(0.1));
    assert_eq!(answer.confidences()[2], None);

    let decision = jev.decide(&context(), &stats);
    let _ = decision.upgrade.unwrap().await.unwrap();
    assert_eq!(
        (
            stats.jev_answers.load(Relaxed),
            stats.jev_unsure.load(Relaxed),
            stats.jev_errors.load(Relaxed),
        ),
        (1, 1, 1)
    );
}

// contract: P1
#[test]
fn late_answers_only_add_settings() {
    let first = AccessPlan::new(Prefetch::Whole, ReadAhead::None, Keep::No);
    let late = AccessPlan::new(Prefetch::None, ReadAhead::Large, Keep::Yes);
    assert_eq!(first.merged(late).name(), "whole/large/yes");
    let first = AccessPlan::new(Prefetch::First, ReadAhead::None, Keep::No);
    let index = AccessPlan::new(Prefetch::Index, ReadAhead::None, Keep::No);
    assert_eq!(first.merged(index).prefetch, Prefetch::Whole);
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
    assert_eq!(phrase(&context()), expected);
    assert_eq!(Decider::Rule.decide(&context()).decided.plan, AccessPlan::STREAM);
}

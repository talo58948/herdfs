//! Fake Jev for F8: one response carries the three independent plan answers.

use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::Result;
use axum::extract::State;
use axum::http::StatusCode;
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::runtime::{self, Runtime};

use crate::Lab;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct JevAnswers {
    pub prefetch: (&'static str, f64),
    pub read_ahead: (&'static str, f64),
    pub keep: (&'static str, f64),
}

/// How the fake Jev answers one three-question request.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum JevMode {
    Answers(JevAnswers),
    /// Omits the confidence of this one named answer.
    Malformed(&'static str),
    Slow(Duration),
    Down,
}

#[derive(Clone)]
struct Shared {
    mode: Arc<Mutex<JevMode>>,
    requests: Arc<AtomicUsize>,
}

pub struct FakeJev {
    url: String,
    shared: Shared,
    _runtime: Runtime,
}

impl FakeJev {
    pub(crate) fn start() -> Result<Self> {
        let runtime = runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()?;
        let listener = runtime.block_on(TcpListener::bind("127.0.0.1:0"))?;
        let url = format!("http://{}/v1/evaluate", listener.local_addr()?);
        let shared = Shared {
            mode: Arc::new(Mutex::new(JevMode::Down)),
            requests: Arc::default(),
        };
        let app = Router::new().fallback(answer).with_state(shared.clone());
        runtime.spawn(async { axum::serve(listener, app).await });
        Ok(Self {
            url,
            shared,
            _runtime: runtime,
        })
    }

    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    pub fn set(&self, mode: JevMode) {
        *self.shared.mode.lock().unwrap_or_else(PoisonError::into_inner) = mode;
    }

    pub fn requests(&self) -> usize {
        self.shared.requests.load(SeqCst)
    }
}

impl Lab {
    pub fn jev(&self, name: &str) -> &FakeJev {
        let jev = self.client(name).jev.as_ref();
        jev.unwrap_or_else(|| panic!("client {name} does not decide with Jev"))
    }
}

async fn answer(State(shared): State<Shared>, Json(request): Json<Value>) -> Result<Json<Value>, StatusCode> {
    let questions = request["questions"].as_object().ok_or(StatusCode::BAD_REQUEST)?;
    if !["prefetch", "read_ahead", "keep"].iter().all(|name| {
        questions
            .get(*name)
            .is_some_and(|question| question["type"] == "choice")
    }) {
        return Err(StatusCode::BAD_REQUEST);
    }
    shared.requests.fetch_add(1, SeqCst);
    let mode = *shared.mode.lock().unwrap_or_else(PoisonError::into_inner);
    let answers = match mode {
        JevMode::Answers(answers) => reply(answers),
        JevMode::Malformed(name) => {
            let mut reply = reply(JevAnswers {
                prefetch: ("index", 0.9),
                read_ahead: ("large", 0.9),
                keep: ("yes", 0.9),
            });
            reply["answers"][name]
                .as_object_mut()
                .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?
                .remove("confidence");
            reply
        }
        JevMode::Slow(delay) => {
            tokio::time::sleep(delay).await;
            reply(JevAnswers {
                prefetch: ("whole", 1.0),
                read_ahead: ("none", 1.0),
                keep: ("no", 1.0),
            })
        }
        JevMode::Down => return Err(StatusCode::SERVICE_UNAVAILABLE),
    };
    Ok(Json(answers))
}

fn reply(answers: JevAnswers) -> Value {
    let answer = |(choice, confidence)| json!({"type": "choice", "choice": choice, "confidence": confidence});
    json!({"answers": {
        "prefetch": answer(answers.prefetch),
        "read_ahead": answer(answers.read_ahead),
        "keep": answer(answers.keep),
    }})
}

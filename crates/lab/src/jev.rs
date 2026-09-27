//! A fake Jev for each client that decides with Jev (F8): it answers as the test sets it, too
//! slowly, or not at all, and counts the questions it gets.

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

/// How the fake Jev answers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum JevMode {
    /// Answers this choice with this confidence.
    Answer(&'static str, f64),
    /// Answers only after this long, past the client's timeout.
    Slow(Duration),
    /// Fails every question with 503.
    Down,
}

#[derive(Clone)]
struct Shared {
    mode: Arc<Mutex<JevMode>>,
    questions: Arc<AtomicUsize>,
}

pub struct FakeJev {
    url: String,
    shared: Shared,
    /// Serves the endpoint until the fake is dropped.
    _runtime: Runtime,
}

impl FakeJev {
    /// Starts a fake that is down until a test sets its mode.
    pub(crate) fn start() -> Result<Self> {
        let runtime = runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()?;
        let listener = runtime.block_on(TcpListener::bind("127.0.0.1:0"))?;
        let url = format!("http://{}/v1/evaluate", listener.local_addr()?);
        let shared = Shared {
            mode: Arc::new(Mutex::new(JevMode::Down)),
            questions: Arc::default(),
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

    /// The questions asked so far.
    pub fn questions(&self) -> usize {
        self.shared.questions.load(SeqCst)
    }
}

impl Lab {
    /// The fake Jev that client `name` asks; the client must mount with `Strategy::Jev`.
    pub fn jev(&self, name: &str) -> &FakeJev {
        let jev = self.client(name).jev.as_ref();
        jev.unwrap_or_else(|| panic!("client {name} does not decide with Jev"))
    }
}

async fn answer(State(shared): State<Shared>) -> Result<Json<Value>, StatusCode> {
    shared.questions.fetch_add(1, SeqCst);
    let mode = *shared.mode.lock().unwrap_or_else(PoisonError::into_inner);
    let (choice, confidence) = match mode {
        JevMode::Answer(choice, confidence) => (choice, confidence),
        JevMode::Slow(delay) => {
            tokio::time::sleep(delay).await;
            ("fetch", 1.0)
        }
        JevMode::Down => return Err(StatusCode::SERVICE_UNAVAILABLE),
    };
    let answer = json!({"type": "choice", "choice": choice, "confidence": confidence});
    Ok(Json(json!({"answers": {"strategy": answer}})))
}

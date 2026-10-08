//! The offline window, which spares calls the wait of a silent outage.

use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use crate::{Error, Result, lock};

const FIRST_WINDOW: Duration = Duration::from_secs(2);
const LAST_WINDOW: Duration = Duration::from_secs(30);

/// Whether the authority can be reached. A silent outage makes every call wait out a timeout,
/// so after one call times out, calls fail at once for a window of 2 s, doubling to 30 s while
/// it lasts. A refused connection fails fast already and opens no window. Whatever the cache
/// holds is still served (C12); a call or probe after the window ends finds out whether the
/// authority answers again. Holds when the window ends, and how long it was.
pub struct Link {
    window: Mutex<Option<(Instant, Duration)>>,
    connected: AtomicBool,
}

impl Default for Link {
    fn default() -> Self {
        Self {
            window: Mutex::default(),
            connected: AtomicBool::new(true),
        }
    }
}

impl Link {
    /// Fails at once while the authority counts as unreachable.
    pub fn check(&self) -> Result<()> {
        match *lock(&self.window) {
            Some((until, _)) if Instant::now() < until => Err(Error::Offline),
            _ => Ok(()),
        }
    }

    /// Opens or lengthens the window after a call timed out, and closes it after any answer.
    pub fn record<T>(&self, result: &Result<T>) {
        let mut state = lock(&self.window);
        let window = state.map_or(FIRST_WINDOW, |(_, window)| (window * 2).min(LAST_WINDOW));
        let grown = Some((Instant::now() + window, window));
        *state = match result {
            Err(Error::TimedOut) => grown,
            Err(Error::Network(error)) if error.is_timeout() => grown,
            Err(Error::Network(_)) => return,
            _ => None,
        };
    }

    /// How long the window has left, while the authority counts as unreachable.
    pub fn remaining(&self) -> Option<Duration> {
        lock(&self.window).map(|(until, _)| until.saturating_duration_since(Instant::now()))
    }

    pub fn record_connection<T>(&self, result: &Result<T>) {
        let connected = result.is_ok() || matches!(result, Err(Error::Api(_)));
        self.connected.store(connected, Ordering::Relaxed);
    }

    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::net::TcpListener;

    /// A call to a listener that never answers, or to a port nobody listens on.
    async fn failed(listening: bool) -> Result<(), Error> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        if !listening {
            drop(listener);
        }
        let call = reqwest::Client::new()
            .get(url)
            .timeout(Duration::from_millis(100));
        Err(Error::Network(call.send().await.unwrap_err()))
    }

    // contract: C12
    #[tokio::test]
    async fn a_timed_out_call_opens_a_growing_window_that_an_answer_closes() {
        let (link, silent, refused) = (Link::default(), failed(true).await, failed(false).await);
        link.record(&refused);
        assert!(link.check().is_ok(), "a refused connection opens no window");
        link.record(&silent);
        assert!(matches!(link.check(), Err(Error::Offline)));
        assert!(link.remaining().is_some_and(|left| left <= FIRST_WINDOW));
        link.record(&refused);
        assert!(link.remaining().is_some_and(|left| left <= FIRST_WINDOW));
        link.record(&silent);
        assert!(link.remaining().is_some_and(|left| left > FIRST_WINDOW));
        link.record(&Ok(()));
        assert!(link.check().is_ok() && link.remaining().is_none());
    }
}

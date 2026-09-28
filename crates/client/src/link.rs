//! Whether the authority can be reached. A silent outage makes every call wait out a timeout,
//! so after one call times out, calls fail at once for a window of 2 s, doubling to 30 s while
//! it lasts. A refused connection fails fast already and opens no window. Whatever the cache
//! holds is still served (C12); a call or probe after the window ends finds out whether the
//! authority answers again.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::{error::Error, lock};

const FIRST_WINDOW: Duration = Duration::from_secs(2);
const LAST_WINDOW: Duration = Duration::from_secs(30);

/// While the authority counts as unreachable: when the window ends, and how long it was.
#[derive(Default)]
pub struct Link(Mutex<Option<(Instant, Duration)>>);

impl Link {
    /// Fails at once while the authority counts as unreachable.
    pub fn check(&self) -> Result<(), Error> {
        match *lock(&self.0) {
            Some((until, _)) if Instant::now() < until => Err(Error::Offline),
            _ => Ok(()),
        }
    }

    /// Opens or lengthens the window after a call timed out, and closes it after any answer.
    pub fn record<T>(&self, result: &Result<T, Error>) {
        let mut state = lock(&self.0);
        *state = match result {
            Err(Error::Network(error)) if error.is_timeout() => {
                let window = state.map_or(FIRST_WINDOW, |(_, window)| (window * 2).min(LAST_WINDOW));
                Some((Instant::now() + window, window))
            }
            Err(Error::Network(_)) => return,
            _ => None,
        };
    }

    /// How long the window has left, while the authority counts as unreachable.
    pub fn remaining(&self) -> Option<Duration> {
        lock(&self.0).map(|(until, _)| until.saturating_duration_since(Instant::now()))
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

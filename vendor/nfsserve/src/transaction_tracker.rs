use std::collections::{hash_map, HashMap};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// `TransactionTracker` tracks the state of transactions to detect retransmissions.
pub struct TransactionTracker {
    retention_period: Duration,
    transactions: Mutex<HashMap<(u32, RequestChecksum, IpAddr), TransactionState>>,
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
pub(crate) struct RequestChecksum(pub(crate) u64);

impl TransactionTracker {
    pub fn new(retention_period: Duration) -> Self {
        Self {
            retention_period,
            transactions: Mutex::new(HashMap::new()),
        }
    }

    /// Checks if the transaction is a retransmission.
    /// If it's a new transaction, it is marked as `InProgress`.
    ///
    pub async fn begin(&self, xid: u32, checksum: RequestChecksum, client_addr: IpAddr) -> Begin {
        let key = (xid, checksum, client_addr);
        loop {
            let (notified, wait) = {
                let mut transactions = self
                    .transactions
                    .lock()
                    .expect("unable to unlock transactions mutex");
                let finished = match transactions.entry(key) {
                    hash_map::Entry::Vacant(entry) => {
                        entry.insert(TransactionState::InProgress {
                            started: Instant::now(),
                            finished: Arc::new(Notify::new()),
                        });
                        return Begin::New;
                    }
                    hash_map::Entry::Occupied(entry) => match entry.get() {
                        TransactionState::InProgress { started, finished } => {
                            let Some(wait) = self.retention_period.checked_sub(started.elapsed()) else {
                                return Begin::Drop;
                            };
                            (finished.clone(), wait)
                        }
                        TransactionState::Completed { reply, .. } => {
                            return Begin::Replay(reply.clone());
                        }
                    },
                };
                (finished.0.notified_owned(), finished.1)
            };
            if tokio::time::timeout(wait, notified).await.is_err() {
                return Begin::Drop;
            }
        }
    }

    /// Marks the transaction as processed.
    pub fn mark_processed(&self, xid: u32, checksum: RequestChecksum, client_addr: IpAddr, reply: Vec<u8>) {
        self.finish(xid, checksum, client_addr, Some(reply));
    }

    pub fn mark_failed(&self, xid: u32, checksum: RequestChecksum, client_addr: IpAddr) {
        self.finish(xid, checksum, client_addr, None);
    }

    fn finish(&self, xid: u32, checksum: RequestChecksum, client_addr: IpAddr, reply: Option<Vec<u8>>) {
        let key = (xid, checksum, client_addr);
        let mut transactions = self
            .transactions
            .lock()
            .expect("unable to unlock transactions mutex");
        if let Some(state) = transactions.get_mut(&key) {
            let finished = match state {
                TransactionState::InProgress { finished, .. } => finished.clone(),
                TransactionState::Completed { .. } => return,
            };
            if let Some(reply) = reply {
                *state = TransactionState::Completed {
                    finished: Instant::now(),
                    reply,
                };
            } else {
                transactions.remove(&key);
            }
            finished.notify_waiters();
        }
    }

    pub fn sweep(&self) {
        let cutoff = Instant::now() - self.retention_period;
        self.transactions
            .lock()
            .expect("unable to unlock transactions mutex")
            .retain(|_, state| match state {
                TransactionState::InProgress { .. } => true,
                TransactionState::Completed { finished, .. } => *finished >= cutoff,
            });
    }
}

pub enum Begin {
    New,
    Replay(Vec<u8>),
    Drop,
}

pub enum TransactionState {
    InProgress { started: Instant, finished: Arc<Notify> },
    Completed { finished: Instant, reply: Vec<u8> },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> IpAddr {
        "127.0.0.1".parse().unwrap()
    }

    fn request() -> RequestChecksum {
        RequestChecksum(17)
    }

    // contract: M1
    #[tokio::test]
    async fn duplicate_xid_replays_the_first_encoded_reply() {
        let tracker = TransactionTracker::new(Duration::from_secs(60));
        assert!(matches!(tracker.begin(7, request(), client()).await, Begin::New));
        tracker.mark_processed(7, request(), client(), vec![1, 2, 3]);
        assert!(matches!(
            tracker.begin(7, request(), client()).await,
            Begin::Replay(reply) if reply == vec![1, 2, 3]
        ));
    }

    // contract: M1
    #[tokio::test]
    async fn same_xid_with_different_request_is_new() {
        let tracker = TransactionTracker::new(Duration::from_secs(60));
        assert!(matches!(tracker.begin(8, request(), client()).await, Begin::New));
        tracker.mark_processed(8, request(), client(), vec![1]);
        assert!(matches!(
            tracker.begin(8, RequestChecksum(18), client()).await,
            Begin::New
        ));
    }

    // contract: M1
    #[tokio::test]
    async fn duplicate_in_progress_waits_for_the_reply() {
        let tracker = Arc::new(TransactionTracker::new(Duration::from_secs(60)));
        assert!(matches!(tracker.begin(9, request(), client()).await, Begin::New));
        let duplicate = tokio::spawn({
            let tracker = tracker.clone();
            async move { tracker.begin(9, request(), client()).await }
        });
        tokio::task::yield_now().await;
        assert!(!duplicate.is_finished());
        tracker.mark_processed(9, request(), client(), vec![4, 5]);
        assert!(matches!(
            duplicate.await.unwrap(),
            Begin::Replay(reply) if reply == vec![4, 5]
        ));
    }

    // contract: M1
    #[tokio::test]
    async fn an_old_in_progress_request_is_not_run_again() {
        let tracker = TransactionTracker::new(Duration::from_millis(1));
        assert!(matches!(tracker.begin(11, request(), client()).await, Begin::New));
        tokio::time::sleep(Duration::from_millis(2)).await;
        assert!(matches!(tracker.begin(11, request(), client()).await, Begin::Drop));
    }

    // contract: M1
    #[tokio::test]
    async fn a_failed_request_is_not_cached() {
        let tracker = TransactionTracker::new(Duration::from_secs(60));
        assert!(matches!(tracker.begin(13, request(), client()).await, Begin::New));
        tracker.mark_failed(13, request(), client());
        assert!(matches!(tracker.begin(13, request(), client()).await, Begin::New));
    }
}

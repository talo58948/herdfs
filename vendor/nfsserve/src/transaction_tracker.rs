use std::collections::{hash_map, HashMap};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// `TransactionTracker` tracks the state of transactions to detect retransmissions.
pub struct TransactionTracker {
    retention_period: Duration,
    transactions: Mutex<HashMap<(u32, String), TransactionState>>,
}

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
    /// Returns `true` if the transaction is a retransmission, `false` otherwise.
    pub async fn begin(&self, xid: u32, client_addr: &str) -> Option<Vec<u8>> {
        let key = (xid, client_addr.to_string());
        loop {
            let notified = {
                let mut transactions = self
                    .transactions
                    .lock()
                    .expect("unable to unlock transactions mutex");
                housekeeping(&mut transactions, self.retention_period);
                let finished = match transactions.entry(key.clone()) {
                    hash_map::Entry::Vacant(entry) => {
                        entry.insert(TransactionState::InProgress {
                            started: Instant::now(),
                            finished: Arc::new(Notify::new()),
                        });
                        return None;
                    }
                    hash_map::Entry::Occupied(entry) => match entry.get() {
                        TransactionState::InProgress { finished, .. } => finished.clone(),
                        TransactionState::Completed { reply, .. } => return Some(reply.clone()),
                    },
                };
                finished.notified_owned()
            };
            notified.await;
        }
    }

    /// Marks the transaction as processed.
    pub fn mark_processed(&self, xid: u32, client_addr: &str, reply: Vec<u8>) {
        let key = (xid, client_addr.to_string());
        let mut transactions = self
            .transactions
            .lock()
            .expect("unable to unlock transactions mutex");
        if let Some(state) = transactions.get_mut(&key) {
            let finished = match state {
                TransactionState::InProgress { finished, .. } => finished.clone(),
                TransactionState::Completed { .. } => return,
            };
            *state = TransactionState::Completed {
                finished: Instant::now(),
                reply,
            };
            finished.notify_waiters();
        }
    }
}

fn housekeeping(transactions: &mut HashMap<(u32, String), TransactionState>, max_age: Duration) {
    let cutoff = Instant::now() - max_age;
    transactions.retain(|_, v| match v {
        TransactionState::InProgress { started, finished } if *started < cutoff => {
            finished.notify_waiters();
            false
        }
        TransactionState::InProgress { .. } => true,
        TransactionState::Completed { finished, .. } => *finished >= cutoff,
    });
}

pub enum TransactionState {
    InProgress { started: Instant, finished: Arc<Notify> },
    Completed { finished: Instant, reply: Vec<u8> },
}

#[cfg(test)]
mod tests {
    use super::*;

    // contract: M1
    #[tokio::test]
    async fn duplicate_xid_replays_the_first_encoded_reply() {
        let tracker = TransactionTracker::new(Duration::from_secs(60));
        assert_eq!(tracker.begin(7, "client").await, None);
        tracker.mark_processed(7, "client", vec![1, 2, 3]);
        assert_eq!(tracker.begin(7, "client").await, Some(vec![1, 2, 3]));
    }

    // contract: M1
    #[tokio::test]
    async fn duplicate_in_progress_waits_for_the_reply() {
        let tracker = Arc::new(TransactionTracker::new(Duration::from_secs(60)));
        assert_eq!(tracker.begin(9, "client").await, None);
        let duplicate = tokio::spawn({
            let tracker = tracker.clone();
            async move { tracker.begin(9, "client").await }
        });
        tokio::task::yield_now().await;
        assert!(!duplicate.is_finished());
        tracker.mark_processed(9, "client", vec![4, 5]);
        assert_eq!(duplicate.await.unwrap(), Some(vec![4, 5]));
    }
}

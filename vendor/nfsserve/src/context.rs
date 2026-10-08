use std::fmt;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

use tokio::sync::{mpsc, Notify};

use crate::transaction_tracker::TransactionTracker;
use crate::vfs::NFSFileSystem;

#[derive(Clone)]
pub struct RPCContext {
    pub local_port: u16,
    pub client_addr: String,
    pub auth: crate::rpc::auth_unix,
    pub vfs: Arc<dyn NFSFileSystem + Send + Sync>,
    pub mount_signal: Option<mpsc::Sender<bool>>,
    pub export_name: Arc<String>,
    pub transaction_tracker: Arc<TransactionTracker>,
    pub arrivals: Arc<AtomicU64>,
    pub arrival: u64,
    pub sequencer: Arc<ArrivalSequencer>,
}

impl RPCContext {
    pub(crate) fn arrived(&mut self) {
        self.arrival = self.arrivals.fetch_add(1, Ordering::Relaxed) + 1;
    }

    pub(crate) async fn sequence(&self, ordered: bool) -> Option<ArrivalTurn> {
        loop {
            let notified = {
                let mut next = self.sequencer.next.lock().expect("unable to lock RPC sequencer");
                if *next == self.arrival {
                    if ordered {
                        return Some(ArrivalTurn(self.sequencer.clone()));
                    }
                    *next += 1;
                    self.sequencer.changed.notify_waiters();
                    return None;
                }
                self.sequencer.changed.clone().notified_owned()
            };
            notified.await;
        }
    }
}

#[derive(Default)]
pub(crate) struct ArrivalSequencer {
    next: Mutex<u64>,
    changed: Arc<Notify>,
}

impl ArrivalSequencer {
    pub(crate) fn new() -> Self {
        Self {
            next: Mutex::new(1),
            changed: Arc::new(Notify::new()),
        }
    }
}

pub(crate) struct ArrivalTurn(Arc<ArrivalSequencer>);

impl Drop for ArrivalTurn {
    fn drop(&mut self) {
        *self.0.next.lock().expect("unable to lock RPC sequencer") += 1;
        self.0.changed.notify_waiters();
    }
}

impl fmt::Debug for RPCContext {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("RPCContext")
            .field("local_port", &self.local_port)
            .field("client_addr", &self.client_addr)
            .field("auth", &self.auth)
            .finish()
    }
}

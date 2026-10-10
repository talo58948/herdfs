use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use tokio::sync::{mpsc, Notify};

use crate::transaction_tracker::TransactionTracker;
use crate::vfs::NFSFileSystem;

#[derive(Clone)]
pub struct RPCContext {
    pub local_port: u16,
    pub client_addr: IpAddr,
    pub client_port: u16,
    pub owner_uid: u32,
    pub auth: crate::rpc::auth_unix,
    pub vfs: Arc<dyn NFSFileSystem + Send + Sync>,
    pub mount_signal: Option<mpsc::Sender<bool>>,
    pub export_name: Arc<String>,
    pub transaction_tracker: Arc<TransactionTracker>,
    pub handle_sequences: Arc<HandleSequencer>,
}

#[derive(Default)]
pub(crate) struct HandleSequencer(Mutex<HashMap<Vec<u8>, Arc<Completion>>>);

impl HandleSequencer {
    pub(crate) fn ticket(self: &Arc<Self>, handle: Vec<u8>) -> HandleTicket {
        let current = Arc::new(Completion::default());
        let previous = self
            .0
            .lock()
            .expect("unable to lock handle sequencer")
            .insert(handle.clone(), current.clone());
        HandleTicket {
            sequencer: self.clone(),
            handle,
            previous,
            current,
        }
    }
}

#[derive(Default)]
struct Completion {
    done: AtomicBool,
    notify: Notify,
}

impl Completion {
    async fn wait(&self) {
        while !self.done.load(Ordering::Acquire) {
            let notified = self.notify.notified();
            if self.done.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    fn finish(&self) {
        self.done.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
}

pub(crate) struct HandleTicket {
    sequencer: Arc<HandleSequencer>,
    handle: Vec<u8>,
    previous: Option<Arc<Completion>>,
    current: Arc<Completion>,
}

impl HandleTicket {
    pub(crate) async fn wait(&self) {
        if let Some(previous) = &self.previous {
            previous.wait().await;
        }
    }
}

impl Drop for HandleTicket {
    fn drop(&mut self) {
        self.current.finish();
        let mut sequences = self
            .sequencer
            .0
            .lock()
            .expect("unable to lock handle sequencer");
        if sequences
            .get(&self.handle)
            .is_some_and(|last| Arc::ptr_eq(last, &self.current))
        {
            sequences.remove(&self.handle);
        }
    }
}

impl fmt::Debug for RPCContext {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("RPCContext")
            .field("local_port", &self.local_port)
            .field("client_addr", &self.client_addr)
            .field("client_port", &self.client_port)
            .field("auth", &self.auth)
            .finish()
    }
}

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use serde::Serialize;

use super::{AccessPlan, Decided};
use crate::lock;

pub(crate) struct AccessRecord {
    decided: Mutex<Decided>,
    pub bytes_fetched: AtomicU64,
}

impl AccessRecord {
    pub(crate) fn new(decided: Decided) -> Self {
        Self {
            decided: Mutex::new(decided),
            bytes_fetched: AtomicU64::new(0),
        }
    }

    pub(crate) fn plan(&self) -> AccessPlan {
        lock(&self.decided).plan
    }

    pub(crate) fn decided(&self) -> Decided {
        *lock(&self.decided)
    }

    pub(crate) fn update(&self, decided: Decided) {
        *lock(&self.decided) = decided;
    }

    pub(crate) fn json(&self) -> serde_json::Result<Vec<u8>> {
        #[derive(Serialize)]
        struct Record {
            #[serde(flatten)]
            decided: Decided,
            bytes_fetched: u64,
        }

        serde_json::to_vec(&Record {
            decided: self.decided(),
            bytes_fetched: self.bytes_fetched.load(Relaxed),
        })
    }
}

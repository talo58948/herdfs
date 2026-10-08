//! Durable identity used by a loopback NFS export across client restarts.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use herdfs_core::proto::EntryId;
use serde::{Deserialize, Serialize};

use crate::lock;

#[derive(Deserialize, Serialize)]
struct State {
    export_id: [u8; 16],
    port: u16,
    creates: Vec<Create>,
}

#[derive(Clone, Deserialize, Serialize)]
struct Create {
    parent: EntryId,
    name: String,
    verifier: [u8; 8],
    entry: EntryId,
    created: u64,
}

const CREATE_WINDOW: Duration = Duration::from_secs(10 * 60);

pub(crate) struct Identity {
    path: PathBuf,
    state: Mutex<State>,
}

impl Identity {
    pub(crate) fn open(journal: &Path) -> io::Result<Self> {
        let path = journal.with_extension("nfs.json");
        let (mut state, created) = match fs::read(&path) {
            Ok(bytes) => (serde_json::from_slice(&bytes)?, false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (
                State {
                    export_id: rand::random(),
                    port: 0,
                    creates: Vec::new(),
                },
                true,
            ),
            Err(error) => return Err(error),
        };
        let before = state.creates.len();
        state.creates.retain(|create| create.created >= create_cutoff());
        let pruned = state.creates.len() != before;
        let identity = Self {
            path,
            state: Mutex::new(state),
        };
        if created || pruned {
            identity.persist()?;
        }
        Ok(identity)
    }

    pub(crate) fn export_id(&self) -> [u8; 16] {
        lock(&self.state).export_id
    }

    pub(crate) fn port(&self) -> u16 {
        lock(&self.state).port
    }

    pub(crate) fn set_port(&self, port: u16) -> io::Result<()> {
        lock(&self.state).port = port;
        self.persist()
    }

    pub(crate) fn replay(&self, parent: EntryId, name: &str, verifier: [u8; 8]) -> Option<EntryId> {
        lock(&self.state)
            .creates
            .iter()
            .rev()
            .find(|create| {
                create.created >= create_cutoff()
                    && create.parent == parent
                    && create.name == name
                    && create.verifier == verifier
            })
            .map(|create| create.entry)
    }

    pub(crate) fn remember(
        &self,
        parent: EntryId,
        name: String,
        verifier: [u8; 8],
        entry: EntryId,
    ) -> io::Result<()> {
        let mut state = lock(&self.state);
        state.creates.retain(|create| create.created >= create_cutoff());
        state.creates.push(Create {
            parent,
            name,
            verifier,
            entry,
            created: unix_seconds(),
        });
        drop(state);
        self.persist()
    }

    fn persist(&self) -> io::Result<()> {
        let temp = self.path.with_extension("json.tmp");
        let bytes = serde_json::to_vec(&*lock(&self.state))?;
        let mut file = File::create(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(temp, &self.path)?;
        File::open(self.path.parent().unwrap_or(Path::new(".")))?.sync_all()
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn create_cutoff() -> u64 {
    unix_seconds().saturating_sub(CREATE_WINDOW.as_secs())
}

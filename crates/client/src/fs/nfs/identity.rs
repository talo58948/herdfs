//! Durable identity used by a loopback NFS export across client restarts.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
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
        parent(&path)?;
        let (mut state, created, exposed) = match fs::symlink_metadata(&path) {
            Ok(metadata)
                if metadata.file_type().is_symlink()
                    || metadata.mode() & 0o077 != 0
                    || metadata.uid() != rustix::process::geteuid().as_raw() =>
            {
                (State::new(), false, true)
            }
            Ok(_) => {
                let mut file = File::open(&path)?;
                file.set_permissions(fs::Permissions::from_mode(0o600))?;
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)?;
                (serde_json::from_slice(&bytes)?, false, false)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => (State::new(), true, false),
            Err(error) => return Err(error),
        };
        let before = state.creates.len();
        state.creates.retain(|create| create.created >= create_cutoff());
        let pruned = state.creates.len() != before;
        let identity = Self {
            path,
            state: Mutex::new(state),
        };
        if created || exposed || pruned {
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
        let mut state = lock(&self.state);
        state.port = port;
        self.persist_state(&state)
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
        self.persist_state(&state)
    }

    fn persist(&self) -> io::Result<()> {
        self.persist_state(&lock(&self.state))
    }

    fn persist_state(&self, state: &State) -> io::Result<()> {
        let parent = parent(&self.path)?;
        let mut temp = tempfile::Builder::new()
            .prefix(".nfs-identity.")
            .suffix(".tmp")
            .permissions(fs::Permissions::from_mode(0o600))
            .tempfile_in(parent)?;
        temp.write_all(&serde_json::to_vec(state)?)?;
        temp.as_file().sync_all()?;
        temp.persist(&self.path).map_err(|error| error.error)?;
        File::open(parent)?.sync_all()
    }
}

impl State {
    fn new() -> Self {
        Self {
            export_id: rand::random(),
            port: 0,
            creates: Vec::new(),
        }
    }
}

fn parent(path: &Path) -> io::Result<&Path> {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "NFS identity needs a parent directory",
            )
        })
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

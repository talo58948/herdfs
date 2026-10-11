use std::sync::Arc;

use fuser::Notifier;
use tokio::runtime::Handle as Runtime;

use super::directory::Directories;
use crate::Result;
use crate::fs::Space;
use crate::fs::answer::{self, Answer};

pub struct Fuse {
    pub(super) space: Arc<Space>,
    pub(super) runtime: Runtime,
    pub(super) directories: Directories,
}

impl Fuse {
    pub fn new(space: Arc<Space>, runtime: Runtime) -> Self {
        Self {
            space,
            runtime,
            directories: Directories::default(),
        }
    }

    pub fn mounted(space: &Space, notifier: Notifier) {
        answer::mounted(space, notifier);
    }

    pub(super) fn space(&self) -> Arc<Space> {
        self.space.clone()
    }

    /// Runs `work` on the runtime and answers with its value or its error's errno.
    pub(super) fn spawn<T>(
        &self,
        reply: impl Answer<T> + Send + 'static,
        work: impl Future<Output = Result<T>> + Send + 'static,
    ) {
        self.runtime.spawn(async move {
            answer::reply(reply, work.await);
        });
    }
}

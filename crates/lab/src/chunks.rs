//! What a cold read must fetch: the chunks of a version that its byte ranges overlap (C9).

use std::collections::HashMap;
use std::ops::Range;

use anyhow::{Result, bail};
use herdfs_core::proto::{Request, Response, Version};
use herdfs_core::{ChunkRef, SpacePath};

use crate::{Lab, SPACE};

impl Lab {
    /// The head version of `path` in [`SPACE`], with its chunk list, as `user` opens it.
    pub fn head(&self, user: &str, path: &str) -> Result<Version> {
        let path = SpacePath::try_from(path)?;
        let open = Request::Open {
            space: SPACE.into(),
            path: path.clone(),
        };
        match self.rpc(user, &open)? {
            Response::Opened {
                version: Some(version),
                ..
            } => Ok(version),
            reply => bail!("{path} has no head version: {reply:?}"),
        }
    }
}

/// The bytes in the distinct chunks that overlap any of `ranges`: what serving those ranges from a
/// cold cache fetches, before any prefetch (C9). A chunk that recurs in the file counts once.
pub fn overlapping_chunk_bytes(chunks: &[ChunkRef], ranges: &[Range<u64>]) -> u64 {
    let mut touched = HashMap::new();
    let mut start = 0;
    for chunk in chunks {
        let end = start + u64::from(chunk.len);
        if ranges
            .iter()
            .any(|range| !range.is_empty() && range.start < end && start < range.end)
        {
            touched.insert(chunk.id, u64::from(chunk.len));
        }
        start = end;
    }
    touched.values().sum()
}

#[cfg(test)]
mod tests {
    use herdfs_core::ChunkId;

    use super::*;

    fn chunk(len: u32, content: &[u8]) -> ChunkRef {
        ChunkRef {
            id: ChunkId::of(content),
            len,
        }
    }

    #[test]
    fn counts_each_overlapped_chunk_once() {
        // Chunk ends: 10, 30, 35, 45; the first and the last hold the same bytes.
        let chunks = [chunk(10, b"x"), chunk(20, b"y"), chunk(5, b"z"), chunk(10, b"x")];
        assert_eq!(overlapping_chunk_bytes(&chunks, &[0..1, 2..10]), 10);
        assert_eq!(overlapping_chunk_bytes(&chunks, &[9..11, 11..12]), 30);
        assert_eq!(overlapping_chunk_bytes(&chunks, &[30..31, 12..13]), 25);
        assert_eq!(overlapping_chunk_bytes(&chunks, &[0..1, 44..45]), 10);
        assert_eq!(overlapping_chunk_bytes(&chunks, &[5..5, 45..90]), 0);
    }
}

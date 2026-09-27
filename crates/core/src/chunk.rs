//! Content-defined chunking and chunk identity.

use std::{fmt, io, str::FromStr};

use fastcdc::v2020::StreamCDC;
use serde::{Deserialize, Serialize};

pub const MIN_CHUNK: usize = 256 * 1024;
pub const AVG_CHUNK: usize = 1024 * 1024;
pub const MAX_CHUNK: usize = 4 * 1024 * 1024;

/// The BLAKE3 hash of a chunk's bytes. Chunks are stored under this id.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ChunkId([u8; 32]);

#[derive(Debug, thiserror::Error)]
#[error("invalid chunk id {0:?}")]
pub struct InvalidChunkId(String);

impl ChunkId {
    pub fn of(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }
}

impl fmt::Display for ChunkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&blake3::Hash::from_bytes(self.0).to_hex())
    }
}

impl fmt::Debug for ChunkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ChunkId({self})")
    }
}

impl FromStr for ChunkId {
    type Err = InvalidChunkId;

    fn from_str(hex: &str) -> Result<Self, InvalidChunkId> {
        blake3::Hash::from_hex(hex)
            .map(|hash| Self(*hash.as_bytes()))
            .map_err(|_| InvalidChunkId(hex.to_owned()))
    }
}

impl TryFrom<String> for ChunkId {
    type Error = InvalidChunkId;

    fn try_from(hex: String) -> Result<Self, InvalidChunkId> {
        hex.parse()
    }
}

impl From<ChunkId> for String {
    fn from(id: ChunkId) -> Self {
        id.to_string()
    }
}

/// One chunk of a file version, in file order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkRef {
    pub id: ChunkId,
    pub len: u32,
}

pub fn total_len(chunks: &[ChunkRef]) -> u64 {
    chunks.iter().map(|chunk| u64::from(chunk.len)).sum()
}

/// Splits `source` into content-defined chunks, yielding each chunk's
/// reference together with its bytes.
pub fn split(source: impl io::Read) -> impl Iterator<Item = io::Result<(ChunkRef, Vec<u8>)>> {
    StreamCDC::new(source, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK).map(|chunk| {
        let data = chunk.map_err(io::Error::other)?.data;
        let len = u32::try_from(data.len()).map_err(io::Error::other)?;
        Ok((
            ChunkRef {
                id: ChunkId::of(&data),
                len,
            },
            data,
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;
        (0..len)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 56) as u8
            })
            .collect()
    }

    fn ids(data: &[u8]) -> Vec<ChunkId> {
        split(data).map(|chunk| chunk.unwrap().0.id).collect()
    }

    #[test]
    fn chunks_reassemble_to_the_input() {
        let data = pseudo_random(9 * 1024 * 1024, 1);
        let chunks: Vec<_> = split(&data[..]).map(Result::unwrap).collect();
        assert!(chunks.len() > 2);
        assert!(
            chunks
                .iter()
                .all(|(r, bytes)| r.len as usize <= MAX_CHUNK && r.id == ChunkId::of(bytes))
        );
        let joined: Vec<u8> = chunks.into_iter().flat_map(|(_, bytes)| bytes).collect();
        assert_eq!(joined, data);
    }

    #[test]
    fn a_small_insert_keeps_most_chunks() {
        let data = pseudo_random(16 * 1024 * 1024, 2);
        let mut edited = data.clone();
        edited.splice(5_000_000..5_000_000, *b"inserted");
        let before = ids(&data);
        let after = ids(&edited);
        let reused = after.iter().filter(|id| before.contains(id)).count();
        assert!(reused + 2 >= after.len(), "reused {reused} of {}", after.len());
    }

    #[test]
    fn ids_round_trip_through_hex() {
        let id = ChunkId::of(b"hello");
        assert_eq!(id.to_string().parse::<ChunkId>().unwrap(), id);
        assert!("zz".parse::<ChunkId>().is_err());
    }
}

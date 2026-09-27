//! Types shared by the herdfs authority and its clients.

pub mod chunk;
pub mod path;
pub mod proto;

pub use chunk::{ChunkId, ChunkRef};
pub use path::SpacePath;

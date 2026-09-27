//! Where each chunk of a version lies in the file.

use std::ops::Range;

use herdfs_core::ChunkRef;

/// The cumulative end offset of each chunk, searched by bisection.
pub struct ChunkMap {
    ends: Vec<u64>,
}

impl ChunkMap {
    pub fn new(chunks: &[ChunkRef]) -> Self {
        let ends = chunks
            .iter()
            .scan(0, |end, chunk| {
                *end += u64::from(chunk.len);
                Some(*end)
            })
            .collect();
        Self { ends }
    }

    pub fn size(&self) -> u64 {
        self.ends.last().copied().unwrap_or(0)
    }

    /// Indices of the chunks that overlap `[off, off + len)`.
    pub fn overlapping(&self, off: u64, len: u64) -> Range<usize> {
        let end = off.saturating_add(len).min(self.size());
        if off >= end {
            return 0..0;
        }
        let first = self.ends.partition_point(|&chunk_end| chunk_end <= off);
        let last = self.ends.partition_point(|&chunk_end| chunk_end < end);
        first..last + 1
    }

    /// The part of chunk `index` inside `[off, off + len)`, as offsets within the chunk.
    pub fn within(&self, index: usize, off: u64, len: u64) -> Range<usize> {
        let start = index.checked_sub(1).map_or(0, |previous| self.ends[previous]);
        let from = off.clamp(start, self.ends[index]);
        let to = off.saturating_add(len).clamp(from, self.ends[index]);
        (from - start) as usize..(to - start) as usize
    }
}

#[cfg(test)]
mod tests {
    use herdfs_core::ChunkId;

    use super::*;

    fn map(lens: &[u32]) -> ChunkMap {
        let chunks: Vec<_> = lens
            .iter()
            .map(|&len| ChunkRef {
                id: ChunkId::of(&len.to_le_bytes()),
                len,
            })
            .collect();
        ChunkMap::new(&chunks)
    }

    // contract: C9
    #[test]
    fn overlapping_is_exactly_the_touched_chunks() {
        let map = map(&[10, 10, 10]);
        assert_eq!(map.overlapping(0, 10), 0..1);
        assert_eq!(map.overlapping(5, 10), 0..2);
        assert_eq!(map.overlapping(10, 10), 1..2);
        assert_eq!(map.overlapping(19, 2), 1..3);
        assert_eq!(map.overlapping(25, 100), 2..3);
        assert_eq!(map.overlapping(30, 1), 0..0);
        assert_eq!(map.overlapping(3, 0), 0..0);
        assert_eq!(ChunkMap::new(&[]).overlapping(0, 1), 0..0);
    }

    // contract: C9
    #[test]
    fn within_gives_offsets_inside_each_chunk() {
        let map = map(&[10, 20, 5]);
        assert_eq!(map.within(0, 4, 100), 4..10);
        assert_eq!(map.within(1, 4, 100), 0..20);
        assert_eq!(map.within(1, 12, 3), 2..5);
        assert_eq!(map.within(2, 0, 33), 0..3);
    }
}

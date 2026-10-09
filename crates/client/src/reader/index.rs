//! Bounded format probing for index prefetch. Ranges become chunk indices before download, so
//! every byte still uses the ordinary grant, verification and cache path (C9).

use std::ops::Range;
use std::sync::atomic::AtomicU64;

use crate::Result;
use crate::strategy::{Prefetch, ReadAhead};
use crate::version::OpenVersion;

use super::Reader;

/// The first-window prefetch and small read-ahead are two chunks (about 2 MiB at the average
/// chunk size): enough to hide one round trip without dominating a short access.
const FIRST_CHUNKS: usize = 2;
const SMALL_CHUNKS: usize = 2;
/// Large read-ahead is eight chunks (about 8 MiB): one download-concurrency batch.
const LARGE_CHUNKS: usize = 8;
/// Larger format-declared indexes fall back to the bounded edge window.
const MAX_INDEX_REGION: u64 = 8 * 1024 * 1024;

/// ZIP's 22-byte EOCD plus its maximum 65,535-byte comment.
const ZIP_TAIL: u64 = 65_557;
/// Enough for an MP4 box's 64-bit `largesize` header.
const BOX_HEADER: u64 = 16;
/// Bounded box walking finds the common early `moov` without scanning the file.
const BOX_PROBES: usize = 4;

impl Reader {
    pub async fn prefetch(&self, file: &OpenVersion, plan: Prefetch, fetched: &AtomicU64) -> Result<()> {
        let chunks = file.version.chunks.len();
        match plan {
            Prefetch::None => Ok(()),
            Prefetch::First => {
                self.prefetch_indices(file, 0..chunks.min(FIRST_CHUNKS), fetched)
                    .await
            }
            Prefetch::Whole => self.prefetch_indices(file, 0..chunks, fetched).await,
            Prefetch::Index => self.prefetch_index(file, fetched).await,
        }
    }

    pub async fn read_ahead(
        &self,
        file: &OpenVersion,
        after: u64,
        read_ahead: ReadAhead,
        fetched: &AtomicU64,
    ) -> Result<()> {
        let chunks = match read_ahead {
            ReadAhead::None => return Ok(()),
            ReadAhead::Small => SMALL_CHUNKS,
            ReadAhead::Large => LARGE_CHUNKS,
        };
        let first = file.overlapping(after.saturating_sub(1), 1).end;
        let end = first.saturating_add(chunks).min(file.version.chunks.len());
        self.prefetch_indices(file, first..end, fetched).await
    }

    async fn prefetch_index(&self, file: &OpenVersion, fetched: &AtomicU64) -> Result<()> {
        let size = file.version.size;
        if size == 0 {
            return Ok(());
        }
        let first = self.read(file, 0, BOX_HEADER.min(size), Some(fetched)).await?;
        let parquet_file = first.starts_with(b"PAR1");
        if mp4_header(&first)
            && let Some(range) = self.mp4(file, first, fetched).await?
        {
            return self.prefetch_range(file, range, fetched).await;
        }
        let tail_start = size.saturating_sub(ZIP_TAIL);
        let tail = self
            .read(file, tail_start, size - tail_start, Some(fetched))
            .await?;
        if parquet_file {
            return match parquet(size, &tail) {
                Some(range) => self.prefetch_range(file, range, fetched).await,
                None => self.prefetch_edges(file, fetched).await,
            };
        }
        if let Some(range) = zip(size, tail_start, &tail) {
            let signature = self.read(file, range.start, 4, Some(fetched)).await?;
            if signature == b"PK\x01\x02" {
                return self.prefetch_range(file, range, fetched).await;
            }
        }
        self.prefetch_edges(file, fetched).await
    }

    async fn mp4(
        &self,
        file: &OpenVersion,
        first: Vec<u8>,
        fetched: &AtomicU64,
    ) -> Result<Option<Range<u64>>> {
        let (mut offset, size, mut header) = (0, file.version.size, first);
        for remaining in (0..BOX_PROBES).rev() {
            let Some((kind, range)) = mp4_box(offset, size, &header) else {
                return Ok(None);
            };
            if kind == *b"moov" {
                return Ok(Some(range));
            }
            offset = range.end;
            if offset >= size || remaining == 0 {
                return Ok(None);
            }
            header = self
                .read(file, offset, BOX_HEADER.min(size - offset), Some(fetched))
                .await?;
        }
        Ok(None)
    }

    async fn prefetch_range(&self, file: &OpenVersion, range: Range<u64>, fetched: &AtomicU64) -> Result<()> {
        self.prefetch_indices(
            file,
            file.overlapping(range.start, range.end - range.start),
            fetched,
        )
        .await
    }

    async fn prefetch_edges(&self, file: &OpenVersion, fetched: &AtomicU64) -> Result<()> {
        let chunks = file.version.chunks.len();
        let last = chunks.saturating_sub(FIRST_CHUNKS);
        let indices = (0..chunks.min(FIRST_CHUNKS)).chain(last..chunks);
        self.prefetch_indices(file, indices, fetched).await
    }
}

fn parquet(size: u64, tail: &[u8]) -> Option<Range<u64>> {
    let suffix = tail.get(tail.len().checked_sub(8)?..)?;
    if suffix.get(4..)? != b"PAR1" {
        return None;
    }
    let len = u32::from_le_bytes(suffix.get(..4)?.try_into().ok()?);
    let total = u64::from(len).checked_add(8)?;
    if total > MAX_INDEX_REGION {
        return None;
    }
    let start = size.checked_sub(total)?;
    Some(start..size)
}

fn zip(size: u64, tail_start: u64, tail: &[u8]) -> Option<Range<u64>> {
    let eocd = tail.windows(4).rposition(|bytes| bytes == b"PK\x05\x06")?;
    let record = tail.get(eocd..eocd + 22)?;
    let central_len = u32::from_le_bytes(record.get(12..16)?.try_into().ok()?) as u64;
    let central_start = u32::from_le_bytes(record.get(16..20)?.try_into().ok()?) as u64;
    let central_end = central_start.checked_add(central_len)?;
    let eocd_start = tail_start + eocd as u64;
    let index_len = size.checked_sub(central_start)?;
    (central_len >= 4
        && index_len <= MAX_INDEX_REGION
        && central_end <= eocd_start
        && eocd_start < size)
        .then_some(central_start..size)
}

fn mp4_header(header: &[u8]) -> bool {
    header.get(4..8) == Some(b"ftyp")
}

fn mp4_box(offset: u64, file_size: u64, header: &[u8]) -> Option<([u8; 4], Range<u64>)> {
    let kind = header.get(4..8)?.try_into().ok()?;
    let short = u32::from_be_bytes(header.get(..4)?.try_into().ok()?);
    let len = match short {
        0 => file_size.checked_sub(offset)?,
        1 => u64::from_be_bytes(header.get(8..16)?.try_into().ok()?),
        len => u64::from(len),
    };
    let header_len = if short == 1 { 16 } else { 8 };
    let end = offset.checked_add(len)?;
    (len >= header_len && len <= MAX_INDEX_REGION && end <= file_size)
        .then_some((kind, offset..end))
}

#[cfg(test)]
mod tests {
    use super::*;

    // contract: P3, C9
    #[test]
    fn parquet_footer_is_the_index() {
        let file = [b"PAR1data".as_slice(), b"meta", &4u32.to_le_bytes(), b"PAR1"].concat();
        assert_eq!(parquet(file.len() as u64, &file), Some(8..20));
    }

    // contract: P3, C9
    #[test]
    fn zip_central_directory_through_eocd_is_the_index() {
        let mut file = b"payloadPK\x01\x02RAL".to_vec();
        let central = 7u32;
        let len = 7u32;
        file.extend_from_slice(b"PK\x05\x06\0\0\0\0\0\0\0\0");
        file.extend_from_slice(&len.to_le_bytes());
        file.extend_from_slice(&central.to_le_bytes());
        file.extend_from_slice(&[0, 0]);
        assert_eq!(zip(file.len() as u64, 0, &file), Some(7..file.len() as u64));
    }

    // contract: P3, C9
    #[test]
    fn oversized_format_indexes_are_rejected() {
        let parquet_tail = [
            ((MAX_INDEX_REGION - 7) as u32).to_le_bytes().as_slice(),
            b"PAR1",
        ]
        .concat();
        assert_eq!(parquet(MAX_INDEX_REGION + 8, &parquet_tail), None);

        let mp4 = [
            ((MAX_INDEX_REGION + 1) as u32).to_be_bytes().as_slice(),
            b"moov",
        ]
        .concat();
        assert_eq!(mp4_box(0, MAX_INDEX_REGION + 1, &mp4), None);
    }

    // contract: P3, C9
    #[test]
    fn mp4_walk_finds_moov() {
        let file = [
            12u32.to_be_bytes().as_slice(),
            b"ftyp",
            b"test",
            16u32.to_be_bytes().as_slice(),
            b"moov",
            b"metadata",
        ]
        .concat();
        let (_, first) = mp4_box(0, file.len() as u64, &file).unwrap();
        let (kind, second) = mp4_box(first.end, file.len() as u64, &file[first.end as usize..]).unwrap();
        assert_eq!((&kind, second), (b"moov", 12..28));
    }

    // contract: P3, C9
    #[test]
    fn unexpected_bytes_match_no_format() {
        let file = b"not an indexed format";
        assert_eq!(parquet(file.len() as u64, file), None);
        assert_eq!(zip(file.len() as u64, 0, file), None);
        assert!(!mp4_header(file));
    }
}

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
        if let Some(directory) = zip(size, tail_start, &tail)
            && self.zip_directory(file, &directory, fetched).await?
        {
            return self.prefetch_range(file, directory.range, fetched).await;
        }
        self.prefetch_edges(file, fetched).await
    }

    async fn zip_directory(
        &self,
        file: &OpenVersion,
        directory: &ZipDirectory,
        fetched: &AtomicU64,
    ) -> Result<bool> {
        let mut offset = directory.range.start;
        for _ in 0..directory.entries {
            let Some(remaining) = directory.range.end.checked_sub(offset) else {
                return Ok(false);
            };
            if remaining < ZIP_ENTRY_HEADER {
                return Ok(false);
            }
            let header = self.read(file, offset, ZIP_ENTRY_HEADER, Some(fetched)).await?;
            let Some(len) = zip_entry_len(&header) else {
                return Ok(false);
            };
            let Some(end) = offset.checked_add(len) else {
                return Ok(false);
            };
            if end > directory.range.end {
                return Ok(false);
            }
            offset = end;
        }
        Ok(offset == directory.range.end)
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

const ZIP_ENTRY_HEADER: u64 = 46;

#[derive(Debug, PartialEq, Eq)]
struct ZipDirectory {
    range: Range<u64>,
    entries: u16,
}

fn zip(size: u64, tail_start: u64, tail: &[u8]) -> Option<ZipDirectory> {
    let eocd = tail.windows(4).rposition(|bytes| bytes == b"PK\x05\x06")?;
    let record = tail.get(eocd..eocd + 22)?;
    let disk = u16::from_le_bytes(record.get(4..6)?.try_into().ok()?);
    let central_disk = u16::from_le_bytes(record.get(6..8)?.try_into().ok()?);
    let disk_entries = u16::from_le_bytes(record.get(8..10)?.try_into().ok()?);
    let entries = u16::from_le_bytes(record.get(10..12)?.try_into().ok()?);
    let central_len = u32::from_le_bytes(record.get(12..16)?.try_into().ok()?) as u64;
    let central_start = u32::from_le_bytes(record.get(16..20)?.try_into().ok()?) as u64;
    let comment_len = u16::from_le_bytes(record.get(20..22)?.try_into().ok()?) as u64;
    let central_end = central_start.checked_add(central_len)?;
    let eocd_start = tail_start + eocd as u64;
    let eocd_end = eocd_start.checked_add(22)?.checked_add(comment_len)?;
    let headers_len = u64::from(entries).checked_mul(ZIP_ENTRY_HEADER)?;
    (disk == 0
        && central_disk == 0
        && disk_entries == entries
        && central_len >= headers_len
        && central_len <= MAX_INDEX_REGION
        && central_end == eocd_start
        && eocd_end == size)
        .then_some(ZipDirectory {
            range: central_start..central_end,
            entries,
        })
}

fn zip_entry_len(header: &[u8]) -> Option<u64> {
    if header.get(..4)? != b"PK\x01\x02" {
        return None;
    }
    let name = u16::from_le_bytes(header.get(28..30)?.try_into().ok()?);
    let extra = u16::from_le_bytes(header.get(30..32)?.try_into().ok()?);
    let comment = u16::from_le_bytes(header.get(32..34)?.try_into().ok()?);
    Some(ZIP_ENTRY_HEADER + u64::from(name) + u64::from(extra) + u64::from(comment))
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
    (len >= header_len && len <= MAX_INDEX_REGION && end <= file_size).then_some((kind, offset..end))
}

#[cfg(test)]
#[path = "index_tests.rs"]
mod tests;

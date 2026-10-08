//! Bounded format probing for index prefetch. Ranges become chunk indices before download, so
//! every byte still uses the ordinary grant, verification and cache path (C9).

use std::ops::Range;
use std::sync::atomic::AtomicU64;

use crate::Result;
use crate::strategy::Prefetch;
use crate::version::OpenVersion;

use super::Reader;

/// The first-window prefetch and small read-ahead are two chunks (about 2 MiB at the average
/// chunk size): enough to hide one round trip without dominating a short access.
pub const FIRST_CHUNKS: usize = 2;
pub const SMALL_CHUNKS: usize = 2;
/// Large read-ahead is eight chunks (about 8 MiB): one download-concurrency batch.
pub const LARGE_CHUNKS: usize = 8;

const ZIP_TAIL: u64 = 65_557;
const BOX_HEADER: u64 = 16;
const BOX_PROBES: usize = 4;

impl Reader {
    pub async fn prefetch(&self, file: &OpenVersion, plan: Prefetch, access: &AtomicU64) -> Result<()> {
        let chunks = file.version.chunks.len();
        match plan {
            Prefetch::None => Ok(()),
            Prefetch::First => {
                self.prefetch_indices(file, 0..chunks.min(FIRST_CHUNKS), access)
                    .await
            }
            Prefetch::Whole => self.prefetch_indices(file, 0..chunks, access).await,
            Prefetch::Index => self.prefetch_index(file, access).await,
        }
    }

    pub async fn read_ahead(
        &self,
        file: &OpenVersion,
        after: u64,
        chunks: usize,
        access: &AtomicU64,
    ) -> Result<()> {
        let first = file.overlapping(after.saturating_sub(1), 1).end;
        let end = first.saturating_add(chunks).min(file.version.chunks.len());
        self.prefetch_indices(file, first..end, access).await
    }

    async fn prefetch_index(&self, file: &OpenVersion, access: &AtomicU64) -> Result<()> {
        let size = file.version.size;
        if size == 0 {
            return Ok(());
        }
        let first = self.read(file, 0, BOX_HEADER.min(size), Some(access)).await?;
        if first.starts_with(b"PAR1") {
            let tail_start = size.saturating_sub(8);
            let tail = self
                .read(file, tail_start, size - tail_start, Some(access))
                .await?;
            return match parquet(size, &tail) {
                Some(range) => self.prefetch_range(file, range, access).await,
                None => self.prefetch_edges(file, access).await,
            };
        }
        if mp4_header(&first)
            && let Some(range) = self.mp4(file, first, access).await?
        {
            return self.prefetch_range(file, range, access).await;
        }
        let tail_start = size.saturating_sub(ZIP_TAIL);
        let tail = self
            .read(file, tail_start, size - tail_start, Some(access))
            .await?;
        if let Some(range) = zip(size, tail_start, &tail) {
            return self.prefetch_range(file, range, access).await;
        }
        self.prefetch_edges(file, access).await
    }

    async fn mp4(
        &self,
        file: &OpenVersion,
        first: Vec<u8>,
        access: &AtomicU64,
    ) -> Result<Option<Range<u64>>> {
        let (mut offset, size) = (0, file.version.size);
        for probe in 0..BOX_PROBES {
            let header = if probe == 0 {
                first.clone()
            } else {
                self.read(file, offset, BOX_HEADER.min(size - offset), Some(access))
                    .await?
            };
            let Some((kind, range)) = mp4_box(offset, size, &header) else {
                return Ok(None);
            };
            if kind == *b"moov" {
                return Ok(Some(range));
            }
            offset = range.end;
            if offset >= size {
                return Ok(None);
            }
        }
        Ok(None)
    }

    async fn prefetch_range(&self, file: &OpenVersion, range: Range<u64>, access: &AtomicU64) -> Result<()> {
        self.prefetch_indices(
            file,
            file.overlapping(range.start, range.end - range.start),
            access,
        )
        .await
    }

    async fn prefetch_edges(&self, file: &OpenVersion, access: &AtomicU64) -> Result<()> {
        let chunks = file.version.chunks.len();
        let last = chunks.saturating_sub(FIRST_CHUNKS);
        let indices = (0..chunks.min(FIRST_CHUNKS)).chain(last..chunks);
        self.prefetch_indices(file, indices, access).await
    }
}

fn parquet(size: u64, tail: &[u8]) -> Option<Range<u64>> {
    let suffix = tail.get(tail.len().checked_sub(8)?..)?;
    if suffix.get(4..)? != b"PAR1" {
        return None;
    }
    let len = u32::from_le_bytes(suffix.get(..4)?.try_into().ok()?);
    let start = size.checked_sub(u64::from(len) + 8)?;
    Some(start..size)
}

fn zip(size: u64, tail_start: u64, tail: &[u8]) -> Option<Range<u64>> {
    let eocd = tail.windows(4).rposition(|bytes| bytes == b"PK\x05\x06")?;
    let record = tail.get(eocd..eocd + 22)?;
    let central_len = u32::from_le_bytes(record.get(12..16)?.try_into().ok()?) as u64;
    let central_start = u32::from_le_bytes(record.get(16..20)?.try_into().ok()?) as u64;
    let central_end = central_start.checked_add(central_len)?;
    let eocd_start = tail_start + eocd as u64;
    (central_end <= eocd_start && eocd_start < size).then_some(central_start..size)
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
    (len >= header_len && end <= file_size).then_some((kind, offset..end))
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
        let mut file = b"payloadCENTRAL".to_vec();
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

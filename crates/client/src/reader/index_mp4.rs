use std::ops::Range;
use std::sync::atomic::AtomicU64;

use crate::Result;
use crate::version::OpenVersion;

use super::{BOX_HEADER, BOX_PROBES, FIRST_CHUNKS, MAX_INDEX_REGION, Reader};

impl Reader {
    pub(super) async fn mp4(
        &self,
        file: &OpenVersion,
        first: Vec<u8>,
        fetched: &AtomicU64,
    ) -> Result<Option<Range<u64>>> {
        let (mut offset, size, mut header) = (0, file.version.size, first);
        for remaining in (0..BOX_PROBES).rev() {
            let Some((kind, range, header_len)) = mp4_box(offset, size, &header) else {
                return Ok(None);
            };
            if kind == *b"moov" {
                if !self.valid_moov(file, &range, header_len, fetched).await? {
                    return Ok(None);
                }
                let indices = file.overlapping(range.start, range.end - range.start);
                return Ok(self.fits_index_fetch(file, indices, fetched).then_some(range));
            }
            offset = range.end;
            if offset >= size || remaining == 0 {
                return Ok(None);
            }
            let len = BOX_HEADER.min(size - offset);
            header = match self.cached_range(file, offset, len) {
                Some(header) => header,
                // A lone ftyp claim is not enough to widen an unknown file's fallback.
                None if range.start == 0 || !self.probe_fits(file, offset, len, fetched) => return Ok(None),
                None => self.read(file, offset, len, Some(fetched)).await?,
            };
        }
        Ok(None)
    }

    async fn valid_moov(
        &self,
        file: &OpenVersion,
        range: &Range<u64>,
        header_len: u64,
        fetched: &AtomicU64,
    ) -> Result<bool> {
        let Some(mut offset) = range.start.checked_add(header_len) else {
            return Ok(false);
        };
        let mut has_movie_header = false;
        for _ in 0..BOX_PROBES {
            if offset == range.end {
                return Ok(has_movie_header);
            }
            let len = BOX_HEADER.min(range.end - offset);
            if len < 8 {
                return Ok(false);
            }
            let header = match self.cached_range(file, offset, len) {
                Some(header) => header,
                None if !self.probe_fits(file, offset, len, fetched) => return Ok(false),
                None => self.read(file, offset, len, Some(fetched)).await?,
            };
            let Some((kind, child, _)) = mp4_box(offset, range.end, &header) else {
                return Ok(false);
            };
            has_movie_header |= kind == *b"mvhd";
            offset = child.end;
        }
        Ok(offset == range.end && has_movie_header)
    }

    fn probe_fits(&self, file: &OpenVersion, offset: u64, len: u64, fetched: &AtomicU64) -> bool {
        let chunks = file.version.chunks.len();
        let last = chunks.saturating_sub(FIRST_CHUNKS);
        let probe = file.overlapping(offset, len);
        let fallback = (0..chunks.min(FIRST_CHUNKS)).chain(last..chunks);
        self.fits_index_fetch(file, probe.chain(fallback), fetched)
    }
}

pub(super) fn mp4_header(header: &[u8], size: u64) -> bool {
    mp4_box(0, size, header).is_some_and(|(kind, range, _)| kind == *b"ftyp" && range.end >= 16)
}

pub(super) fn mp4_box(offset: u64, parent_end: u64, header: &[u8]) -> Option<([u8; 4], Range<u64>, u64)> {
    let kind = header.get(4..8)?.try_into().ok()?;
    let short = u32::from_be_bytes(header.get(..4)?.try_into().ok()?);
    let len = match short {
        0 => parent_end.checked_sub(offset)?,
        1 => u64::from_be_bytes(header.get(8..16)?.try_into().ok()?),
        len => u64::from(len),
    };
    let header_len = if short == 1 { 16 } else { 8 };
    let end = offset.checked_add(len)?;
    (len >= header_len && (kind != *b"moov" || len <= MAX_INDEX_REGION) && end <= parent_end).then_some((
        kind,
        offset..end,
        header_len,
    ))
}

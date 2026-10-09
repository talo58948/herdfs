//! Minimal compact-Thrift validation for Parquet FileMetaData.

const STOP: u8 = 0;
const BOOL_TRUE: u8 = 1;
const BOOL_FALSE: u8 = 2;
const BYTE: u8 = 3;
const I16: u8 = 4;
const I32: u8 = 5;
const I64: u8 = 6;
const DOUBLE: u8 = 7;
const BINARY: u8 = 8;
const LIST: u8 = 9;
const SET: u8 = 10;
const MAP: u8 = 11;
const STRUCT: u8 = 12;
const MAX_DEPTH: usize = 32;

pub(super) fn valid_metadata(bytes: &[u8]) -> bool {
    let mut input = Input::new(bytes);
    let Some(fields) = input.structure(0, true) else {
        return false;
    };
    fields & 0b1111 == 0b1111 && input.bytes.is_empty()
}

struct Input<'a> {
    bytes: &'a [u8],
}

impl<'a> Input<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    fn byte(&mut self) -> Option<u8> {
        let (&byte, rest) = self.bytes.split_first()?;
        self.bytes = rest;
        Some(byte)
    }

    fn take(&mut self, len: usize) -> Option<()> {
        self.bytes = self.bytes.get(len..)?;
        Some(())
    }

    fn varint(&mut self) -> Option<u64> {
        let mut value = 0_u64;
        for shift in (0..64).step_by(7) {
            let byte = self.byte()?;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }

    fn structure(&mut self, depth: usize, metadata: bool) -> Option<u8> {
        if depth > MAX_DEPTH {
            return None;
        }
        let (mut previous, mut required) = (0_i64, 0_u8);
        loop {
            let header = self.byte()?;
            let kind = header & 0x0f;
            if kind == STOP {
                return Some(required);
            }
            let delta = i64::from(header >> 4);
            let field = if delta == 0 {
                let encoded = self.varint()?;
                i64::try_from(encoded >> 1).ok()? ^ -i64::try_from(encoded & 1).ok()?
            } else {
                previous.checked_add(delta)?
            };
            if metadata && matches!((field, kind), (1, I32) | (2, LIST) | (3, I64) | (4, LIST)) {
                required |= 1 << (field - 1);
            }
            previous = field;
            self.value(kind, depth, false)?;
        }
    }

    fn value(&mut self, kind: u8, depth: usize, collection: bool) -> Option<()> {
        if depth > MAX_DEPTH {
            return None;
        }
        match kind {
            BOOL_TRUE | BOOL_FALSE if collection => self.take(1),
            BOOL_TRUE | BOOL_FALSE => Some(()),
            BYTE => self.take(1),
            I16 | I32 | I64 => {
                self.varint()?;
                Some(())
            }
            DOUBLE => self.take(8),
            BINARY => {
                let len = usize::try_from(self.varint()?).ok()?;
                self.take(len)
            }
            LIST | SET => self.list(depth),
            MAP => self.map(depth),
            STRUCT => {
                self.structure(depth + 1, false)?;
                Some(())
            }
            _ => None,
        }
    }

    fn list(&mut self, depth: usize) -> Option<()> {
        let header = self.byte()?;
        let mut len = usize::from(header >> 4);
        if len == 15 {
            len = usize::try_from(self.varint()?).ok()?;
        }
        let kind = header & 0x0f;
        if !(BOOL_TRUE..=STRUCT).contains(&kind) {
            return None;
        }
        (0..len).try_for_each(|_| self.value(kind, depth + 1, true))
    }

    fn map(&mut self, depth: usize) -> Option<()> {
        let len = usize::try_from(self.varint()?).ok()?;
        if len == 0 {
            return Some(());
        }
        let kinds = self.byte()?;
        (0..len).try_for_each(|_| {
            self.value(kinds >> 4, depth + 1, true)?;
            self.value(kinds & 0x0f, depth + 1, true)
        })
    }
}

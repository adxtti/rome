//! Bounds-checked little/big-endian byte reader.
//!
//! Every decoder reads untrusted game data, so nothing here panics on short
//! input: all reads return `Result`.

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ByteError {
    #[error("read of {len} bytes at offset {offset} past end of buffer ({size} bytes)")]
    OutOfBounds {
        offset: usize,
        len: usize,
        size: usize,
    },
    #[error("bad magic at offset {offset}: expected {expected:?}, found {found:?}")]
    BadMagic {
        offset: usize,
        expected: Vec<u8>,
        found: Vec<u8>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endian {
    Little,
    Big,
}

/// A cursor over a byte slice.
#[derive(Clone, Debug)]
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    endian: Endian,
}

macro_rules! read_int {
    ($name:ident, $at:ident, $ty:ty) => {
        pub fn $name(&mut self) -> Result<$ty, ByteError> {
            let v = self.$at(self.pos)?;
            self.pos += std::mem::size_of::<$ty>();
            Ok(v)
        }

        pub fn $at(&self, offset: usize) -> Result<$ty, ByteError> {
            const N: usize = std::mem::size_of::<$ty>();
            let b: [u8; N] = self
                .bytes_at(offset, N)?
                .try_into()
                .expect("length checked");
            Ok(match self.endian {
                Endian::Little => <$ty>::from_le_bytes(b),
                Endian::Big => <$ty>::from_be_bytes(b),
            })
        }
    };
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            endian: Endian::Little,
        }
    }

    pub fn with_endian(data: &'a [u8], endian: Endian) -> Self {
        Self {
            data,
            pos: 0,
            endian,
        }
    }

    pub fn set_endian(&mut self, endian: Endian) {
        self.endian = endian;
    }

    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    pub fn seek(&mut self, pos: usize) {
        self.pos = pos;
    }

    pub fn skip(&mut self, n: usize) {
        self.pos = self.pos.saturating_add(n);
    }

    pub fn bytes_at(&self, offset: usize, len: usize) -> Result<&'a [u8], ByteError> {
        let end = offset.checked_add(len).filter(|&e| e <= self.data.len());
        match end {
            Some(end) => Ok(&self.data[offset..end]),
            None => Err(ByteError::OutOfBounds {
                offset,
                len,
                size: self.data.len(),
            }),
        }
    }

    pub fn bytes(&mut self, len: usize) -> Result<&'a [u8], ByteError> {
        let b = self.bytes_at(self.pos, len)?;
        self.pos += len;
        Ok(b)
    }

    pub fn expect_magic(&mut self, magic: &[u8]) -> Result<(), ByteError> {
        let offset = self.pos;
        let found = self.bytes(magic.len())?;
        if found != magic {
            return Err(ByteError::BadMagic {
                offset,
                expected: magic.to_vec(),
                found: found.to_vec(),
            });
        }
        Ok(())
    }

    read_int!(u8, u8_at, u8);
    read_int!(u16, u16_at, u16);
    read_int!(u32, u32_at, u32);
    read_int!(u64, u64_at, u64);
    read_int!(i16, i16_at, i16);
    read_int!(i32, i32_at, i32);
    read_int!(i64, i64_at, i64);

    pub fn f32(&mut self) -> Result<f32, ByteError> {
        self.u32().map(f32::from_bits)
    }

    pub fn f32_at(&self, offset: usize) -> Result<f32, ByteError> {
        self.u32_at(offset).map(f32::from_bits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_bounds() {
        let data = [0x01, 0x02, 0x03, 0x04, 0x05];
        let mut r = Reader::new(&data);
        assert_eq!(r.u32().unwrap(), 0x0403_0201);
        assert!(r.u32().is_err());
        let r = Reader::with_endian(&data, Endian::Big);
        assert_eq!(r.u16_at(0).unwrap(), 0x0102);
        assert!(r.bytes_at(usize::MAX, 2).is_err());
    }
}
